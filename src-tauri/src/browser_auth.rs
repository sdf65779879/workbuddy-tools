//! Browser device authorization. The upstream state and credentials remain in the
//! backend; the renderer receives an opaque local session ID and safe progress only.
use crate::{
    config::{self, AccountConfig},
    gateway::SharedConfig,
    renewal::AccountRefresh,
};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

const TTL_MS: i64 = 15 * 60_000;
const MAX_SESSIONS: usize = 8;
const MAX_CACHED_SESSIONS: usize = 32;
const RESPONSE_LIMIT: usize = 1024 * 1024;
const LOGIN_UA: &str = "CLI/2.63.2 CodeBuddy/2.63.2";

#[derive(Clone, Debug, Serialize)]
pub struct LoginStart {
    pub session_id: String,
    pub url: String,
    pub realm: String,
    pub expires_at: i64,
    pub poll_interval_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LoginPoll {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl LoginPoll {
    fn pending(message: &str) -> Self {
        Self {
            status: "pending".into(),
            uid: None,
            nickname: None,
            message: Some(message.into()),
        }
    }
    fn expired() -> Self {
        Self {
            status: "expired".into(),
            uid: None,
            nickname: None,
            message: Some("授权链接已过期，请重新获取".into()),
        }
    }
}

#[derive(Clone)]
struct TokenBundle {
    access: String,
    refresh: String,
    domain: String,
    expires_at: Option<i64>,
    login_ip: Option<String>,
}

struct Session {
    upstream_state: String,
    realm: String,
    expires_at: i64,
    polling: AtomicBool,
    token: Mutex<Option<TokenBundle>>,
    completed: Mutex<Option<LoginPoll>>,
}

impl Session {
    fn is_completed(&self) -> bool {
        self.completed
            .lock()
            .map(|result| result.is_some())
            .unwrap_or(false)
    }
}

fn prepare_session_slot(
    sessions: &mut HashMap<String, Arc<Session>>,
    now: i64,
) -> Result<(), String> {
    sessions.retain(|_, session| session.expires_at > now);
    if sessions
        .values()
        .filter(|session| !session.is_completed())
        .count()
        >= MAX_SESSIONS
    {
        return Err("授权会话过多，请关闭旧会话后重试".into());
    }
    // Completed results remain available for harmless repeated polling, but they
    // neither consume active login slots nor grow the cache without bound.
    let remove_count = sessions.len().saturating_sub(MAX_CACHED_SESSIONS - 1);
    if remove_count > 0 {
        let mut completed: Vec<_> = sessions
            .iter()
            .filter(|(_, session)| session.is_completed())
            .map(|(id, session)| (session.expires_at, id.clone()))
            .collect();
        completed.sort();
        for (_, id) in completed.into_iter().take(remove_count) {
            sessions.remove(&id);
        }
    }
    Ok(())
}

struct PollGuard(Arc<Session>);
impl Drop for PollGuard {
    fn drop(&mut self) {
        self.0.polling.store(false, Ordering::Release);
    }
}

struct StartGuard<'a>(&'a AtomicUsize);
impl Drop for StartGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct Service {
    cfg: SharedConfig,
    account_refreshes: Arc<Mutex<HashSet<String>>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    starting: AtomicUsize,
    client: reqwest::Client,
    import_ip: crate::import_ip::Observer,
    base_cn: String,
    base_global: String,
    config_path: PathBuf,
}

impl Service {
    pub fn new(cfg: SharedConfig, account_refreshes: Arc<Mutex<HashSet<String>>>) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            account_refreshes,
            sessions: Mutex::new(HashMap::new()),
            starting: AtomicUsize::new(0),
            import_ip: crate::import_ip::Observer::new(),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("browser authorization HTTP client"),
            base_cn: "https://copilot.tencent.com".into(),
            base_global: "https://www.workbuddy.ai".into(),
            config_path: config::config_path(),
        })
    }

    fn base(&self, realm: &str) -> Result<(&str, &str), String> {
        match realm {
            "cn" => Ok((&self.base_cn, "https://www.codebuddy.cn")),
            "global" => Ok((&self.base_global, "https://www.workbuddy.ai")),
            _ => Err("账号区域必须为 cn 或 global".into()),
        }
    }

    async fn request(
        &self,
        realm: &str,
        path: &str,
        state: Option<&str>,
        bearer: Option<&str>,
        post: bool,
    ) -> Result<Value, String> {
        let (base, origin) = self.base(realm)?;
        let url = format!("{base}{path}");
        let mut request = if post {
            self.client.post(&url).json(&json!({}))
        } else {
            self.client.get(&url)
        };
        request = request
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/plain, */*")
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Origin", origin)
            .header("Referer", format!("{origin}/"))
            .header("User-Agent", LOGIN_UA);
        if let Some(state) = state {
            request = request.query(&[("state", state)]);
        }
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        let response = request
            .send()
            .await
            .map_err(|_| "授权服务暂时无法连接，请重试".to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("授权服务返回 HTTP {}，请重试", status.as_u16()));
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "读取授权服务响应失败".to_string())?;
            if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
                return Err("授权服务响应过大".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| "授权服务返回了无效 JSON".to_string())?;
        if !value.is_object() || integer(&value["code"]).is_none() {
            return Err("授权服务响应缺少有效业务状态码".into());
        }
        Ok(value)
    }

    pub async fn start(&self, realm: &str) -> Result<LoginStart, String> {
        self.base(realm)?;
        if self
            .starting
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_SESSIONS).then_some(count + 1)
            })
            .is_err()
        {
            return Err("正在创建的授权会话过多，请稍后重试".into());
        }
        let _starting = StartGuard(&self.starting);
        {
            let mut sessions = self.sessions.lock().map_err(|_| "无法读取授权会话")?;
            prepare_session_slot(&mut sessions, now_ms())?;
        }
        let envelope = self
            .request(
                realm,
                "/v2/plugin/auth/state?platform=CLI",
                None,
                None,
                true,
            )
            .await?;
        let data = success_data(&envelope, "获取授权链接")?;
        let upstream_state = text(data, &["state"])
            .filter(|state| state.len() <= 2048)
            .ok_or("授权服务未返回有效 state")?
            .to_string();
        let url = text(data, &["authUrl", "auth_url"]).ok_or("授权服务未返回授权链接")?;
        validate_auth_url(url)?;
        let expires_at = now_ms() + TTL_MS;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let mut sessions = self.sessions.lock().map_err(|_| "无法保存授权会话")?;
        prepare_session_slot(&mut sessions, now_ms())?;
        sessions.insert(
            id.clone(),
            Arc::new(Session {
                upstream_state,
                realm: realm.into(),
                expires_at,
                polling: AtomicBool::new(false),
                token: Mutex::new(None),
                completed: Mutex::new(None),
            }),
        );
        Ok(LoginStart {
            session_id: id,
            url: url.into(),
            realm: realm.into(),
            expires_at,
            poll_interval_ms: 3000,
        })
    }

    pub async fn poll(&self, session_id: &str) -> Result<LoginPoll, String> {
        let session = self
            .sessions
            .lock()
            .map_err(|_| "无法读取授权会话")?
            .get(session_id)
            .cloned()
            .ok_or("授权会话已关闭，请重新获取授权链接")?;
        if let Some(done) = session
            .completed
            .lock()
            .map_err(|_| "无法读取登录结果")?
            .clone()
        {
            return Ok(done);
        }
        if now_ms() >= session.expires_at {
            return Ok(LoginPoll::expired());
        }
        if session
            .polling
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(LoginPoll::pending("正在确认授权结果"));
        }
        let _polling = PollGuard(session.clone());
        let cached = session
            .token
            .lock()
            .map_err(|_| "无法读取授权凭证")?
            .clone();
        let token = if let Some(token) = cached {
            token
        } else {
            let envelope = self
                .request(
                    &session.realm,
                    "/v2/plugin/auth/token",
                    Some(&session.upstream_state),
                    None,
                    false,
                )
                .await?;
            if !matches!(integer(&envelope["code"]), Some(0 | 200)) {
                if is_pending(&envelope) {
                    return Ok(LoginPoll::pending("等待在浏览器完成授权"));
                }
                return Err(business_failure(&envelope, "查询授权状态"));
            }
            let token = parse_token(success_data(&envelope, "查询授权状态")?, now_ms())?;
            *session.token.lock().map_err(|_| "无法保存授权凭证")? = Some(token.clone());
            token
        };
        let account_envelope = self
            .request(
                &session.realm,
                "/v2/plugin/login/account",
                Some(&session.upstream_state),
                Some(&token.access),
                false,
            )
            .await?;
        let account_data = success_data(&account_envelope, "读取登录账号")?;
        let mut incoming = parse_account(account_data, &token, &session.realm)?;
        if crate::renewal::token_expiry_ms(&incoming).is_some_and(|expiry| expiry <= now_ms()) {
            return Err("授权服务返回的 Access Token 已过期，请重新授权".into());
        }
        // Observe only once authorization has finished. No account credentials
        // are shared with the independent IP service. Failure is non-fatal.
        if incoming.login_ip.is_none() {
            incoming.import_ip = self.import_ip.observe().await;
        }
        self.commit(session_id, &session, incoming).await
    }

    async fn commit(
        &self,
        session_id: &str,
        session: &Arc<Session>,
        incoming: AccountConfig,
    ) -> Result<LoginPoll, String> {
        let local_uid = {
            let cfg = self.cfg.read().await;
            find_existing(&cfg.accounts, &incoming)?
                .map(|account| account.uid.clone())
                .unwrap_or_else(|| incoming.uid.clone())
        };
        let _update = match AccountRefresh::start(&self.account_refreshes, &local_uid) {
            Ok(lock) => lock,
            Err(_) => return Ok(LoginPoll::pending("该账号正在续期，稍后自动完成导入")),
        };
        let mut cfg = self.cfg.write().await;
        // Cancel and the final write share this mutex, so cancellation that wins
        // the race cannot be followed by a late credential write.
        let sessions = self.sessions.lock().map_err(|_| "无法确认授权会话")?;
        if !sessions
            .get(session_id)
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            return Err("授权会话已关闭，未导入账号".into());
        }
        if now_ms() >= session.expires_at {
            return Ok(LoginPoll::expired());
        }
        let mut next = cfg.clone();
        let existing = find_existing(&next.accounts, &incoming)?;
        if existing.is_some_and(|account| account.uid != local_uid) {
            return Ok(LoginPoll::pending("账号列表已更新，正在重新确认导入"));
        }
        let saved = if let Some(existing) = existing {
            merge_account(existing, &incoming)
        } else {
            incoming
        };
        if let Some(index) = next
            .accounts
            .iter()
            .position(|account| account.uid == saved.uid)
        {
            next.accounts[index] = saved.clone();
        } else {
            next.accounts.push(saved.clone());
        }
        config::save_config_at(&self.config_path, &next)?;
        *cfg = next;
        let result = LoginPoll {
            status: "completed".into(),
            uid: Some(saved.uid),
            nickname: Some(saved.nickname),
            message: Some("账号已授权并加入账号池".into()),
        };
        *session.completed.lock().map_err(|_| "无法保存登录结果")? = Some(result.clone());
        // Completed polls are idempotent; credentials no longer need to remain in
        // the temporary device session after their durable commit.
        *session.token.lock().map_err(|_| "无法清理临时凭证")? = None;
        Ok(result)
    }

    pub fn cancel(&self, session_id: &str) -> Result<(), String> {
        self.sessions
            .lock()
            .map_err(|_| "无法关闭授权会话")?
            .remove(session_id);
        Ok(())
    }
}

fn now_ms() -> i64 {
    crate::reqlog::now_ms()
}
fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}
fn text<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn success_data<'a>(envelope: &'a Value, stage: &str) -> Result<&'a Value, String> {
    if !matches!(integer(&envelope["code"]), Some(0 | 200)) {
        return Err(business_failure(envelope, stage));
    }
    envelope
        .get("data")
        .filter(|data| data.is_object())
        .ok_or_else(|| format!("{stage}响应缺少有效 data"))
}

fn business_failure(value: &Value, stage: &str) -> String {
    // A business code identifies the error without echoing a credential/state that
    // an upstream or intermediate service might have put in its free-form message.
    match integer(&value["code"]) {
        Some(code) => format!("{stage}失败（code={code}），请重新获取授权链接或重试"),
        None => format!("{stage}响应无效，请重试"),
    }
}

fn is_pending(value: &Value) -> bool {
    // The auth/token endpoint returns HTTP 200 + 11217 ("login ing...")
    // before browser authorization completes. This is a progress state, not a
    // failed login. Use the code as well so localized messages remain compatible.
    if integer(&value["code"]) == Some(11217) {
        return true;
    }
    let message = text(value, &["msg", "message"])
        .unwrap_or("")
        .to_ascii_lowercase();
    let message = message
        .trim_end_matches(|character| matches!(character, '.' | '…' | '。' | '!' | '！'))
        .trim();
    matches!(
        message,
        "login ing"
            | "logging in"
            | "waiting for login"
            | "authorization_pending"
            | "pending"
            | "登录中"
            | "等待登录"
            | "等待授权"
    )
}

fn validate_auth_url(value: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(value).map_err(|_| "授权链接格式无效".to_string())?;
    let host = url.host_str().unwrap_or("");
    let official = ["codebuddy.cn", "workbuddy.cn", "workbuddy.ai"]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
        || matches!(host, "copilot.tencent.com" | "copilot.codebuddy.ai");
    if url.scheme() != "https"
        || !official
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
    {
        return Err("授权服务返回了非官方 HTTPS 授权链接，已拒绝打开".into());
    }
    Ok(())
}

fn parse_token(data: &Value, now: i64) -> Result<TokenBundle, String> {
    let access =
        text(data, &["accessToken", "access_token"]).ok_or("授权响应缺少 Access Token，请重试")?;
    let lifetime = data
        .get("expiresIn")
        .or_else(|| data.get("expires_in"))
        .and_then(integer)
        .filter(|seconds| *seconds > 0 && *seconds < 10 * 366 * 86_400)
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|ms| now.checked_add(ms));
    let expiry = data
        .get("expiresAt")
        .or_else(|| data.get("expires_at"))
        .and_then(integer)
        .filter(|value| *value > 0)
        .and_then(|value| {
            if value < 1_000_000_000_000 {
                value.checked_mul(1000)
            } else {
                Some(value)
            }
        });
    let token = TokenBundle {
        access: access.into(),
        refresh: text(data, &["refreshToken", "refresh_token"])
            .unwrap_or("")
            .into(),
        domain: text(data, &["domain"]).unwrap_or("").into(),
        expires_at: expiry
            .or(lifetime)
            .or_else(|| crate::renewal::jwt_expiry_ms(access)),
        login_ip: observed_login_ip(data),
    };
    if token.expires_at.is_some_and(|expiry| expiry <= now) {
        return Err("授权服务返回的 Access Token 已过期，请重新授权".into());
    }
    Ok(token)
}

fn parse_account(data: &Value, token: &TokenBundle, realm: &str) -> Result<AccountConfig, String> {
    let uid = text(data, &["uid", "userId", "user_id"])
        .filter(|uid| AccountConfig::valid_workbuddy_uid(uid))
        .ok_or("授权完成但未取得真实 WorkBuddy UID，请重试")?;
    Ok(AccountConfig {
        uid: uid.into(),
        workbuddy_uid: Some(uid.into()),
        nickname: text(data, &["nickname", "nickName"]).unwrap_or(uid).into(),
        token: token.access.clone(),
        refresh_token: token.refresh.clone(),
        email: text(data, &["email"]).unwrap_or("").into(),
        login_ip: observed_login_ip(data).or_else(|| token.login_ip.clone()),
        import_ip: None,
        enterprise_id: text(data, &["enterpriseId", "enterprise_id"])
            .unwrap_or("")
            .into(),
        enterprise_name: text(data, &["enterpriseName", "enterprise_name"])
            .unwrap_or("")
            .into(),
        domain: token.domain.clone(),
        expires_at: token.expires_at,
        realm: realm.into(),
        enabled: true,
        paused: false,
    })
}

fn find_existing<'a>(
    accounts: &'a [AccountConfig],
    incoming: &AccountConfig,
) -> Result<Option<&'a AccountConfig>, String> {
    let mut matches = accounts.iter().filter(|account| {
        account.official_uid() == incoming.official_uid() || account.token.trim() == incoming.token
    });
    let found = matches.next();
    if matches.next().is_some() {
        return Err("多个已有账号匹配本次授权，请先合并重复账号后重试".into());
    }
    if let Some(account) = found {
        if account
            .official_uid()
            .is_some_and(|uid| Some(uid) != incoming.official_uid())
        {
            return Err("已有 Token 的真实账号身份与本次授权不一致，未覆盖账号".into());
        }
    } else if accounts.iter().any(|account| account.uid == incoming.uid) {
        return Err("本次账号 UID 与已有本地索引冲突，未覆盖账号".into());
    }
    Ok(found)
}

fn merge_account(existing: &AccountConfig, incoming: &AccountConfig) -> AccountConfig {
    let mut saved = existing.clone();
    saved.workbuddy_uid.clone_from(&incoming.workbuddy_uid);
    saved.token.clone_from(&incoming.token);
    if !incoming.refresh_token.is_empty() {
        saved.refresh_token.clone_from(&incoming.refresh_token);
    }
    if incoming.expires_at.is_some() || incoming.token != existing.token {
        saved.expires_at = incoming.expires_at;
    }
    for (target, source) in [
        (&mut saved.email, &incoming.email),
        (&mut saved.enterprise_id, &incoming.enterprise_id),
        (&mut saved.enterprise_name, &incoming.enterprise_name),
        (&mut saved.domain, &incoming.domain),
    ] {
        if !source.is_empty() {
            target.clone_from(source);
        }
    }
    if saved.nickname.trim().is_empty() {
        saved.nickname.clone_from(&incoming.nickname);
    }
    if incoming.login_ip.is_some() {
        saved.login_ip.clone_from(&incoming.login_ip);
    }
    if incoming.import_ip.is_some() {
        saved.import_ip.clone_from(&incoming.import_ip);
    }
    saved.realm.clone_from(&incoming.realm);
    saved
}

/// Only explicit login observations qualify; generic ip/client_ip/remote_addr and
/// gateway request logs do not identify the account's last login address.
pub(crate) fn observed_login_ip(value: &Value) -> Option<String> {
    let keys = [
        "loginIp",
        "loginIP",
        "login_ip",
        "lastLoginIp",
        "lastLoginIP",
        "last_login_ip",
    ];
    std::iter::once(value)
        .chain(["account", "auth"].iter().filter_map(|key| value.get(*key)))
        .find_map(|source| {
            keys.iter()
                .filter_map(|key| source.get(*key).and_then(Value::as_str))
                .find_map(|ip| ip.trim().parse::<IpAddr>().ok().map(|ip| ip.to_string()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use axum::{
        body::{to_bytes, Body},
        extract::Request,
        http::{HeaderMap, Method, StatusCode},
        response::Response,
        routing::any,
        Router,
    };
    use std::collections::VecDeque;

    #[derive(Clone)]
    struct RequestSeen {
        method: Method,
        path: String,
        query: String,
        headers: HeaderMap,
        body: Vec<u8>,
    }
    struct Replies {
        state: Value,
        tokens: VecDeque<Value>,
        account: Value,
        import_ip: Value,
        status: StatusCode,
        account_delay_ms: u64,
        import_ip_delay_ms: u64,
        seen: Vec<RequestSeen>,
    }
    impl Default for Replies {
        fn default() -> Self {
            Self {
                state: json!({"code":0,"data":{"state":"opaque/state?with&symbols", "authUrl":"https://www.codebuddy.cn/login?state=fake"}}),
                tokens: VecDeque::from([
                    json!({"code":0,"data":{"accessToken":"new-access","refreshToken":"new-refresh","expiresIn":3600,"domain":"test-domain"}}),
                ]),
                account: json!({"code":0,"data":{"uid":"official-login-user","nickname":"Browser Account","loginIp":"203.0.113.42"}}),
                import_ip: json!({"ip":"203.0.113.90"}),
                status: StatusCode::OK,
                account_delay_ms: 0,
                import_ip_delay_ms: 0,
                seen: Vec::new(),
            }
        }
    }

    async fn service(
        replies: Replies,
        accounts: Vec<AccountConfig>,
    ) -> (
        Arc<Service>,
        Arc<Mutex<Replies>>,
        tokio::task::JoinHandle<()>,
    ) {
        let replies = Arc::new(Mutex::new(replies));
        let response_state = replies.clone();
        let app = Router::new().fallback(any(move |request: Request| {
            let response_state = response_state.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = to_bytes(body, 8192).await.unwrap().to_vec();
                let (status, response, delay) = {
                    let mut state = response_state.lock().unwrap();
                    let path = parts.uri.path().to_owned();
                    state.seen.push(RequestSeen {
                        method: parts.method,
                        path: path.clone(),
                        query: parts.uri.query().unwrap_or("").into(),
                        headers: parts.headers,
                        body,
                    });
                    let response = match path.as_str() {
                        "/v2/plugin/auth/state" => state.state.clone(),
                        "/v2/plugin/auth/token" => {
                            if state.tokens.len() > 1 {
                                state.tokens.pop_front().unwrap()
                            } else {
                                state.tokens.front().cloned().unwrap_or(Value::Null)
                            }
                        }
                        "/v2/plugin/login/account" => state.account.clone(),
                        "/import-ip" => state.import_ip.clone(),
                        _ => json!({"code":999,"msg":"unexpected request"}),
                    };
                    (
                        state.status,
                        response,
                        if path.ends_with("/account") {
                            state.account_delay_ms
                        } else if path == "/import-ip" {
                            state.import_ip_delay_ms
                        } else {
                            0
                        },
                    )
                };
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(response.to_string()))
                    .unwrap()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let cfg = Arc::new(tokio::sync::RwLock::new(AppConfig {
            accounts,
            ..Default::default()
        }));
        let mut service = Service::new(cfg, Arc::new(Mutex::new(HashSet::new())));
        let mutable = Arc::get_mut(&mut service).unwrap();
        mutable.base_cn = base.clone();
        mutable.import_ip = crate::import_ip::Observer::at(format!("{base}/import-ip"));
        mutable.base_global = base;
        let dir = std::env::temp_dir().join(format!(
            "workbuddy-browser-auth-test-{}",
            uuid::Uuid::new_v4()
        ));
        mutable.config_path = dir.join("config.json");
        (service, replies, server)
    }

    fn legacy_account() -> AccountConfig {
        serde_json::from_value(json!({"uid":"acct-preserve-history","nickname":"User's label","token":"new-access","refresh_token":"existing-refresh",
            "realm":"cn","enabled":false,"paused":true,"email":"retained@example.test","enterprise_id":"retained-enterprise",
            "domain":"retained-domain","login_ip":"2001:db8::8"})).unwrap()
    }

    #[tokio::test]
    async fn browser_flow_polls_then_persists_real_identity_and_keeps_secrets_backend_only() {
        for realm in ["cn", "global"] {
            let mut replies = Replies::default();
            replies
                .tokens
                .push_front(json!({"code":10001,"msg":"login ing","data":null}));
            let (service, replies, server) = service(replies, vec![]).await;
            let start = service.start(realm).await.unwrap();
            assert_eq!(start.realm, realm);
            assert_eq!(start.poll_interval_ms, 3000);
            assert!(!start.session_id.contains("opaque"));
            assert_eq!(
                service.poll(&start.session_id).await.unwrap().status,
                "pending"
            );
            let result = service.poll(&start.session_id).await.unwrap();
            assert_eq!(result.status, "completed");
            assert_eq!(result.uid.as_deref(), Some("official-login-user"));
            let safe = serde_json::to_string(&result).unwrap();
            assert!(!safe.contains("new-access"));
            assert!(!safe.contains("new-refresh"));
            let completed_again = service.poll(&start.session_id).await.unwrap();
            assert_eq!(completed_again.status, "completed");
            let cfg = service.cfg.read().await;
            assert_eq!(cfg.accounts.len(), 1);
            let account = &cfg.accounts[0];
            assert_eq!(account.official_uid(), Some("official-login-user"));
            assert_eq!(account.token, "new-access");
            assert_eq!(account.refresh_token, "new-refresh");
            assert_eq!(account.login_ip.as_deref(), Some("203.0.113.42"));
            assert_eq!(account.realm, realm);
            assert!(account.expires_at.unwrap() > now_ms());
            let saved: AppConfig =
                serde_json::from_str(&std::fs::read_to_string(&service.config_path).unwrap())
                    .unwrap();
            assert_eq!(saved.accounts[0].token, "new-access");
            drop(cfg);
            server.abort();
            let replies = replies.lock().unwrap();
            assert!(!replies.seen.iter().any(|request| request.path == "/import-ip"));
            assert_eq!(
                replies.seen.len(),
                4,
                "completed polls must not repeat upstream login"
            );
            let origin = if realm == "global" {
                "https://www.workbuddy.ai"
            } else {
                "https://www.codebuddy.cn"
            };
            for request in &replies.seen {
                assert_eq!(request.headers["origin"], origin);
                assert_eq!(request.headers["user-agent"], LOGIN_UA);
                assert!(!request.headers.contains_key("x-refresh-token"));
            }
            assert_eq!(replies.seen[0].method, Method::POST);
            assert_eq!(replies.seen[0].body, b"{}");
            assert_eq!(replies.seen[0].query, "platform=CLI");
            assert!(replies.seen[1].query.contains("%2F"));
            assert!(!replies.seen[1].headers.contains_key("authorization"));
            assert_eq!(
                replies.seen[3].headers["authorization"],
                "Bearer new-access"
            );
            assert!(!replies
                .seen
                .iter()
                .any(|request| request.path.contains("checkin") || request.path.contains("trial")));
        }
    }

    #[tokio::test]
    async fn real_11217_replies_keep_the_same_authorization_alive_until_completion() {
        for realm in ["cn", "global"] {
            let mut replies = Replies::default();
            replies
                .tokens
                .push_front(json!({"code":"11217","message":"尚未完成浏览器授权","data":null}));
            replies
                .tokens
                .push_front(json!({"code":11217,"msg":"login ing...","data":null}));
            let (service, replies, server) = service(replies, vec![]).await;
            let start = service.start(realm).await.unwrap();
            for _ in 0..2 {
                let result = service.poll(&start.session_id).await.unwrap();
                assert_eq!(result.status, "pending");
                assert!(result.uid.is_none());
                assert!(service.cfg.read().await.accounts.is_empty());
                assert!(
                    !service.config_path.exists(),
                    "waiting must not persist an account"
                );
                assert!(service
                    .sessions
                    .lock()
                    .unwrap()
                    .contains_key(&start.session_id));
            }
            assert_eq!(
                service.poll(&start.session_id).await.unwrap().status,
                "completed"
            );
            assert_eq!(
                service.poll(&start.session_id).await.unwrap().status,
                "completed"
            );
            assert_eq!(service.cfg.read().await.accounts.len(), 1);
            server.abort();
            let requests = &replies.lock().unwrap().seen;
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.path.ends_with("/state"))
                    .count(),
                1
            );
            let polls: Vec<_> = requests
                .iter()
                .filter(|request| request.path.ends_with("/token"))
                .collect();
            assert_eq!(polls.len(), 3);
            assert!(polls.iter().all(|request| request.query == polls[0].query));
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.path.ends_with("/account"))
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn waiting_code_is_only_progress_during_token_polling() {
        let mut replies = Replies::default();
        replies.state = json!({"code":11217,"msg":"login ing...","data":null});
        let (start_service, _, server) = service(replies, vec![]).await;
        assert!(start_service.start("cn").await.is_err());
        assert!(start_service.sessions.lock().unwrap().is_empty());
        server.abort();

        let mut replies = Replies::default();
        replies.account = json!({"code":11217,"msg":"login ing...","data":null});
        let (account_service, _, server) = service(replies, vec![]).await;
        let start = account_service.start("cn").await.unwrap();
        assert!(account_service.poll(&start.session_id).await.is_err());
        assert!(!account_service.config_path.exists());
        server.abort();

        let (token_service, replies, server) = service(Replies::default(), vec![]).await;
        let start = token_service.start("cn").await.unwrap();
        replies.lock().unwrap().status = StatusCode::UNAUTHORIZED;
        assert!(token_service.poll(&start.session_id).await.is_err());
        assert!(!token_service.config_path.exists());
        server.abort();
    }

    #[test]
    fn pending_variants_do_not_turn_other_business_failures_into_success() {
        for message in [
            "login ing",
            "login ing...",
            " Login ing… ",
            "等待授权……",
            "pending.",
        ] {
            assert!(
                is_pending(&json!({"code":10001,"msg":message})),
                "{message}"
            );
        }
        for value in [
            json!({"code":10001,"msg":"token expired"}),
            json!({"code":10001,"msg":"invalid state"}),
            json!({"code":11151,"msg":"request failed"}),
            json!({"code":99999,"msg":"unknown business error"}),
        ] {
            assert!(!is_pending(&value), "{value}");
        }
    }

    #[tokio::test]
    async fn browser_import_repairs_legacy_identity_and_preserves_metadata_and_refresh() {
        let mut replies = Replies::default();
        replies.tokens = VecDeque::from([
            json!({"code":0,"data":{"accessToken":"new-access","expiresIn":3600}}),
        ]);
        replies.account = json!({"code":0,"data":{"uid":"official-login-user","nickname":"New server nickname","client_ip":"192.0.2.5"}});
        let (service, _, server) = service(replies, vec![legacy_account()]).await;
        let start = service.start("cn").await.unwrap();
        let result = service.poll(&start.session_id).await.unwrap();
        server.abort();
        assert_eq!(result.uid.as_deref(), Some("acct-preserve-history"));
        let cfg = service.cfg.read().await;
        assert_eq!(cfg.accounts.len(), 1);
        let account = &cfg.accounts[0];
        assert_eq!(account.uid, "acct-preserve-history");
        assert_eq!(account.official_uid(), Some("official-login-user"));
        assert_eq!(account.nickname, "User's label");
        assert!(!account.enabled);
        assert!(account.paused);
        assert_eq!(account.refresh_token, "existing-refresh");
        assert_eq!(account.email, "retained@example.test");
        assert_eq!(account.enterprise_id, "retained-enterprise");
        assert_eq!(account.domain, "retained-domain");
        assert_eq!(account.login_ip.as_deref(), Some("2001:db8::8"));
    }

    #[tokio::test]
    async fn pending_renewal_keeps_authorized_credentials_for_retry() {
        let mut account = legacy_account();
        account.workbuddy_uid = Some("official-login-user".into());
        account.token = "old-access".into();
        let (service, replies, server) = service(Replies::default(), vec![account]).await;
        let start = service.start("cn").await.unwrap();
        let refresh =
            AccountRefresh::start(&service.account_refreshes, "acct-preserve-history").unwrap();
        assert_eq!(
            service.poll(&start.session_id).await.unwrap().status,
            "pending"
        );
        assert_eq!(service.cfg.read().await.accounts[0].token, "old-access");
        drop(refresh);
        assert_eq!(
            service.poll(&start.session_id).await.unwrap().status,
            "completed"
        );
        server.abort();
        assert_eq!(service.cfg.read().await.accounts[0].token, "new-access");
        assert_eq!(
            replies
                .lock()
                .unwrap()
                .seen
                .iter()
                .filter(|request| request.path.ends_with("/token"))
                .count(),
            1
        );
    }

    #[test]
    fn a_new_login_does_not_inherit_the_old_tokens_expiration() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let now = now_ms();
        let mut existing = legacy_account();
        existing.token = "expired-old-access".into();
        existing.expires_at = Some(now - 60_000);
        let token = parse_token(&json!({"accessToken":"new-opaque-access"}), now).unwrap();
        let incoming = parse_account(&json!({"uid":"official-login-user"}), &token, "cn").unwrap();
        let saved = merge_account(&existing, &incoming);
        assert_eq!(saved.uid, existing.uid);
        assert_eq!(saved.token, "new-opaque-access");
        assert_eq!(saved.refresh_token, existing.refresh_token);
        assert_eq!(saved.expires_at, None);
        assert_eq!(crate::renewal::token_expiry_ms(&saved), None);

        let exp = now / 1000 + 3600;
        let payload = URL_SAFE_NO_PAD.encode(json!({"exp":exp}).to_string());
        let access = format!("mock.{payload}.signature");
        assert_eq!(
            parse_token(&json!({"accessToken":access}), now)
                .unwrap()
                .expires_at,
            Some(exp * 1000)
        );
        let payload = URL_SAFE_NO_PAD.encode(json!({"exp":now / 1000 - 60}).to_string());
        assert!(parse_token(
            &json!({"accessToken":format!("mock.{payload}.signature")}),
            now
        )
        .is_err());
    }

    #[tokio::test]
    async fn cancel_and_expiration_prevent_late_imports_and_duplicate_polls_are_bounded() {
        let replies = Replies {
            account_delay_ms: 100,
            ..Default::default()
        };
        let (service, replies, server) = service(replies, vec![]).await;
        let start = service.start("cn").await.unwrap();
        let poll_service = service.clone();
        let id = start.session_id.clone();
        let poll = tokio::spawn(async move { poll_service.poll(&id).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if replies
                    .lock()
                    .unwrap()
                    .seen
                    .iter()
                    .any(|request| request.path.ends_with("/account"))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            service.poll(&start.session_id).await.unwrap().status,
            "pending"
        );
        service.cancel(&start.session_id).unwrap();
        assert!(poll.await.unwrap().is_err());
        assert!(service.cfg.read().await.accounts.is_empty());
        assert!(!service.config_path.exists());
        let expired = service.start("cn").await.unwrap();
        {
            let mut sessions = service.sessions.lock().unwrap();
            Arc::get_mut(sessions.get_mut(&expired.session_id).unwrap())
                .unwrap()
                .expires_at = now_ms() - 1;
        }
        assert_eq!(
            service.poll(&expired.session_id).await.unwrap().status,
            "expired"
        );
        server.abort();
    }

    #[tokio::test]
    async fn business_and_protocol_failures_never_create_accounts_or_report_success() {
        for token in [
            json!({"code":10001,"msg":"token expired"}),
            json!({"code":0,"data":{}}),
            json!({"code":0,"data":{"accessToken":"old-token","expiresAt":1}}),
        ] {
            let mut replies = Replies::default();
            replies.tokens = VecDeque::from([token]);
            let (service, _, server) = service(replies, vec![]).await;
            let start = service.start("cn").await.unwrap();
            assert!(service.poll(&start.session_id).await.is_err());
            server.abort();
            assert!(service.cfg.read().await.accounts.is_empty());
            assert!(!service.config_path.exists());
        }
        let replies = Replies {
            account: json!({"code":0,"data":{"uid":"acct-not-real"}}),
            ..Default::default()
        };
        let (service, _, server) = service(replies, vec![]).await;
        let start = service.start("cn").await.unwrap();
        assert!(service.poll(&start.session_id).await.is_err());
        server.abort();
        assert!(service.cfg.read().await.accounts.is_empty());
    }

    #[tokio::test]
    async fn start_rejects_untrusted_authorization_links_and_limits_sessions() {
        for url in [
            "http://www.codebuddy.cn/login",
            "https://www.codebuddy.cn.evil.test/",
            "file:///tmp/login",
            "https://user:pass@www.codebuddy.cn/",
            "https://www.codebuddy.cn:8443/",
        ] {
            let replies = Replies {
                state: json!({"code":0,"data":{"state":"fake","authUrl":url}}),
                ..Default::default()
            };
            let (service, _, server) = service(replies, vec![]).await;
            assert!(service.start("cn").await.is_err());
            server.abort();
            assert!(service.sessions.lock().unwrap().is_empty());
        }
        let (service, replies, server) = service(Replies::default(), vec![]).await;
        assert!(service.start("invalid").await.is_err());
        assert!(replies.lock().unwrap().seen.is_empty());
        let mut first = String::new();
        for index in 0..MAX_SESSIONS {
            let result = service.start("cn").await.unwrap();
            if index == 0 {
                first = result.session_id;
            }
        }
        assert!(service.start("cn").await.is_err());
        service.cancel(&first).unwrap();
        assert!(service.start("cn").await.is_ok());
        server.abort();
    }

    #[tokio::test]
    async fn completed_logins_do_not_consume_active_slots_and_result_cache_is_bounded() {
        let (service, _, server) = service(Replies::default(), vec![]).await;
        let mut latest = String::new();
        for _ in 0..MAX_CACHED_SESSIONS + 5 {
            let start = service.start("cn").await.unwrap();
            assert_eq!(
                service.poll(&start.session_id).await.unwrap().status,
                "completed"
            );
            latest = start.session_id;
            assert!(service.sessions.lock().unwrap().len() <= MAX_CACHED_SESSIONS);
        }
        assert_eq!(service.poll(&latest).await.unwrap().status, "completed");
        for _ in 0..MAX_SESSIONS {
            service.start("cn").await.unwrap();
        }
        assert!(service.start("cn").await.is_err());
        assert!(service.sessions.lock().unwrap().len() <= MAX_CACHED_SESSIONS);
        server.abort();
    }

    #[test]
    fn login_ip_accepts_only_explicit_valid_login_observations() {
        assert_eq!(
            observed_login_ip(&json!({"account":{"lastLoginIP":" 2001:db8::1 "}})).as_deref(),
            Some("2001:db8::1")
        );
        assert_eq!(
            observed_login_ip(&json!({"auth":{"login_ip":"203.0.113.10"}})).as_deref(),
            Some("203.0.113.10")
        );
        assert!(observed_login_ip(&json!({"loginIp":"not an IP", "client_ip":"203.0.113.5", "ip":"203.0.113.6", "remoteAddr":"203.0.113.7"})).is_none());
    }

    #[tokio::test]
    async fn missing_login_ip_collects_separate_egress_and_lookup_failure_does_not_block_import() {
        for realm in ["cn", "global"] {
            for body in [json!({"ip":"203.0.113.90"}), json!({"error":"unavailable"})] {
                let replies = Replies {
                    account: json!({"code":0,"data":{"uid":"official-login-user"}}),
                    import_ip: body.clone(),
                    ..Default::default()
                };
                let (service, replies, server) = service(replies, vec![]).await;
                let start = service.start(realm).await.unwrap();
                assert_eq!(service.poll(&start.session_id).await.unwrap().status, "completed");
                assert_eq!(service.poll(&start.session_id).await.unwrap().status, "completed");
                let cfg = service.cfg.read().await;
                assert!(cfg.accounts[0].login_ip.is_none());
                assert_eq!(cfg.accounts[0].import_ip.as_ref().map(|ip| ip.ip.as_str()), body["ip"].as_str());
                let saved: AppConfig = serde_json::from_str(&std::fs::read_to_string(&service.config_path).unwrap()).unwrap();
                assert_eq!(saved.accounts[0].import_ip, cfg.accounts[0].import_ip);
                let replies = replies.lock().unwrap();
                let seen: Vec<_> = replies.seen.iter().filter(|r| r.path == "/import-ip").collect();
                assert_eq!(seen.len(), 1);
                assert!(!seen[0].headers.contains_key("authorization"));
                assert!(!seen[0].headers.contains_key("cookie"));
                assert!(seen[0].query.is_empty());
                server.abort();
            }
        }
    }

    #[test]
    fn reauthorization_preserves_official_ip_and_previous_observation_when_missing() {
        let mut existing = legacy_account();
        existing.import_ip = Some(crate::import_ip::ImportIp { ip: "203.0.113.80".into(), observed_at: 100 });
        let token = parse_token(&json!({"accessToken":"new-access"}), now_ms()).unwrap();
        let mut incoming = parse_account(&json!({"uid":"official-login-user"}), &token, "cn").unwrap();
        let saved = merge_account(&existing, &incoming);
        assert_eq!(saved.login_ip, existing.login_ip);
        assert_eq!(saved.import_ip, existing.import_ip);
        incoming.import_ip = Some(crate::import_ip::ImportIp { ip: "203.0.113.90".into(), observed_at: 200 });
        let saved = merge_account(&existing, &incoming);
        assert_eq!(saved.login_ip, existing.login_ip);
        assert_eq!(saved.import_ip, incoming.import_ip);
    }

    #[tokio::test]
    async fn cancelling_during_ip_lookup_prevents_late_account_commit() {
        let replies = Replies {
            account: json!({"code":0,"data":{"uid":"official-login-user"}}),
            import_ip_delay_ms: 200,
            ..Default::default()
        };
        let (service, replies, server) = service(replies, vec![]).await;
        let start = service.start("cn").await.unwrap();
        let poll_service = service.clone();
        let id = start.session_id.clone();
        let poll = tokio::spawn(async move { poll_service.poll(&id).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if replies.lock().unwrap().seen.iter().any(|r| r.path == "/import-ip") { break; }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        }).await.unwrap();
        service.cancel(&start.session_id).unwrap();
        assert!(poll.await.unwrap().is_err());
        assert!(service.cfg.read().await.accounts.is_empty());
        assert!(!service.config_path.exists());
        server.abort();
    }
}
