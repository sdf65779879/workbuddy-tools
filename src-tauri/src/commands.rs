use crate::apikey::{self, ApiKey};
use crate::browser_auth;
use crate::config::{self, AccountConfig, AppConfig};
use crate::gateway::{self, Ctx, GatewayState, GatewayStatus, SharedConfig};
use crate::pool::{AcctStatus, Pool};
use crate::renewal::{self, AccountRefresh};
use crate::reqlog::{self, Event, Filter};
use crate::tasks::{self, TaskRunItem, TaskRunResult, TaskView};
use crate::usage::{UsageDashboard, UsageQuery, UsageSnapshot};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tauri::State;

pub struct GatewayHandle {
    pub shutdown: tokio::sync::watch::Sender<bool>,
    pub task: tokio::task::JoinHandle<()>,
}

pub struct AppState {
    pub shared_cfg: SharedConfig,
    pub account_refreshes: Arc<Mutex<HashSet<String>>>,
    pub renewal: Arc<renewal::Service>,
    pub browser_auth: Arc<browser_auth::Service>,
    pub task_jobs: Arc<crate::task_jobs::Service>,
    pub gateway: Mutex<Option<GatewayHandle>>,
    pub keys: Arc<apikey::Store>,
    pub pool: Arc<Pool>,
    pub reqlog: Arc<reqlog::Recorder>,
    pub usage: Arc<crate::usage::UsageRecorder>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub status: u16,
    pub latency_ms: u128,
    pub content: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountStatusView {
    pub uid: String,
    pub nickname: String,
    pub realm: String,
    pub enabled: bool,
    pub paused: bool,
    pub token_len: usize,
    pub credits: Option<i64>,
    pub remaining_credits: Option<f64>,
    pub total_credits: Option<f64>,
    pub used_credits: Option<f64>,
    pub unlimited: bool,
    pub cooling: bool,
    pub cool_kind: String,
    pub cool_remaining_secs: i64,
    pub reason: String,
    pub soft_streak: u32,
    pub consecutive_fails: u32,
    pub success_count: i64,
    pub err_total: i64,
    pub in_flight: usize,
    pub checkin_done: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BalanceResult {
    pub uid: String,
    pub nickname: String,
    pub credits: Option<i64>,
    pub remaining_credits: Option<f64>,
    pub total_credits: Option<f64>,
    pub used_credits: Option<f64>,
    pub unlimited: bool,
    pub fetched_at: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskScanItem {
    pub uid: String,
    pub nickname: String,
    pub tasks: Vec<TaskView>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// 基础配置
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
    Ok(state.shared_cfg.read().await.clone())
}

#[tauri::command]
pub async fn observe_import_ip() -> Option<crate::import_ip::ImportIp> {
    crate::import_ip::Observer::new().observe().await
}

#[derive(Deserialize)]
pub struct AccountCredentialSnapshot {
    uid: String,
    token: String,
    refresh_token: String,
    expires_at: Option<i64>,
}

#[tauri::command]
pub async fn save_config(
    state: State<'_, AppState>,
    mut cfg: AppConfig,
    expected_credentials: Option<AccountCredentialSnapshot>,
) -> Result<AppConfig, String> {
    let _credential_update = expected_credentials
        .as_ref()
        .map(|expected| AccountRefresh::start(&state.account_refreshes, &expected.uid))
        .transpose()?;
    let mut current = state.shared_cfg.write().await;
    // Settings can arrive from a renderer snapshot taken before a background renewal.
    preserve_account_credentials(&current, &mut cfg, expected_credentials.as_ref())?;
    config::save_config(&cfg)?;
    *current = cfg.clone();
    Ok(cfg)
}

fn preserve_account_credentials(
    current: &AppConfig,
    submitted: &mut AppConfig,
    expected: Option<&AccountCredentialSnapshot>,
) -> Result<(), String> {
    if let Some(expected) = expected {
        let existing = current
            .accounts
            .iter()
            .find(|account| account.uid == expected.uid)
            .ok_or_else(|| "导入期间账号已被移除".to_string())?;
        if existing.token != expected.token
            || existing.refresh_token != expected.refresh_token
            || existing.expires_at != expected.expires_at
        {
            return Err("导入期间账号凭证已变更，请重新读取并导入".into());
        }
    }
    for account in &mut submitted.accounts {
        if expected.is_some_and(|expected| account.uid == expected.uid) {
            continue;
        }
        if let Some(existing) = current
            .accounts
            .iter()
            .find(|existing| existing.uid == account.uid)
        {
            copy_account_credentials(account, existing);
        }
    }
    Ok(())
}

fn copy_account_credentials(target: &mut AccountConfig, source: &AccountConfig) {
    target.token.clone_from(&source.token);
    target.refresh_token.clone_from(&source.refresh_token);
    target.expires_at = source.expires_at;
    // An ordinary settings snapshot must not overwrite a more recent login
    // observation saved by browser authorization.
    target.login_ip.clone_from(&source.login_ip);
    target.import_ip.clone_from(&source.import_ip);
}

#[tauri::command]
pub async fn test_connection(cfg: AppConfig) -> Result<TestResult, String> {
    gateway::probe::test_connection(&cfg).await
}

#[tauri::command]
pub async fn get_data_dir() -> Result<String, String> {
    Ok(config::data_dir().display().to_string())
}

#[tauri::command]
pub async fn switch_account(
    state: State<'_, AppState>,
    uid: String,
) -> Result<crate::account_switch::SwitchResult, String> {
    let cfg = state.shared_cfg.read().await.clone();
    let account = cfg
        .accounts
        .iter()
        .find(|a| a.uid == uid)
        .ok_or_else(|| "账号不存在".to_string())?;
    // 切号是阻塞式（会运行 WorkBuddy 官方客户端获取密钥），放到 worker 线程
    let account = account.clone();
    tauri::async_runtime::spawn_blocking(move || crate::account_switch::switch_account(&account))
        .await
        .map_err(|e| format!("切号后台任务失败: {e}"))?
}

/// 从本机当前登录态读取 access token（本机导入）。
#[tauri::command]
pub async fn read_current_workbuddy_token() -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(crate::account_switch::read_current_access_token)
        .await
        .map_err(|e| format!("读取本机登录态失败: {e}"))?
}

#[tauri::command]
pub async fn read_current_workbuddy_account() -> Result<Option<AccountConfig>, String> {
    tauri::async_runtime::spawn_blocking(crate::account_switch::read_current_account)
        .await
        .map_err(|e| format!("读取本机登录态失败: {e}"))?
}

#[derive(Debug, Clone, Serialize)]
pub struct RefreshResult {
    pub ok: bool,
    pub uid: String,
    pub nickname: String,
    pub message: String,
}

/// Renew only the gateway credentials; official session switching remains a separate command.
#[tauri::command]
pub async fn refresh_workbuddy_token(
    state: State<'_, AppState>,
    uid: String,
) -> Result<RefreshResult, String> {
    let account = state.renewal.renew_account(&uid).await?;
    Ok(RefreshResult {
        ok: true,
        uid: account.uid,
        message: format!("已续期账号「{}」的网关凭证", account.nickname),
        nickname: account.nickname,
    })
}

#[tauri::command]
pub async fn keepalive_status(
    state: State<'_, AppState>,
) -> Result<Vec<renewal::RenewalStatus>, String> {
    Ok(state.renewal.statuses().await)
}

#[tauri::command]
pub async fn start_browser_login(
    state: State<'_, AppState>,
    realm: String,
) -> Result<browser_auth::LoginStart, String> {
    state.browser_auth.start(&realm).await
}

#[tauri::command]
pub async fn poll_browser_login(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<browser_auth::LoginPoll, String> {
    state.browser_auth.poll(&session_id).await
}

#[tauri::command]
pub fn cancel_browser_login(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    state.browser_auth.cancel(&session_id)
}

// ---------------------------------------------------------------------------
// 网关启停
// ---------------------------------------------------------------------------

fn build_ctx(state: &AppState) -> Result<Ctx, String> {
    Ok(Arc::new(GatewayState {
        cfg: state.shared_cfg.clone(),
        pool: state.pool.clone(),
        keys: state.keys.clone(),
        reqlog: state.reqlog.clone(),
        usage: state.usage.clone(),
        client: gateway::upstream::new_client()?,
    }))
}

#[tauri::command]
pub async fn start_gateway(state: State<'_, AppState>) -> Result<GatewayStatus, String> {
    let cfg = state.shared_cfg.read().await.clone();
    cfg.validate()?;

    let existing = {
        let mut guard = state.gateway.lock().map_err(|e| e.to_string())?;
        guard.take()
    };
    if let Some(old) = existing {
        let _ = old.shutdown.send(true);
        let _ = old.task.await;
    }

    let port = cfg.listen_port;
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("端口 {port} 绑定失败（可能被占用）: {e}"))?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let ctx = build_ctx(&state)?;
    let router = gateway::build_router(ctx);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let mut rx = shutdown_rx;
                loop {
                    if *rx.borrow() {
                        break;
                    }
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
    });

    let mut guard = state.gateway.lock().map_err(|e| e.to_string())?;
    *guard = Some(GatewayHandle {
        shutdown: shutdown_tx,
        task,
    });

    Ok(GatewayStatus {
        running: true,
        port,
        base_url: format!("http://127.0.0.1:{port}/v1"),
        models: cfg.models.iter().map(|m| m.codex_name.clone()).collect(),
    })
}

#[tauri::command]
pub async fn stop_gateway(state: State<'_, AppState>) -> Result<GatewayStatus, String> {
    let cfg = state.shared_cfg.read().await.clone();
    let handle = {
        let mut guard = state.gateway.lock().map_err(|e| e.to_string())?;
        guard.take()
    };
    if let Some(handle) = handle {
        let _ = handle.shutdown.send(true);
        let _ = handle.task.await;
    }
    Ok(GatewayStatus {
        running: false,
        port: cfg.listen_port,
        base_url: format!("http://127.0.0.1:{}/v1", cfg.listen_port),
        models: cfg.models.iter().map(|m| m.codex_name.clone()).collect(),
    })
}

#[tauri::command]
pub async fn gateway_status(state: State<'_, AppState>) -> Result<GatewayStatus, String> {
    let cfg = state.shared_cfg.read().await.clone();
    let running = {
        let mut guard = state.gateway.lock().map_err(|e| e.to_string())?;
        let mut running = false;
        if let Some(h) = guard.as_ref() {
            running = !h.task.is_finished();
        }
        if !running {
            *guard = None;
        }
        running
    };
    Ok(GatewayStatus {
        running,
        port: cfg.listen_port,
        base_url: format!("http://127.0.0.1:{}/v1", cfg.listen_port),
        models: cfg.models.iter().map(|m| m.codex_name.clone()).collect(),
    })
}

// ---------------------------------------------------------------------------
// Token 提取
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn extract_token_from_logs() -> Result<Option<String>, String> {
    Ok(scan_logs_for_token())
}

fn scan_logs_for_token() -> Option<String> {
    let logs_dir = dirs::home_dir()?.join(".workbuddy").join("logs");
    let mut sk_token: Option<String> = None;
    let mut last_token: Option<String> = None;
    let mut dirs = vec![logs_dir];
    while let Some(dir) = dirs.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".log") && !name.ends_with(".json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                let mut rest = line;
                while let Some(pos) = rest.find("Bearer ") {
                    rest = &rest[pos + 7..];
                    let token: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                        .collect();
                    if token.len() >= 20 {
                        if token.starts_with("sk-") {
                            if sk_token.is_none() {
                                sk_token = Some(token.clone());
                            }
                        } else {
                            last_token = Some(token);
                        }
                    }
                }
            }
        }
    }
    sk_token.or(last_token)
}

