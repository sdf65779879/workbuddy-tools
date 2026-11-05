//! App-level token renewal. Credentials are shared with the gateway and never written to
//! the official WorkBuddy session by this service.
use crate::config::{self, AccountConfig};
use crate::gateway::SharedConfig;
use base64::{engine::general_purpose, Engine};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const REFRESH_ENDPOINT: &str = "https://copilot.tencent.com/v2/plugin/auth/token/refresh";
const REFRESH_ENDPOINT_GLOBAL: &str = "https://www.workbuddy.ai/v2/plugin/auth/token/refresh";
const ADVANCE_MS: i64 = 10 * 60_000;
const UNKNOWN_EXPIRY_INTERVAL_MS: i64 = 6 * 3_600_000;

pub(crate) struct AccountRefresh<'a> {
    pending: &'a Mutex<HashSet<String>>,
    uid: &'a str,
}

impl<'a> AccountRefresh<'a> {
    pub(crate) fn start(pending: &'a Mutex<HashSet<String>>, uid: &'a str) -> Result<Self, String> {
        let mut guard = pending.lock().map_err(|_| "无法读取账号续期状态")?;
        if !guard.insert(uid.to_owned()) {
            return Err("这个账号正在续期，请稍后再试".into());
        }
        Ok(Self { pending, uid })
    }
}

impl Drop for AccountRefresh<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(self.uid);
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RenewalStatus {
    pub uid: String,
    pub in_progress: bool,
    pub last_attempt_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub next_attempt_at: Option<i64>,
    pub consecutive_failures: u32,
    pub error: String,
}

#[derive(Default)]
struct AccountRuntime {
    fingerprint: u64,
    status: RenewalStatus,
}

pub struct Service {
    cfg: SharedConfig,
    pending: Arc<Mutex<HashSet<String>>>,
    runtime: Mutex<HashMap<String, AccountRuntime>>,
    client: reqwest::Client,
    endpoint: String,
    config_path: PathBuf,
}

