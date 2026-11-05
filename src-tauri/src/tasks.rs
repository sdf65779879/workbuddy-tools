//! 任务中心：签到 / 成长任务扫描 / 一键完成（接受全部任务 + 领取达标奖励 + 领礼包/补偿）。
//! 上游端点（来自 codebuddy_sub2api 实测口径）：
//!   POST billing host /v2/billing/meter/daily-checkin (global: /billing first)
//!   GET  /v2/activity/growth/tasks
//!   POST /v2/activity/growth/tasks/accept          {"task_codes":[...]}
//!   POST /v2/activity/growth/tasks/reward/claim    {"task_code":"..."}
//!   POST /billing/meter/claim-gift / claim-compensation

use crate::{balances, config::AccountConfig};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    pub task_code: String,
    pub title: String,
    pub description: String,
    pub task_desc: String,
    pub credit: i64,
    pub energy: i64,
    pub has_reward: bool,
    pub task_type: String,
    pub locked: bool,
    pub target: i64,
    pub current: i64,
    pub accept_status: String,
    pub status: String,
    pub claimed: bool,
    pub claimable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskRunItem {
    pub uid: String,
    pub nickname: String,
    pub checkin: String,
    pub accepted: i64,
    pub claimed: i64,
    pub credit: i64,
    pub energy: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskRunResult {
    pub items: Vec<TaskRunItem>,
    pub total_credit: i64,
    pub total_energy: i64,
}

fn data_of(resp: &serde_json::Value) -> serde_json::Value {
    match resp.get("data") {
        Some(d) => d.clone(),
        None => resp.clone(),
    }
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (status, text) = post_response(client, url, token, body).await?;
    parse_api_response(status, &text)
}

async fn post_response(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    body: &serde_json::Value,
) -> Result<(reqwest::StatusCode, String), String> {
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    Ok((status, text))
}

async fn get_json(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<serde_json::Value, String> {
    let resp = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    parse_api_response(status, &text)
}

fn business_code(value: &serde_json::Value) -> Result<Option<i64>, String> {
    value
        .get("code")
        .map(|code| {
            code.as_i64()
                .or_else(|| code.as_str()?.parse().ok())
                .ok_or_else(|| "响应的业务状态码格式无效".to_string())
        })
        .transpose()
}

fn parse_api_response(
    status: reqwest::StatusCode,
    text: &str,
) -> Result<serde_json::Value, String> {
    if !status.is_success() {
        // Keep the structured diagnostic, rather than hiding every rejection behind
        // "HTTP 400". Never include a raw response body (e.g. a proxy HTML page).
        let detail = serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|value| {
                let code = business_code(&value).ok().flatten();
                let message = value
                    .get("msg")
                    .or_else(|| value.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match (code, message.is_empty()) {
                    (Some(code), _) => Some(format!("（code={code}）: {}", truncate(message, 200))),
                    (None, false) => Some(format!(": {}", truncate(message, 200))),
                    _ => None,
                }
            })
            .unwrap_or_default();
        return Err(format!("HTTP {status}{detail}"));
    }
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "接口返回了无效 JSON".to_string())?;
    if let Some(code) = business_code(&value)? {
        if code != 0 && code != 200 {
            let message = value
                .get("msg")
                .or_else(|| value.get("message"))
                .and_then(|value| value.as_str())
                .unwrap_or("请求失败");
            return Err(format!(
                "业务错误（code={code}）: {}",
                truncate(message, 200)
            ));
        }
    }
    Ok(value)
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn i64_of(v: &serde_json::Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn str_of(v: &serde_json::Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// 签到。
pub async fn checkin(
    client: &reqwest::Client,
    configured_base: &str,
    account: &AccountConfig,
) -> Result<String, String> {
    let base = balances::billing_base(configured_base, &account.realm)?;
    let paths: &[&str] = if account.realm == "global" {
        &[
            "/billing/meter/daily-checkin",
            "/v2/billing/meter/daily-checkin",
        ]
    } else {
        &["/v2/billing/meter/daily-checkin"]
    };
    for (index, path) in paths.iter().enumerate() {
        let response = balances::billing_post(
            client,
            &format!("{base}{path}"),
            account,
            &serde_json::json!({}),
        )
        .send()
        .await
        .map_err(|error| format!("签到请求失败: {error}"))?;
        let status = response.status();
        // Only a missing global endpoint permits a second request. All other replies
        // are classified directly, including the billing API's HTTP 400 "already" reply.
        if status == reqwest::StatusCode::NOT_FOUND && index + 1 < paths.len() {
            continue;
        }
        let text = response.text().await.map_err(|error| error.to_string())?;
        return parse_checkin_response(status, &text);
    }
    Err("签到接口不存在".into())
}

fn parse_checkin_response(status: reqwest::StatusCode, text: &str) -> Result<String, String> {
    // Classify idempotent success using the structured billing reply, never an error string.
    // Billing reports an already completed check-in as HTTP 400. Keep authentication,
    // server and unrelated business failures retryable even if their body mentions check-in.
    if status.is_success() || status == reqwest::StatusCode::BAD_REQUEST {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
            let code = business_code(&value)?;
            let message = value
                .get("msg")
                .or_else(|| value.get("message"))
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if matches!(code, Some(10001 | 14001))
                && matches!(
                    message.as_str(),
                    "已签到"
                        | "今日已签到"
                        | "今天已签到"
                        | "今日已签到，请勿重复签到"
                        | "今天已签到，请勿重复签到"
                        | "今日已签到，请明天再来"
                        | "今天已签到，请明天再来"
                        | "already checked in"
                        | "already checked in today"
                        | "already signed in today"
                )
            {
                return Ok("already".to_string());
            }
        }
    }
    let value = parse_api_response(status, text)?;
    if business_code(&value)?.is_none() {
        return Err("签到响应缺少明确的业务状态码，未确认签到成功".into());
    }
    Ok("ok".to_string())
}

/// 拉取成长任务列表。
pub async fn list_tasks(
    client: &reqwest::Client,
    base: &str,
    token: &str,
) -> Result<Vec<TaskView>, String> {
    let url = format!("{}/v2/activity/growth/tasks", base.trim_end_matches('/'));
    let resp = get_json(client, &url, token).await?;
    let data = data_of(&resp);
    let arr = data
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "响应缺少 tasks 数组".to_string())?;

    let mut out = Vec::new();
    for t in arr {
        let (current, target) = match t.get("progress") {
            Some(p) => (i64_of(&p["current"]), i64_of(&p["target"])),
            None => (i64_of(&t["current"]), i64_of(&t["target"])),
        };
        let accept_status = str_of(&t["accept_status"]);
        let claimed = accept_status == "claimed"
            || t.get("claimed").and_then(|v| v.as_bool()).unwrap_or(false);
        let target_reached = target > 0 && current >= target;
        out.push(TaskView {
            task_code: str_of(&t["task_code"]),
            title: str_of(&t["title"]),
            description: str_of(&t["description"]),
            task_desc: str_of(&t["task_desc"]),
            credit: i64_of(&t["reward_credit"]),
            energy: i64_of(&t["reward_energy"]),
            has_reward: t
                .get("has_reward")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            task_type: str_of(&t["task_type"]),
            locked: t.get("locked").and_then(|v| v.as_bool()).unwrap_or(false),
            target,
            current,
            accept_status: accept_status.clone(),
            status: str_of(&t["status"]),
            claimed,
            claimable: target_reached && !claimed,
        });
    }
    Ok(out)
}

/// 接受任务（报名，幂等）。
pub async fn accept_tasks(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    codes: &[String],
) -> Result<(), String> {
    if codes.is_empty() {
        return Ok(());
    }
    let url = format!(
        "{}/v2/activity/growth/tasks/accept",
        base.trim_end_matches('/')
    );
    post_json(
        client,
        &url,
        token,
        &serde_json::json!({ "task_codes": codes }),
    )
    .await
    .map(|_| ())
}

/// 领取任务奖励。返回 (credit, energy)。
pub async fn claim_reward(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    code: &str,
) -> Result<(i64, i64), String> {
    let url = format!(
        "{}/v2/activity/growth/tasks/reward/claim",
        base.trim_end_matches('/')
    );
    let resp = post_json(
        client,
        &url,
        token,
        &serde_json::json!({ "task_code": code }),
    )
    .await?;
    let data = data_of(&resp);
    Ok((i64_of(&data["credit"]), i64_of(&data["energy"])))
}

async fn try_claim_optional_benefit(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<(), String> {
    let (status, text) = post_response(client, url, token, &serde_json::json!({})).await?;
    // These unconditional probes have no eligibility information: a missing endpoint
    // or billing rejection can simply mean there is no gift/compensation to claim.
    // Surface definite transport/authentication/service failures without treating
    // normal ineligibility as a failed task.
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED
            | reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error()
    {
        parse_api_response(status, &text)?;
    }
    Ok(())
}

/// 对单个账号执行「一键完成」：签到 → 拉任务 → 接受全部未接受 → 领取全部达标未领 → 领礼包/补偿。
pub async fn run_account(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> TaskRunItem {
    let token = account.token.trim();
    let mut item = TaskRunItem {
        uid: account.uid.clone(),
        nickname: account.nickname.clone(),
        checkin: String::new(),
        accepted: 0,
        claimed: 0,
        credit: 0,
        energy: 0,
        error: None,
    };
    let mut errors = Vec::new();

    match checkin(client, base, account).await {
        Ok(r) => item.checkin = r,
        Err(e) => errors.push(format!("签到失败: {e}")),
    }

    match list_tasks(client, base, token).await {
        Ok(tasks) => {
            let to_accept: Vec<String> = tasks
                .iter()
                .filter(|t| !t.locked && t.accept_status == "not_accepted")
                .map(|t| t.task_code.clone())
                .collect();
            if let Err(e) = accept_tasks(client, base, token, &to_accept).await {
                errors.push(format!("接受任务失败: {e}"));
            } else {
                item.accepted = to_accept.len() as i64;
            }

            for t in tasks.iter().filter(|t| t.claimable) {
                match claim_reward(client, base, token, &t.task_code).await {
                    Ok((c, e)) => {
                        item.claimed += 1;
                        item.credit += c;
                        item.energy += e;
                    }
                    Err(e) => errors.push(format!("领取任务奖励失败（{}）: {e}", t.task_code)),
                }
            }
        }
        Err(e) => errors.push(format!("任务列表失败: {e}")),
    }

    let base = base.trim_end_matches('/');
    for (path, label) in [
        ("claim-gift", "领取礼包失败"),
        ("claim-compensation", "领取补偿失败"),
    ] {
        if let Err(e) =
            try_claim_optional_benefit(client, &format!("{base}/billing/meter/{path}"), token).await
        {
            errors.push(format!("{label}: {e}"));
        }
    }
    if !errors.is_empty() {
        item.error = Some(errors.join("；"));
    }

    item
}

pub async fn run_all(
    client: &reqwest::Client,
    base: &str,
    accounts: &[crate::config::AccountConfig],
) -> TaskRunResult {
    let mut items = Vec::new();
    let mut total_credit = 0i64;
    let mut total_energy = 0i64;
    for a in accounts.iter().filter(|a| a.enabled && !a.paused) {
        let item = run_account(client, base, a).await;
        total_credit += item.credit;
        total_energy += item.energy;
        items.push(item);
    }
    TaskRunResult {
        items,
        total_credit,
        total_energy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::to_bytes,
        extract::Request,
        http::{HeaderMap, Method, StatusCode},
        routing::{any, post},
        Router,
    };
    use std::sync::{Arc, Mutex};

    fn account() -> AccountConfig {
        serde_json::from_value(serde_json::json!({
            "uid": "acct-test-local", "workbuddy_uid": "official-test-uid",
            "nickname": "test", "token": "test-token", "realm": "cn",
            "enterprise_id": "test-enterprise", "domain": "test-domain",
            "enabled": true, "paused": false
        }))
        .unwrap()
    }

    struct RecordedRequest {
        method: Method,
        path: String,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    async fn billing_server(
        primary_status: StatusCode,
        primary_body: &'static str,
    ) -> (
        String,
        Arc<Mutex<Vec<RecordedRequest>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let seen = recorded.clone();
        let app = Router::new().fallback(any(move |request: Request| {
            let seen = seen.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = to_bytes(body, 8192).await.unwrap().to_vec();
                let primary = parts.uri.path() == "/billing/meter/daily-checkin";
                seen.lock().unwrap().push(RecordedRequest {
                    method: parts.method,
                    path: parts.uri.path().into(),
                    headers: parts.headers,
                    body,
                });
                if primary {
                    (primary_status, primary_body)
                } else {
                    (StatusCode::OK, r#"{"code":0,"data":{"tasks":[]}}"#)
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), recorded, server)
    }

    async fn mock_api(
        status: StatusCode,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().fallback(post(move || async move { (status, body) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), server)
    }

    #[tokio::test]
    async fn checkin_rejects_business_errors_even_with_http_200() {
        let client = reqwest::Client::new();
        for body in [
            r#"{"code":10001,"msg":"token expired","data":null}"#,
            r#"{"code":14001,"msg":"token expired","data":null}"#,
            r#"{"code":1000,"msg":"token expired"}"#,
            r#"{"code":10001,"msg":"token already expired"}"#,
        ] {
            let (base, server) = mock_api(StatusCode::OK, body).await;
            let result = checkin(&client, &base, &account()).await;
            server.abort();
            let error = result.expect_err("business failures must remain eligible for retry");
            assert!(error.contains("code="));
            assert!(error.contains("expired"));
        }
    }

    #[tokio::test]
    async fn checkin_accepts_explicit_success_and_known_already_checked_in_replies() {
        let client = reqwest::Client::new();
        for (status, body, expected) in [
            (StatusCode::OK, r#"{"code":0,"msg":"ok","data":null}"#, "ok"),
            (
                StatusCode::OK,
                r#"{"code":"200","message":"success","data":{}}"#,
                "ok",
            ),
            (
                StatusCode::OK,
                r#"{"code":10001,"msg":"今天已签到"}"#,
                "already",
            ),
            (
                StatusCode::OK,
                r#"{"code":14001,"msg":"今日已签到"}"#,
                "already",
            ),
            (
                StatusCode::OK,
                r#"{"code":14001,"message":"Already checked in today"}"#,
                "already",
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"code":10001,"msg":"今天已签到，请明天再来"}"#,
                "already",
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"code":"14001","message":"今日已签到，请明天再来"}"#,
                "already",
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"code":10001,"msg":"今天已签到"}"#,
                "already",
            ),
        ] {
            let (base, server) = mock_api(status, body).await;
            let result = checkin(&client, &base, &account()).await;
            server.abort();
            assert_eq!(result.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn checkin_does_not_infer_success_from_empty_malformed_or_http_error_replies() {
        let client = reqwest::Client::new();
        for (status, body) in [
            (StatusCode::OK, ""),
            (StatusCode::OK, "{}"),
            (StatusCode::OK, "null"),
            (StatusCode::OK, "already checked in today 1000"),
            (StatusCode::OK, r#"{"code":false,"msg":"今日已签到"}"#),
            (
                StatusCode::UNAUTHORIZED,
                r#"{"code":10001,"msg":"今日已签到"}"#,
            ),
            (
                StatusCode::FORBIDDEN,
                r#"{"code":10001,"msg":"今天已签到，请明天再来"}"#,
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"code":10001,"msg":"今天已签到，请明天再来"}"#,
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"code":10001,"msg":"invalid user id"}"#,
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"code":10001,"msg":"token expired"}"#,
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"code":0,"msg":"今天已签到，请明天再来"}"#,
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"code":99999,"msg":"今天已签到，请明天再来"}"#,
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"message":"今天已签到，请明天再来"}"#,
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"code":10001,"msg":"今天未签到，请明天再来"}"#,
            ),
        ] {
            let (base, server) = mock_api(status, body).await;
            let result = checkin(&client, &base, &account()).await;
            server.abort();
            assert!(
                result.is_err(),
                "unconfirmed reply must not mark the day complete"
            );
        }
    }

    #[tokio::test]
    async fn other_task_mutations_use_the_same_business_error_validation() {
        let (base, server) = mock_api(
            StatusCode::OK,
            r#"{"code":10001,"msg":"token expired","data":null}"#,
        )
        .await;
        let client = reqwest::Client::new();
        assert!(
            accept_tasks(&client, &base, "test-token", &["test-task".into()])
                .await
                .is_err()
        );
        assert!(claim_reward(&client, &base, "test-token", "test-task")
            .await
            .is_err());
        server.abort();
    }

    #[tokio::test]
    async fn checkin_sends_billing_protocol_and_official_identity() {
        let (base, recorded, server) = billing_server(StatusCode::OK, r#"{"code":0}"#).await;
        let result = checkin(&reqwest::Client::new(), &format!("{base}/"), &account()).await;
        server.abort();
        assert_eq!(result.unwrap(), "ok");
        let requests = recorded.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.path, "/v2/billing/meter/daily-checkin");
        assert_eq!(request.body, b"{}");
        assert_eq!(request.headers["authorization"], "Bearer test-token");
        assert_eq!(request.headers["accept"], "application/json");
        assert_eq!(request.headers["content-type"], "application/json");
        assert_eq!(request.headers["x-codebuddy-request"], "1");
        assert_eq!(request.headers["x-user-id"], "official-test-uid");
        assert_eq!(request.headers["x-enterprise-id"], "test-enterprise");
        assert_eq!(request.headers["x-tenant-id"], "test-enterprise");
        assert_eq!(request.headers["x-domain"], "test-domain");
        assert_eq!(request.headers["accept-language"], "zh-CN,zh;q=0.9");
        assert!(!request.headers.contains_key("x-refresh-token"));
    }

    #[tokio::test]
    async fn checkin_legacy_ids_use_valid_jwt_uid_or_omit_identity() {
        use base64::Engine;
        for jwt_uid in [Some("jwt-official-test-uid"), Some("acct-bad-id"), None] {
            let mut account = account();
            account.workbuddy_uid = None;
            if let Some(uid) = jwt_uid {
                let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(serde_json::json!({"uid": uid}).to_string());
                account.token = format!("fakeheader.{payload}.fakesignature");
            }
            let (base, recorded, server) = billing_server(StatusCode::OK, r#"{"code":0}"#).await;
            assert!(checkin(&reqwest::Client::new(), &base, &account)
                .await
                .is_ok());
            server.abort();
            let requests = recorded.lock().unwrap();
            let uid = requests[0]
                .headers
                .get("x-user-id")
                .map(|value| value.to_str().unwrap());
            assert_eq!(
                uid,
                jwt_uid.filter(|uid| AccountConfig::valid_workbuddy_uid(uid))
            );
        }
    }

    #[tokio::test]
    async fn checkin_global_prefers_unversioned_path_and_only_falls_back_on_404() {
        for (status, body, expected_count, success) in [
            (StatusCode::OK, r#"{"code":0}"#, 1, true),
            (StatusCode::NOT_FOUND, "not found", 2, true),
            (
                StatusCode::BAD_REQUEST,
                r#"{"code":10001,"msg":"今天已签到，请明天再来"}"#,
                1,
                true,
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"code":10001,"msg":"invalid user id"}"#,
                1,
                false,
            ),
            (
                StatusCode::OK,
                r#"{"code":10001,"msg":"token expired"}"#,
                1,
                false,
            ),
        ] {
            let mut account = account();
            account.realm = "global".into();
            let (base, recorded, server) = billing_server(status, body).await;
            let result = checkin(&reqwest::Client::new(), &base, &account).await;
            server.abort();
            assert_eq!(result.is_ok(), success);
            if status == StatusCode::BAD_REQUEST && !success {
                let error = result.unwrap_err();
                assert!(error.contains("400"));
                assert!(error.contains("code=10001"));
                assert!(error.contains("invalid user id"));
            }
            let requests = recorded.lock().unwrap();
            assert_eq!(requests.len(), expected_count);
            assert_eq!(requests[0].path, "/billing/meter/daily-checkin");
            assert_eq!(requests[0].headers["accept-language"], "en-US,en;q=0.9");
            if expected_count == 2 {
                assert_eq!(requests[1].path, "/v2/billing/meter/daily-checkin");
                assert_eq!(requests[1].headers["x-user-id"], "official-test-uid");
            }
        }
    }

    #[tokio::test]
    async fn task_single_and_batch_keep_official_checkin_identity() {
        let (base, recorded, server) = billing_server(StatusCode::OK, r#"{"code":0}"#).await;
        let client = reqwest::Client::new();
        let account = account();
        let single = run_account(&client, &base, &account).await;
        assert_eq!(single.uid, "acct-test-local");
        assert_eq!(single.checkin, "ok");
        let mut paused = account.clone();
        paused.paused = true;
        let batch = run_all(&client, &base, &[account, paused]).await;
        server.abort();
        assert_eq!(batch.items.len(), 1);
        assert_eq!(batch.items[0].uid, "acct-test-local");
        assert_eq!(batch.items[0].checkin, "ok");
        let requests = recorded.lock().unwrap();
        let checkins: Vec<_> = requests
            .iter()
            .filter(|request| request.path.ends_with("daily-checkin"))
            .collect();
        assert_eq!(checkins.len(), 2);
        assert!(checkins
            .iter()
            .all(|request| request.headers["x-user-id"] == "official-test-uid"));
    }

    #[tokio::test]
    async fn task_run_reports_each_failed_reward_and_keeps_successful_work() {
        let app = Router::new().fallback(any(|request: Request| async move {
            let path = request.uri().path().to_string();
            match path.as_str() {
                "/v2/billing/meter/daily-checkin" => (
                    StatusCode::BAD_REQUEST,
                    r#"{"code":10001,"msg":"今天已签到，请明天再来"}"#,
                ),
                "/v2/activity/growth/tasks" => (
                    StatusCode::OK,
                    r#"{"code":0,"data":{"tasks":[
                        {"task_code":"service-failure","target":1,"current":1,"accept_status":"not_accepted"},
                        {"task_code":"business-failure","target":1,"current":1,"accept_status":"accepted"},
                        {"task_code":"successful-reward","target":1,"current":1,"accept_status":"accepted"}
                    ]}}"#,
                ),
                "/v2/activity/growth/tasks/accept" => (
                    StatusCode::UNAUTHORIZED,
                    r#"{"code":10001,"msg":"token expired"}"#,
                ),
                "/v2/activity/growth/tasks/reward/claim" => {
                    let body = to_bytes(request.into_body(), 8192).await.unwrap();
                    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    match body["task_code"].as_str().unwrap() {
                        "service-failure" => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            r#"{"message":"reward service unavailable"}"#,
                        ),
                        "business-failure" => (
                            StatusCode::OK,
                            r#"{"code":10001,"message":"reward request rejected"}"#,
                        ),
                        "successful-reward" => (
                            StatusCode::OK,
                            r#"{"code":0,"data":{"credit":25,"energy":3}}"#,
                        ),
                        _ => panic!("unexpected task code"),
                    }
                }
                "/billing/meter/claim-gift" => (StatusCode::NOT_FOUND, "not found"),
                "/billing/meter/claim-compensation" => (
                    StatusCode::BAD_REQUEST,
                    r#"{"code":10001,"message":"no compensation available"}"#,
                ),
                _ => panic!("unexpected request: {path}"),
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let result = run_account(&reqwest::Client::new(), &base, &account()).await;
        server.abort();

        assert_eq!(result.checkin, "already");
        assert_eq!(result.accepted, 0);
        assert_eq!(result.claimed, 1);
        assert_eq!(result.credit, 25);
        assert_eq!(result.energy, 3);
        let error = result.error.expect("failed operations must be reported");
        assert!(error.contains("接受任务失败"), "{error}");
        assert!(error.contains("service-failure"), "{error}");
        assert!(error.contains("business-failure"), "{error}");
        assert!(error.contains("reward request rejected"), "{error}");
        assert!(!error.contains("领取礼包失败"), "{error}");
        assert!(!error.contains("领取补偿失败"), "{error}");
    }

    #[tokio::test]
    async fn task_run_keeps_earlier_errors_and_reports_definite_optional_benefit_failures() {
        let app = Router::new().fallback(any(|request: Request| async move {
            match request.uri().path() {
                "/v2/billing/meter/daily-checkin" => (
                    StatusCode::UNAUTHORIZED,
                    r#"{"code":10001,"msg":"token expired"}"#,
                ),
                "/v2/activity/growth/tasks" => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    r#"{"message":"task service unavailable"}"#,
                ),
                "/billing/meter/claim-gift" => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    r#"{"message":"gift service unavailable"}"#,
                ),
                "/billing/meter/claim-compensation" => {
                    (StatusCode::FORBIDDEN, r#"{"message":"access denied"}"#)
                }
                path => panic!("unexpected request: {path}"),
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let result = run_account(&reqwest::Client::new(), &base, &account()).await;
        server.abort();

        assert!(result.checkin.is_empty());
        assert_eq!(result.claimed, 0);
        let error = result
            .error
            .expect("all failed operations must be reported");
        for operation in ["签到失败", "任务列表失败", "领取礼包失败", "领取补偿失败"]
        {
            assert!(error.contains(operation), "{error}");
        }
    }
}