// ---------------------------------------------------------------------------
// API Keys
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_api_keys(state: State<'_, AppState>) -> Result<Vec<ApiKey>, String> {
    Ok(state.keys.list())
}

#[tauri::command]
pub async fn create_api_key(
    state: State<'_, AppState>,
    req: apikey::CreateRequest,
) -> Result<ApiKey, String> {
    state.keys.create(req)
}

#[tauri::command]
pub async fn update_api_key(
    state: State<'_, AppState>,
    id: String,
    req: apikey::UpdateRequest,
) -> Result<ApiKey, String> {
    state.keys.update(&id, req)
}

#[tauri::command]
pub async fn delete_api_key(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.keys.delete(&id)
}

// ---------------------------------------------------------------------------
// 账号池
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn pool_status(state: State<'_, AppState>) -> Result<Vec<AccountStatusView>, String> {
    let cfg = state.shared_cfg.read().await.clone();
    let statuses = state.pool.statuses(&cfg.accounts);
    let now = std::time::Instant::now();
    Ok(statuses
        .into_iter()
        .map(|(a, s)| status_view(&a, &s, now))
        .collect())
}

fn status_view(a: &AccountConfig, s: &AcctStatus, now: std::time::Instant) -> AccountStatusView {
    let cool_remaining = s
        .cooling_until
        .map(|u| u.duration_since(now).as_secs() as i64)
        .unwrap_or(0);
    AccountStatusView {
        uid: a.uid.clone(),
        nickname: a.nickname.clone(),
        realm: a.realm.clone(),
        enabled: a.enabled,
        paused: a.paused,
        token_len: a.token.len(),
        credits: if s.unlimited { None } else { s.credits },
        remaining_credits: s.remaining_credits,
        total_credits: s.total_credits,
        used_credits: s.used_credits,
        unlimited: s.unlimited,
        cooling: s.cooling_until.map(|u| now < u).unwrap_or(false),
        cool_kind: s.cool_kind.clone(),
        cool_remaining_secs: cool_remaining.max(0),
        reason: s.reason.clone(),
        soft_streak: s.soft_streak,
        consecutive_fails: s.consecutive_fails,
        success_count: s.success_count,
        err_total: s.err_total,
        in_flight: s.in_flight,
        checkin_done: s.checkin_done,
    }
}