impl Service {
    pub fn new(cfg: SharedConfig, pending: Arc<Mutex<HashSet<String>>>) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            pending,
            runtime: Mutex::default(),
            client: reqwest::Client::new(),
            endpoint: REFRESH_ENDPOINT.into(),
            config_path: config::config_path(),
        })
    }

    pub fn start(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tauri::async_runtime::spawn(async move {
            loop {
                let Some(service) = weak.upgrade() else {
                    break;
                };
                service.tick().await;
                drop(service);
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }

    pub async fn statuses(&self) -> Vec<RenewalStatus> {
        let cfg = self.cfg.read().await;
        let mut runtime = self.runtime.lock().unwrap();
        let now = crate::reqlog::now_ms();
        cfg.accounts
            .iter()
            .map(|account| {
                let entry = runtime.entry(account.uid.clone()).or_default();
                synchronize_runtime(entry, account, now);
                let mut status = entry.status.clone();
                if !cfg.schedule.keepalive_enabled || !account.enabled || account.paused {
                    status.next_attempt_at = None;
                }
                if account.refresh_token.trim().is_empty() {
                    status.next_attempt_at = None;
                    status.error = "未配置 Refresh Token，请在账号保险柜补充续期凭证".into();
                }
                status
            })
            .collect()
    }

    async fn tick(&self) {
        let cfg = self.cfg.read().await.clone();
        if !cfg.schedule.keepalive_enabled {
            return;
        }
        let now = crate::reqlog::now_ms();
        let due = {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.retain(|uid, _| cfg.accounts.iter().any(|account| &account.uid == uid));
            let mut due = Vec::new();
            for account in &cfg.accounts {
                let entry = runtime.entry(account.uid.clone()).or_default();
                synchronize_runtime(entry, account, now);
                if account.enabled
                    && !account.paused
                    && !account.refresh_token.trim().is_empty()
                    && entry.status.next_attempt_at.is_some_and(|time| time <= now)
                {
                    due.push(account.uid.clone());
                }
            }
            due
        };
        futures_util::stream::iter(due)
            .for_each_concurrent(4, |uid| async move {
                let enabled = {
                    let cfg = self.cfg.read().await;
                    cfg.schedule.keepalive_enabled
                        && cfg
                            .accounts
                            .iter()
                            .any(|account| account.uid == uid && account.enabled && !account.paused)
                };
                if enabled {
                    let _ = self.renew_account(&uid).await;
                }
            })
            .await;
    }

    pub async fn renew_account(&self, uid: &str) -> Result<AccountConfig, String> {
        let _refresh = AccountRefresh::start(&self.pending, uid)?;
        let account = self
            .cfg
            .read()
            .await
            .accounts
            .iter()
            .find(|account| account.uid == uid)
            .cloned()
            .ok_or_else(|| "账号不存在".to_string())?;
        let now = crate::reqlog::now_ms();
        {
            let mut runtime = self.runtime.lock().unwrap();
            let entry = runtime.entry(uid.to_owned()).or_default();
            synchronize_runtime(entry, &account, now);
            entry.status.in_progress = true;
            entry.status.last_attempt_at = Some(now);
        }
        let result = self.refresh_and_save(&account).await;
        let now = crate::reqlog::now_ms();
        let mut runtime = self.runtime.lock().unwrap();
        let entry = runtime.entry(uid.to_owned()).or_default();
        entry.status.in_progress = false;
        match &result {
            Ok(updated) => {
                entry.fingerprint = fingerprint(updated);
                entry.status.last_success_at = Some(now);
                entry.status.consecutive_failures = 0;
                entry.status.error.clear();
                entry.status.next_attempt_at = Some(next_success_attempt(updated, now));
            }
            Err(error) => {
                entry.status.consecutive_failures =
                    entry.status.consecutive_failures.saturating_add(1);
                entry.status.error = error.clone();
                entry.status.next_attempt_at =
                    Some(now + retry_delay_ms(entry.status.consecutive_failures));
            }
        }
        result
    }

    async fn refresh_and_save(&self, account: &AccountConfig) -> Result<AccountConfig, String> {
        if account.refresh_token.trim().is_empty() {
            return Err("该账号没有 Refresh Token，请补充续期凭证".into());
        }
        let response = self
            .client
            .post(self.refresh_endpoint(&account.realm))
            .timeout(std::time::Duration::from_secs(30))
            .bearer_auth(account.token.trim())
            .header("X-Refresh-Token", account.refresh_token.trim())
            .header("X-Auth-Refresh-Source", "plugin")
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|_| "续期请求失败，将自动重试".to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "续期服务拒绝请求（HTTP {}），请检查续期凭证",
                response.status().as_u16()
            ));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| "续期服务返回了无效 JSON".to_string())?;
        let updated = parse_refreshed(account, &body, crate::reqlog::now_ms())?;
        let mut cfg = self.cfg.write().await;
        let mut next = cfg.clone();
        let current = next
            .accounts
            .iter_mut()
            .find(|current| current.uid == account.uid)
            .ok_or_else(|| "续期期间账号已被移除".to_string())?;
        if current.token != account.token
            || current.refresh_token != account.refresh_token
            || current.expires_at != account.expires_at
        {
            return Err("续期期间账号凭证已变更，请重试".into());
        }
        current.token.clone_from(&updated.token);
        current.refresh_token.clone_from(&updated.refresh_token);
        current.expires_at = updated.expires_at;
        let saved = current.clone();
        config::save_config_at(&self.config_path, &next)?;
        *cfg = next;
        Ok(saved)
    }

    fn refresh_endpoint(&self, realm: &str) -> &str {
        // A custom endpoint belongs to the isolated test service. Production
        // credentials stay on the same official realm as browser authorization.
        if self.endpoint == REFRESH_ENDPOINT && realm == "global" {
            REFRESH_ENDPOINT_GLOBAL
        } else {
            &self.endpoint
        }
    }
}