#[tauri::command]
pub async fn refresh_balances(state: State<'_, AppState>) -> Result<Vec<BalanceResult>, String> {
    let cfg = state.shared_cfg.read().await.clone();
    let client = gateway::upstream::new_client()?;
    // Disabled/paused accounts still belong to the vault's real balance total.
    let mut pending = Vec::with_capacity(cfg.accounts.len());
    for account in cfg.accounts {
        let client = client.clone();
        let base_url = cfg.base_url.clone();
        let pool = state.pool.clone();
        pending.push(async move { refresh_balance_for(&client, &base_url, &account, &pool).await });
    }
    let results = futures_util::stream::iter(pending)
        .buffered(4)
        .collect()
        .await;
    Ok(results)
}

async fn refresh_balance_for(
    client: &reqwest::Client,
    base_url: &str,
    account: &AccountConfig,
    pool: &Pool,
) -> BalanceResult {
    match crate::balances::fetch(client, base_url, account).await {
        Ok(balance) => {
            pool.set_balance(&account.uid, &balance);
            BalanceResult {
                uid: account.uid.clone(),
                nickname: account.nickname.clone(),
                credits: balance.remaining.map(|remaining| remaining.round() as i64),
                remaining_credits: balance.remaining,
                total_credits: balance.total,
                used_credits: balance.used,
                unlimited: balance.unlimited,
                fetched_at: Some(reqlog::now_ms()),
                error: None,
            }
        }
        Err(error) => BalanceResult {
            uid: account.uid.clone(),
            nickname: account.nickname.clone(),
            credits: None,
            remaining_credits: None,
            total_credits: None,
            used_credits: None,
            unlimited: false,
            fetched_at: None,
            error: Some(error),
        },
    }
}

// ---------------------------------------------------------------------------
// 用量与日志
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn usage_snapshot(state: State<'_, AppState>) -> Result<UsageSnapshot, String> {
    Ok(state.usage.snapshot())
}

#[tauri::command]
pub async fn usage_dashboard(
    state: State<'_, AppState>,
    query: UsageQuery,
) -> Result<UsageDashboard, String> {
    query.validate()?;
    Ok(state.usage.dashboard(&query))
}

#[tauri::command]
pub async fn reqlog_snapshot(state: State<'_, AppState>) -> Result<reqlog::Snapshot, String> {
    Ok(state.reqlog.snapshot())
}

#[tauri::command]
pub async fn reqlog_read(
    state: State<'_, AppState>,
    limit: usize,
    filter: Filter,
) -> Result<Vec<Event>, String> {
    Ok(state.reqlog.read_archive(limit, filter))
}

// ---------------------------------------------------------------------------
// 任务中心
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn task_scan_all(state: State<'_, AppState>) -> Result<Vec<TaskScanItem>, String> {
    let cfg = state.shared_cfg.read().await.clone();
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "无法创建任务查询客户端".to_string())?;
    let accounts: Vec<_> = cfg
        .accounts
        .iter()
        .filter(|a| a.enabled && !a.paused && a.realm == "cn")
        .cloned()
        .collect();
    Ok(futures_util::stream::iter(accounts)
        .map(|a| {
            let client = client.clone();
            let base = cfg.base_url.clone();
            async move {
                let mut res = match tokio::time::timeout(
                    std::time::Duration::from_secs(35),
                    tasks::list_tasks_for_account(&client, &base, &a),
                )
                .await
                {
                    Ok(res) => res,
                    Err(_) => Err("任务查询超时，请重试".into()),
                };
                if let Err(error) = &mut res {
                    for secret in [a.token.trim(), a.refresh_token.trim()] {
                        if !secret.is_empty() {
                            *error = error.replace(secret, "[已隐藏凭证]");
                        }
                    }
                }
                TaskScanItem {
                    uid: a.uid,
                    nickname: a.nickname,
                    tasks: res.clone().unwrap_or_default(),
                    error: res.err(),
                }
            }
        })
        .buffered(4)
        .collect()
        .await)
}