fn parse_refreshed(
    account: &AccountConfig,
    body: &Value,
    now: i64,
) -> Result<AccountConfig, String> {
    let code = body.get("code").and_then(integer);
    if !matches!(code, Some(0 | 200)) {
        return Err(match code {
            Some(code) => format!("续期服务返回业务错误（code={code}），请检查续期凭证"),
            None => "续期响应缺少有效业务状态码".into(),
        });
    }
    let data = body
        .get("data")
        .filter(|value| value.is_object())
        .ok_or_else(|| "续期响应缺少 data 字段".to_string())?;
    let access = data
        .get("accessToken")
        .or_else(|| data.get("access_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "续期响应缺少有效 Access Token".to_string())?;
    let refresh = data
        .get("refreshToken")
        .or_else(|| data.get("refresh_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .unwrap_or(&account.refresh_token);
    let explicit_expiry = data
        .get("expiresAt")
        .or_else(|| data.get("expires_at"))
        .and_then(integer)
        .and_then(normalize_epoch_ms);
    let lifetime = data
        .get("expiresIn")
        .or_else(|| data.get("expires_in"))
        .and_then(integer)
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|ms| now.checked_add(ms));
    let expires_at = explicit_expiry
        .or_else(|| jwt_expiry_ms(access))
        .or(lifetime);
    if expires_at.is_some_and(|expiry| expiry <= now) {
        return Err("续期服务返回的 Access Token 已过期".into());
    }
    let mut updated = account.clone();
    updated.token = access.into();
    updated.refresh_token = refresh.into();
    updated.expires_at = expires_at;
    Ok(updated)
}

fn integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

fn normalize_epoch_ms(value: i64) -> Option<i64> {
    if value <= 0 {
        None
    } else if value < 1_000_000_000_000 {
        value.checked_mul(1000)
    } else {
        Some(value)
    }
}

pub(crate) fn jwt_expiry_ms(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let decoded = general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    integer(claims.get("exp")?)
        .and_then(|seconds| seconds.checked_mul(1000))
        .filter(|value| *value > 0)
}

pub fn token_expiry_ms(account: &AccountConfig) -> Option<i64> {
    account
        .expires_at
        .and_then(normalize_epoch_ms)
        .or_else(|| jwt_expiry_ms(&account.token))
}

fn fingerprint(account: &AccountConfig) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    account.token.hash(&mut hash);
    account.refresh_token.hash(&mut hash);
    account.expires_at.hash(&mut hash);
    hash.finish()
}

fn synchronize_runtime(entry: &mut AccountRuntime, account: &AccountConfig, now: i64) {
    let version = fingerprint(account);
    if entry.status.uid != account.uid || entry.fingerprint != version {
        *entry = AccountRuntime {
            fingerprint: version,
            status: RenewalStatus {
                uid: account.uid.clone(),
                next_attempt_at: Some(
                    token_expiry_ms(account)
                        .map(|expiry| (expiry - ADVANCE_MS).max(now))
                        .unwrap_or(now),
                ),
                ..Default::default()
            },
        };
    }
}

fn next_success_attempt(account: &AccountConfig, now: i64) -> i64 {
    token_expiry_ms(account)
        .map(|expiry| (expiry - ADVANCE_MS).max(now + 30_000))
        .unwrap_or(now + UNKNOWN_EXPIRY_INTERVAL_MS)
}

fn retry_delay_ms(failures: u32) -> i64 {
    (60_000_i64 * (1_i64 << failures.saturating_sub(1).min(4))).min(15 * 60_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_account_renewal_stays_on_its_authorized_realm() {
        let service = Service::new(
            Arc::new(tokio::sync::RwLock::new(crate::config::AppConfig::default())),
            Arc::new(Mutex::default()),
        );
        assert_eq!(
            service.refresh_endpoint("cn"),
            "https://copilot.tencent.com/v2/plugin/auth/token/refresh"
        );
        assert_eq!(
            service.refresh_endpoint("global"),
            "https://www.workbuddy.ai/v2/plugin/auth/token/refresh"
        );
    }
    use crate::config::AppConfig;
    use axum::{
        http::{HeaderMap, StatusCode},
        routing::post,
        Json, Router,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn account() -> AccountConfig {
        serde_json::from_value(serde_json::json!({
            "uid":"test-account", "nickname":"Test", "token":"old-access", "refresh_token":"old-refresh",
            "expires_at":1, "realm":"cn", "enabled":true, "paused":false
        })).unwrap()
    }

    async fn service(
        app: Router,
        account: AccountConfig,
    ) -> (Arc<Service>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/refresh", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let cfg = Arc::new(tokio::sync::RwLock::new(AppConfig {
            accounts: vec![account],
            ..Default::default()
        }));
        let mut service = Service::new(cfg, Arc::new(Mutex::default()));
        let mutable = Arc::get_mut(&mut service).unwrap();
        mutable.endpoint = endpoint;
        mutable.config_path = std::env::temp_dir().join(format!(
            "workbuddy-renewal-test-{}.json",
            uuid::Uuid::new_v4()
        ));
        (service, server)
    }

    #[test]
    fn expiry_and_success_scheduling_use_new_observations() {
        let now = 1_800_000_000_000;
        let mut existing = account();
        existing.expires_at = Some(now / 1000);
        assert_eq!(token_expiry_ms(&existing), Some(now));
        existing.expires_at = Some(now);
        assert_eq!(token_expiry_ms(&existing), Some(now));
        let jwt = format!(
            "e30.{}.sig",
            general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{}}}"#, now / 1000 + 3600))
        );
        let updated = parse_refreshed(
            &existing,
            &serde_json::json!({"code":0,"data":{"accessToken":jwt,"refreshToken":""}}),
            now,
        )
        .unwrap();
        assert_eq!(updated.refresh_token, "old-refresh");
        assert_eq!(updated.expires_at, Some(now + 3_600_000));
        assert_eq!(next_success_attempt(&updated, now), now + 3_000_000);
        let opaque = parse_refreshed(
            &existing,
            &serde_json::json!({"code":0,"data":{"accessToken":"new-opaque"}}),
            now,
        )
        .unwrap();
        assert_eq!(
            opaque.expires_at, None,
            "an old expiry must not survive renewal"
        );
        assert_eq!(
            next_success_attempt(&opaque, now),
            now + UNKNOWN_EXPIRY_INTERVAL_MS
        );
        let lifetime = parse_refreshed(
            &existing,
            &serde_json::json!({"code":0,"data":{"access_token":"new","expires_in":3600}}),
            now,
        )
        .unwrap();
        assert_eq!(lifetime.expires_at, Some(now + 3_600_000));
    }

    #[test]
    fn malformed_or_expired_refresh_results_are_not_saved_as_success() {
        let existing = account();
        let now = 1_800_000_000_000;
        for body in [
            serde_json::json!({"code":10001,"msg":"expired","data":null}),
            serde_json::json!({"code":0,"data":{"accessToken":"   "}}),
            serde_json::json!({"code":0,"data":{"accessToken":"new","expiresAt":now-1000}}),
            serde_json::json!({"data":{"accessToken":"new"}}),
        ] {
            assert!(parse_refreshed(&existing, &body, now).is_err());
        }
        assert_eq!(retry_delay_ms(1), 60_000);
        assert_eq!(retry_delay_ms(2), 120_000);
        assert_eq!(retry_delay_ms(10), 900_000);
    }

    #[tokio::test]
    async fn background_tick_rotates_and_persists_gateway_credentials() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route("/refresh", post(move |headers:HeaderMap| {
            counter.fetch_add(1, Ordering::SeqCst);
            async move {
                assert_eq!(headers["authorization"], "Bearer old-access");
                assert_eq!(headers["x-refresh-token"], "old-refresh");
                Json(serde_json::json!({"code":0,"data":{"accessToken":"new-access","refreshToken":"new-refresh","expires_in":3600}}))
            }
        }));
        let (service, server) = service(app, account()).await;
        service.tick().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let cfg = service.cfg.read().await.clone();
        assert_eq!(cfg.accounts[0].token, "new-access");
        assert_eq!(cfg.accounts[0].refresh_token, "new-refresh");
        let disk: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(&service.config_path).unwrap()).unwrap();
        assert_eq!(disk.accounts[0].refresh_token, "new-refresh");
        assert_eq!(disk.accounts[0].token, "new-access");
        let status = service.statuses().await.remove(0);
        assert!(status.last_success_at.is_some());
        assert!(status.error.is_empty());
        service.tick().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "fresh token must not refresh every scan"
        );
        server.abort();
        std::fs::remove_file(&service.config_path).unwrap();
    }

    #[tokio::test]
    async fn failed_refresh_retries_after_short_backoff_and_preserves_credentials() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route("/refresh", post(move || {
            let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
            async move {
                if first { (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"code":10001}))) }
                else { (StatusCode::OK, Json(serde_json::json!({"code":0,"data":{"accessToken":"renewed","expires_in":3600}}))) }
            }
        }));
        let (service, server) = service(app, account()).await;
        service.tick().await;
        assert_eq!(service.cfg.read().await.accounts[0].token, "old-access");
        assert!(!service.config_path.exists());
        let failure = service.statuses().await.remove(0);
        assert_eq!(failure.consecutive_failures, 1);
        assert!(failure.error.contains("401"));
        assert!(failure.next_attempt_at.unwrap() <= crate::reqlog::now_ms() + 60_000);
        service.tick().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        service
            .runtime
            .lock()
            .unwrap()
            .get_mut("test-account")
            .unwrap()
            .status
            .next_attempt_at = Some(0);
        service.tick().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(service.cfg.read().await.accounts[0].token, "renewed");
        assert_eq!(service.statuses().await[0].consecutive_failures, 0);
        server.abort();
        std::fs::remove_file(&service.config_path).unwrap();
    }

    #[tokio::test]
    async fn disabled_missing_refresh_and_not_yet_due_accounts_do_not_send_requests() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().route(
            "/refresh",
            post(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { StatusCode::INTERNAL_SERVER_ERROR }
            }),
        );
        let (service, server) = service(app, account()).await;
        service.cfg.write().await.schedule.keepalive_enabled = false;
        service.tick().await;
        service.cfg.write().await.schedule.keepalive_enabled = true;
        service.cfg.write().await.accounts[0].refresh_token.clear();
        service.tick().await;
        assert!(service.statuses().await[0].error.contains("Refresh Token"));
        service.cfg.write().await.accounts[0].refresh_token = "old-refresh".into();
        service.cfg.write().await.accounts[0].expires_at =
            Some(crate::reqlog::now_ms() + 3_600_000);
        service.tick().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn concurrent_refresh_and_deleted_accounts_cannot_overwrite_config() {
        let arrived = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let notify = arrived.clone();
        let resume = release.clone();
        let app = Router::new().route("/refresh", post(move || {
            let notify = notify.clone();
            let resume = resume.clone();
            async move {
                notify.notify_one();
                resume.notified().await;
                Json(serde_json::json!({"code":0,"data":{"accessToken":"new","expires_in":3600}}))
            }
        }));
        let (service, server) = service(app, account()).await;
        let clone = service.clone();
        let first = tokio::spawn(async move { clone.renew_account("test-account").await });
        arrived.notified().await;
        assert!(service
            .renew_account("test-account")
            .await
            .unwrap_err()
            .contains("正在续期"));
        service.cfg.write().await.accounts.clear();
        release.notify_one();
        assert!(first.await.unwrap().unwrap_err().contains("已被移除"));
        assert!(!service.config_path.exists());
        assert!(service.cfg.read().await.accounts.is_empty());
        server.abort();
    }
}