#[tauri::command]
pub async fn start_task_job(
    state: State<'_, AppState>,
    uid: Option<String>,
) -> Result<crate::task_jobs::Snapshot, String> {
    state.task_jobs.start(uid).await
}

#[tauri::command]
pub fn task_job_status(state: State<'_, AppState>) -> Option<crate::task_jobs::Snapshot> {
    state.task_jobs.status()
}

async fn wait_task_job(
    service: &Arc<crate::task_jobs::Service>,
    uid: Option<String>,
) -> Result<TaskRunResult, String> {
    let started = service.start(uid).await?;
    loop {
        let snapshot = service.status().ok_or("任务作业不存在")?;
        if snapshot.id != started.id {
            return Err("任务已结束，新作业已启动，请在成长任务查看结果".into());
        }
        if snapshot.status != "running" {
            return Ok(snapshot.result);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

#[tauri::command]
pub async fn task_run_all(state: State<'_, AppState>) -> Result<TaskRunResult, String> {
    wait_task_job(&state.task_jobs, None).await
}

#[tauri::command]
pub async fn task_run_one(state: State<'_, AppState>, uid: String) -> Result<TaskRunItem, String> {
    wait_task_job(&state.task_jobs, Some(uid))
        .await?
        .items
        .into_iter()
        .next()
        .ok_or("任务未产生账号结果".into())
}

/// 单个账号签到，不执行成长任务或领取其他福利。
#[tauri::command]
pub async fn checkin_one(state: State<'_, AppState>, uid: String) -> Result<TaskRunItem, String> {
    let started_at_ms = reqlog::now_ms();
    let cfg = state.shared_cfg.read().await.clone();
    let account = cfg
        .accounts
        .iter()
        .find(|a| a.uid == uid)
        .ok_or_else(|| "账号不存在".to_string())?;
    let client = gateway::upstream::new_client()?;
    let result = tasks::checkin(&client, cfg.base_url.trim_end_matches('/'), account).await;
    if result.is_ok() {
        state.pool.set_checkin_done(&account.uid, started_at_ms);
    }
    Ok(TaskRunItem {
        uid: account.uid.clone(),
        nickname: account.nickname.clone(),
        checkin: result.as_ref().cloned().unwrap_or_else(|_| "failed".into()),
        accepted: 0,
        completed: 0,
        skipped: 0,
        waiting: 0,
        claimed: 0,
        credit: 0,
        energy: 0,
        error: result.err(),
    })
}

/// 保活（续 token）：调用账号信息接口验证 token 有效性并刷新余额。
#[tauri::command]
pub async fn keepalive_one(
    state: State<'_, AppState>,
    uid: String,
) -> Result<BalanceResult, String> {
    let cfg = state.shared_cfg.read().await.clone();
    let account = cfg
        .accounts
        .iter()
        .find(|a| a.uid == uid)
        .ok_or_else(|| "账号不存在".to_string())?;
    let client = gateway::upstream::new_client()?;
    Ok(refresh_balance_for(&client, &cfg.base_url, account, &state.pool).await)
}

#[cfg(test)]
mod renewal_tests {
    use super::*;

    fn account(uid: &str, token: &str, refresh: &str, expires_at: i64) -> AccountConfig {
        serde_json::from_value(serde_json::json!({
            "uid": uid, "nickname": "Original", "token": token, "refresh_token": refresh,
            "expires_at": expires_at, "realm": "cn", "enabled": true, "paused": false
        }))
        .unwrap()
    }

    #[test]
    fn saving_settings_keeps_renewed_credentials_and_accepts_imports() {
        let mut current = AppConfig {
            accounts: vec![account("existing", "renewed", "new-refresh", 1000)],
            ..AppConfig::default()
        };
        current.accounts[0].login_ip = Some("203.0.113.8".into());
        current.accounts[0].import_ip = Some(crate::import_ip::ImportIp {
            ip: "203.0.113.9".into(),
            observed_at: 1000,
        });
        let mut submitted = AppConfig {
            accounts: vec![
                account("existing", "old", "old-refresh", 500),
                account("import", "imported", "imported-refresh", 750),
            ],
            ..AppConfig::default()
        };
        submitted.accounts[0].nickname = "Renamed".into();
        submitted.accounts[0].paused = true;
        submitted.accounts[0].workbuddy_uid = Some("official-user".into());

        preserve_account_credentials(&current, &mut submitted, None).unwrap();

        let saved = &submitted.accounts[0];
        assert_eq!(saved.token, "renewed");
        assert_eq!(saved.refresh_token, "new-refresh");
        assert_eq!(saved.expires_at, Some(1000));
        assert_eq!(saved.login_ip.as_deref(), Some("203.0.113.8"));
        assert_eq!(saved.import_ip, current.accounts[0].import_ip);
        assert_eq!(saved.nickname, "Renamed");
        assert!(saved.paused);
        assert_eq!(saved.official_uid(), Some("official-user"));
        assert_eq!(submitted.accounts[1].token, "imported");
    }

    #[test]
    fn reimport_updates_credentials_only_from_a_current_snapshot() {
        let mut current = AppConfig {
            accounts: vec![account("existing", "access", "", 500)],
            ..AppConfig::default()
        };
        let mut submitted = AppConfig {
            accounts: vec![account("existing", "access", "imported-refresh", 750)],
            ..AppConfig::default()
        };
        let expected = AccountCredentialSnapshot {
            uid: "existing".into(),
            token: "access".into(),
            refresh_token: "".into(),
            expires_at: Some(500),
        };

        preserve_account_credentials(&current, &mut submitted, Some(&expected)).unwrap();
        assert_eq!(submitted.accounts[0].refresh_token, "imported-refresh");
        assert_eq!(submitted.accounts[0].expires_at, Some(750));

        current.accounts[0].refresh_token = "renewed-refresh".into();
        assert!(preserve_account_credentials(&current, &mut submitted, Some(&expected)).is_err());
    }

    #[test]
    fn one_renewal_at_a_time_per_account() {
        let pending = Mutex::default();
        let first = AccountRefresh::start(&pending, "a").unwrap();
        assert!(AccountRefresh::start(&pending, "a").is_err());
        assert!(AccountRefresh::start(&pending, "b").is_ok());
        drop(first);
        assert!(AccountRefresh::start(&pending, "a").is_ok());
    }
}

#[cfg(test)]
mod balance_status_tests {
    use super::*;
    #[test]
    fn pool_status_exposes_exact_balances_and_hides_unlimited_routing_weight() {
        let account: AccountConfig = serde_json::from_value(serde_json::json!({
            "uid":"test-user","nickname":"test","token":"","realm":"cn","enabled":true,"paused":false
        })).unwrap();
        let pool = Pool::new(Arc::new(tokio::sync::RwLock::new(AppConfig::default())));
        pool.set_balance(
            &account.uid,
            &crate::balances::BalanceObservation {
                remaining: Some(1187.55),
                total: Some(2000.0),
                used: Some(812.45),
                unlimited: false,
            },
        );
        let statuses = pool.statuses(&[account.clone()]);
        let view = status_view(&account, &statuses[0].1, std::time::Instant::now());
        assert_eq!(view.remaining_credits, Some(1187.55));
        assert_eq!(view.total_credits, Some(2000.0));
        assert_eq!(view.used_credits, Some(812.45));
        pool.set_balance(
            &account.uid,
            &crate::balances::BalanceObservation {
                remaining: None,
                total: None,
                used: Some(12.25),
                unlimited: true,
            },
        );
        let statuses = pool.statuses(&[account.clone()]);
        assert!(
            statuses[0].1.credits.unwrap() > 100,
            "routing still sees sufficient quota"
        );
        let view = status_view(&account, &statuses[0].1, std::time::Instant::now());
        assert!(view.unlimited);
        assert_eq!(view.credits, None);
        assert_eq!(view.remaining_credits, None);
        assert_eq!(view.total_credits, None);
        assert_eq!(view.used_credits, Some(12.25));
    }
}
