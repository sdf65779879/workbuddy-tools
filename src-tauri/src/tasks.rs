//! 任务中心：签到 / 成长任务扫描 / 一键完成（接受全部任务 + 领取达标奖励 + 领礼包/补偿）。
//! 上游端点（来自 codebuddy_sub2api 实测口径）：
//!   POST billing host /v2/billing/meter/daily-checkin (global: /billing first)
//!   GET  /v2/activity/growth/tasks
//!   POST /v2/activity/growth/tasks/accept          {"task_codes":[...]}
//!   POST web host /activity/growth/tasks/{task_code}/claim (no body)
//!   POST /billing/meter/claim-gift / claim-compensation

use crate::{
    balances,
    config::{AccountConfig, AppConfig},
    growth_auto,
};
use futures_util::StreamExt;
use serde::Serialize;
use std::time::Duration;

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
    pub skipped: bool,
    pub skip_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskRunItem {
    pub uid: String,
    pub nickname: String,
    pub checkin: String,
    pub accepted: i64,
    pub completed: i64,
    pub skipped: i64,
    pub waiting: i64,
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

#[derive(Debug, Clone, Serialize)]
pub struct TaskProgress {
    pub task_code: Option<String>,
    pub status: String,
    pub message: String,
}

fn emit(
    progress: &(dyn Fn(TaskProgress) + Send + Sync),
    code: Option<&str>,
    status: &str,
    message: impl Into<String>,
) {
    progress(TaskProgress {
        task_code: code.map(str::to_owned),
        status: status.into(),
        message: message.into(),
    });
}

pub fn skip_reason(code: &str, title: &str) -> Option<String> {
    let compact: String = title.chars().filter(|c| !c.is_whitespace()).collect();
    let ignored = [
        "无畏契约拼豆专家",
        "中国电商运营专家",
        "商家宣传页与海报设计师",
        "直播电商教练",
        "抖音策略师",
    ];
    (ignored.iter().any(|name| compact.contains(name))
        || code == "wb_wechat_oa_subscribe_task"
        || (compact.contains("关注")
            && compact.contains("WorkBuddy")
            && compact.contains("公众号")))
    .then(|| "按设置自动忽略该任务".into())
}

pub(crate) async fn response_text(
    response: reqwest::Response,
) -> Result<(reqwest::StatusCode, String), String> {
    let status = response.status();
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "读取任务响应失败".to_string())?;
        if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
            return Err("任务响应过大".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((
        status,
        String::from_utf8(bytes).map_err(|_| "任务响应编码无效")?,
    ))
}

fn growth_request(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    account: &AccountConfig,
) -> reqwest::RequestBuilder {
    let mut req = client
        .request(method, url)
        .timeout(Duration::from_secs(25))
        .bearer_auth(account.token.trim())
        .header("User-Agent", "WorkBuddy/5.5.4")
        .header("X-CodeBuddy-Request", "1")
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    if let Some(uid) = balances::upstream_user_id(account) {
        req = req.header("X-User-Id", uid.as_ref());
    }
    if !account.enterprise_id.trim().is_empty() {
        req = req
            .header("X-Enterprise-Id", account.enterprise_id.trim())
            .header("X-Tenant-Id", account.enterprise_id.trim());
    }
    if !account.domain.trim().is_empty() {
        req = req.header("X-Domain", account.domain.trim());
    }
    req
}

pub(crate) fn growth_base(base: &str, realm: &str) -> Result<String, String> {
    let url = crate::gateway::request::chat_url(base, realm)?;
    Ok(url.trim_end_matches("/v2/chat/completions").into())
}

pub(crate) fn web_base(base: &str, realm: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(base).map_err(|_| "任务服务地址无效")?;
    if matches!(
        parsed.host_str(),
        Some(
            "copilot.tencent.com"
                | "www.codebuddy.cn"
                | "codebuddy.cn"
                | "www.workbuddy.cn"
                | "workbuddy.cn"
                | "www.workbuddy.ai"
                | "workbuddy.ai"
        )
    ) {
        Ok(if realm == "global" {
            "https://www.workbuddy.ai"
        } else {
            "https://www.workbuddy.cn"
        }
        .into())
    } else {
        Ok(base.trim_end_matches('/').into())
    }
}

#[cfg(test)]
fn token_account(token: &str) -> AccountConfig {
    serde_json::from_value(serde_json::json!({"uid":"local-account","nickname":"","token":token,"realm":"cn","enabled":true,"paused":false})).expect("token account")
}

fn data_of(resp: &serde_json::Value) -> serde_json::Value {
    match resp.get("data") {
        Some(d) => d.clone(),
        None => resp.clone(),
    }
}

async fn post_response(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    body: &serde_json::Value,
) -> Result<(reqwest::StatusCode, String), String> {
    let resp = client
        .post(url)
        .timeout(Duration::from_secs(25))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    response_text(resp).await
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

pub(crate) fn parse_api_response(
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
pub async fn list_tasks_for_account(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> Result<Vec<TaskView>, String> {
    let url = format!(
        "{}/v2/activity/growth/tasks",
        growth_base(base, &account.realm)?
    );
    let response = growth_request(client, reqwest::Method::GET, &url, account)
        .send()
        .await
        .map_err(|_| "任务列表请求失败或超时")?;
    let (status, text) = response_text(response).await?;
    let resp = parse_api_response(status, &text)?;
    let data = data_of(&resp);
    let arr = data
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "响应缺少 tasks 数组".to_string())?;

    let mut out = Vec::new();
    for t in arr {
        let (current, target) = match t.get("progress") {
            Some(p) if p.is_object() && (i64_of(&p["current"]) > 0 || i64_of(&p["target"]) > 0) => {
                (i64_of(&p["current"]), i64_of(&p["target"]))
            }
            _ => (i64_of(&t["current"]), i64_of(&t["target"])),
        };
        let accept_status = str_of(&t["accept_status"]);
        let claimed = accept_status == "claimed"
            || t.get("claimed").and_then(|v| v.as_bool()).unwrap_or(false);
        let target_reached = target > 0 && current >= target;
        let skip_reason = skip_reason(&str_of(&t["task_code"]), &str_of(&t["title"]));
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
            claimable: target_reached
                && !claimed
                && skip_reason.is_none()
                && !t["locked"].as_bool().unwrap_or(false),
            skipped: skip_reason.is_some(),
            skip_reason,
        });
    }
    Ok(out)
}

/// 接受任务（报名，幂等）。
#[cfg(test)]
pub async fn accept_tasks(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    codes: &[String],
) -> Result<(), String> {
    accept_tasks_for_account(client, base, &token_account(token), codes).await
}

async fn accept_tasks_for_account(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
    codes: &[String],
) -> Result<(), String> {
    if codes.is_empty() {
        return Ok(());
    }
    let url = format!(
        "{}/v2/activity/growth/tasks/accept",
        growth_base(base, &account.realm)?
    );
    let response = growth_request(client, reqwest::Method::POST, &url, account)
        .json(&serde_json::json!({"task_codes":codes}))
        .send()
        .await
        .map_err(|_| "接受任务请求失败或超时")?;
    let (status, text) = response_text(response).await?;
    let value = parse_api_response(status, &text)?;
    if !value.is_object() {
        return Err("接受任务响应无效".into());
    }
    Ok(())
}

#[cfg(test)]
pub async fn claim_reward(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    code: &str,
) -> Result<(i64, i64), String> {
    claim_reward_for_account(client, base, &token_account(token), code)
        .await
        .map(|(credit, energy, _)| (credit, energy))
}

async fn claim_reward_for_account(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
    code: &str,
) -> Result<(i64, i64, bool), String> {
    if code.trim().is_empty() {
        return Err("任务码为空".into());
    }
    let base = web_base(base, &account.realm)?;
    let mut url = reqwest::Url::parse(&format!("{base}/activity/growth/tasks/"))
        .map_err(|_| "领奖地址无效")?;
    url.path_segments_mut()
        .map_err(|_| "领奖地址无效")?
        .pop_if_empty()
        .push(code)
        .push("claim");
    let origin = if account.realm == "global" {
        "https://www.workbuddy.ai"
    } else {
        "https://www.workbuddy.cn"
    };
    let response = growth_request(client, reqwest::Method::POST, url.as_str(), account)
        .header("x-client-platform", "web")
        .header("Origin", origin)
        .header("Referer", format!("{origin}/profile/growth-center"))
        .send()
        .await
        .map_err(|_| "领奖请求失败或超时")?;
    let (status, text) = response_text(response).await?;
    let response = parse_api_response(status, &text)?;
    let data = data_of(&response);
    if data["already_claimed"].as_bool() == Some(true) {
        return Ok((0, 0, true));
    }
    if !data["credit"].is_number() && !data["energy"].is_number() {
        return Err("领奖响应缺少奖励结果".into());
    }
    Ok((i64_of(&data["credit"]), i64_of(&data["energy"]), false))
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

/// 真实对话动作仅在少数任务（夜猫子 black_cat 等）需要；其余可自动化任务的
/// 行为动作（chat_5 纯事件补报、桌面指纹事件链、专家召唤等）统一由
/// `crate::growth_auto` 实现（移植自 codebuddy_sub2api 实测口径）。

const CHAT_TASK_PROMPT: &str = "你好，请用一句话介绍你能帮助完成哪些工作。";

pub(crate) async fn real_conversation(
    client: &reqwest::Client,
    base: &str,
    cfg: &AppConfig,
    account: &AccountConfig,
    model: &str,
) -> Result<(), String> {
    // 别名 → 上游模型 id：与本地网关 translate.rs 的出站翻译同一口径，
    // 尊重用户自定义模型映射（config models）。
    let model = cfg.resolve_model(model);
    let url = crate::gateway::request::chat_url(base, &account.realm)?;
    let body = serde_json::json!({"model":model,"messages":[{"role":"user","content":CHAT_TASK_PROMPT}],"stream":true,"max_tokens":512});
    let response = crate::gateway::request::chat_post(client, &url, account, &body)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|_| "真实对话请求失败或超时")?;
    crate::gateway::probe::collect_chat_response(response)
        .await
        .map(|_| ())
        .map_err(|error| {
            // Sanitization of upstream text prevents credentials being copied into job logs.
            let error = error.replace(account.token.trim(), "[凭证已隐藏]");
            if account.refresh_token.is_empty() {
                error
            } else {
                error.replace(&account.refresh_token, "[凭证已隐藏]")
            }
        })
}

/// 对话活跃上报（chat_request_send 事件）：POST {billing 域}/v2/report。
/// 官方成长任务的对话计数以该上报为准——直接调用 chat completions 不会推进
/// 进度；官方客户端每次对话后都会补发这条事件（codebuddy_sub2api 实测口径）。
/// 全字段照抄官方客户端形状；userId 必填（=账号官方 uid），缺失会被服务端
/// 200 但静默丢弃。
pub(crate) async fn report_chat_activity(
    client: &reqwest::Client,
    configured_base: &str,
    account: &AccountConfig,
    conversation_id: &str,
    model_id: &str,
    model_name: &str,
) -> Result<(), String> {
    let uid = balances::upstream_user_id(account)
        .ok_or_else(|| "账号缺少官方用户 ID，无法上报对话活跃".to_string())?
        .into_owned();
    let now = crate::reqlog::now_ms();
    let event = serde_json::json!({
        "eventCode": "chat_request_send",
        "timestamp": now,
        "reportDelay": 0,
        "mode": "craft",
        "conversationId": conversation_id,
        "requestId": conversation_id,
        "inputLength": CHAT_TASK_PROMPT.chars().count(),
        "requestModelId": model_id,
        "requestModelName": model_name,
        "isPlan": false,
        "isAutoExecuteTerminal": false,
        "isAutoModify": false,
        "codebaseEnable": false,
        "maxToken": 0,
        "maxSteps": 0,
        "temperature": 0,
        "maxRetries": 0,
        "mentionContexts": [],
        "knowledgeId": [],
        "knowledgeName": [],
        "codebaseId": "",
        "mentionContextCount": 0,
        "command": "",
        "expertId": "",
        "recommendId": "",
        "skillId": "",
        "skillCount": 0,
        "totalCount": 0,
        "fileUri": "",
        "presentAt": now,
        "traceId": "",
        "rootRequestId": conversation_id,
        "parentConversationId": conversation_id,
        "agentName": "default",
        "agentType": "conversation",
        "userId": uid,
    });
    let base = balances::billing_base(configured_base, &account.realm)?;
    let response = balances::billing_post(
        client,
        &format!("{base}/v2/report"),
        account,
        &serde_json::json!([event]),
    )
    .send()
    .await
    .map_err(|_| "对话活跃上报请求失败或超时".to_string())?;
    let (status, text) = response_text(response).await?;
    parse_api_response(status, &text)?;
    Ok(())
}

#[cfg(test)]
pub async fn run_account(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> TaskRunItem {
    run_account_with_progress(client, base, &AppConfig::default(), account, &|_| {}).await
}

pub async fn run_account_with_progress(
    client: &reqwest::Client,
    base: &str,
    cfg: &AppConfig,
    account: &AccountConfig,
    progress: &(dyn Fn(TaskProgress) + Send + Sync),
) -> TaskRunItem {
    let clean = |text: String| {
        let mut cleaned = text;
        for secret in [account.token.trim(), account.refresh_token.trim()] {
            if !secret.is_empty() {
                cleaned = cleaned.replace(secret, "[凭证已隐藏]");
            }
        }
        cleaned
    };
    let safe_progress = |mut event: TaskProgress| {
        event.message = clean(event.message);
        progress(event);
    };
    let mut item = run_account_inner(client, base, cfg, account, &safe_progress).await;
    item.error = item.error.map(clean);
    item
}

async fn run_account_inner(
    client: &reqwest::Client,
    base: &str,
    cfg: &AppConfig,
    account: &AccountConfig,
    progress: &(dyn Fn(TaskProgress) + Send + Sync),
) -> TaskRunItem {
    let mut item = TaskRunItem {
        uid: account.uid.clone(),
        nickname: account.nickname.clone(),
        checkin: String::new(),
        accepted: 0,
        completed: 0,
        skipped: 0,
        waiting: 0,
        claimed: 0,
        credit: 0,
        energy: 0,
        error: None,
    };
    let mut errors = Vec::new();
    emit(progress, None, "running", "正在签到");
    match checkin(client, base, account).await {
        Ok(result) => {
            item.checkin = result;
            emit(progress, None, "completed", "签到已确认");
        }
        Err(error) => {
            emit(progress, None, "failed", format!("签到失败：{error}"));
            errors.push(format!("签到失败: {error}"));
        }
    }
    emit(progress, None, "running", "正在读取成长任务");
    match list_tasks_for_account(client, base, account).await {
        Ok(tasks) => {
            for mut task in tasks {
                let code = task.task_code.clone();
                let label = if task.title.is_empty() {
                    code.clone()
                } else {
                    task.title.clone()
                };
                if task.skipped {
                    item.skipped += 1;
                    emit(
                        progress,
                        Some(&code),
                        "skipped",
                        format!("已忽略「{label}」"),
                    );
                    continue;
                }
                if task.claimed {
                    emit(
                        progress,
                        Some(&code),
                        "completed",
                        format!("「{label}」已领取，无需重复执行"),
                    );
                    continue;
                }
                if task.locked {
                    item.waiting += 1;
                    emit(
                        progress,
                        Some(&code),
                        "waiting",
                        format!("「{label}」尚未解锁"),
                    );
                    continue;
                }
                if !matches!(task.accept_status.as_str(), "accepted" | "completed") {
                    emit(
                        progress,
                        Some(&code),
                        "running",
                        format!("正在接受「{label}」"),
                    );
                    match accept_tasks_for_account(client, base, account, &[code.clone()]).await {
                        Ok(()) => {
                            item.accepted += 1;
                        }
                        Err(error) => {
                            errors.push(format!("接受任务失败（{code}）: {error}"));
                            emit(
                                progress,
                                Some(&code),
                                "failed",
                                format!("「{label}」接受失败：{error}"),
                            );
                            if !task.claimable {
                                continue;
                            }
                        }
                    }
                    // Accept can unlock/complete tasks. Never claim based on a stale list.
                    match list_tasks_for_account(client, base, account).await {
                        Ok(updated) => {
                            if let Some(current) = updated.into_iter().find(|t| t.task_code == code)
                            {
                                task = current;
                            } else {
                                item.waiting += 1;
                                emit(
                                    progress,
                                    Some(&code),
                                    "waiting",
                                    format!("「{label}」接受后暂未出现在任务列表"),
                                );
                                continue;
                            }
                        }
                        Err(error) => {
                            errors.push(format!("任务回读失败（{code}）: {error}"));
                            emit(progress, Some(&code), "failed", "接受后回读任务失败");
                            continue;
                        }
                    }
                }
                if task.claimed {
                    continue;
                }
                if task.skipped || task.locked {
                    if task.skipped {
                        item.skipped += 1;
                    } else {
                        item.waiting += 1;
                    }
                    emit(
                        progress,
                        Some(&code),
                        "waiting",
                        "任务状态已改变，本轮停止执行",
                    );
                    continue;
                }
                if !task.claimable {
                    let hour = ((crate::reqlog::now_ms() / 3_600_000 + 8) % 24) as u8;
                    let mut acted = false;
                    if code == "black_cat" {
                        // 夜猫子：仅北京时间 23:00–08:00 窗口内执行真实对话。
                        if (8..23).contains(&hour) {
                            item.waiting += 1;
                            emit(
                                progress,
                                Some(&code),
                                "waiting",
                                "夜间任务仅在北京时间 23:00–08:00 执行",
                            );
                            continue;
                        }
                        emit(
                            progress,
                            Some(&code),
                            "running",
                            format!("正在为「{label}」执行真实对话"),
                        );
                        if let Err(error) =
                            real_conversation(client, base, cfg, account, "glm-5.2").await
                        {
                            errors.push(format!("任务对话失败（{code}）: {error}"));
                            emit(
                                progress,
                                Some(&code),
                                "failed",
                                format!("真实对话失败：{error}"),
                            );
                            continue;
                        }
                        acted = true;
                    } else if growth_auto::auto_action_supported(&code) {
                        emit(
                            progress,
                            Some(&code),
                            "running",
                            format!("正在执行「{label}」"),
                        );
                        match growth_auto::run_task_action(client, base, cfg, account, &task).await
                        {
                            Ok(message) => {
                                emit(progress, Some(&code), "running", message);
                                acted = true;
                            }
                            Err(error) => {
                                errors.push(format!("任务执行失败（{code}）: {error}"));
                                emit(
                                    progress,
                                    Some(&code),
                                    "failed",
                                    format!("执行失败：{error}"),
                                );
                                continue;
                            }
                        }
                    }
                    if acted {
                        // 给上游事件处理留时间，再回读进度确认是否推进/达标。
                        tokio::time::sleep(Duration::from_millis(1200)).await;
                        let previous = task.current;
                        for read in 0..4 {
                            if read > 0 {
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                            match list_tasks_for_account(client, base, account).await {
                                Ok(updated) => {
                                    if let Some(updated) =
                                        updated.into_iter().find(|t| t.task_code == code)
                                    {
                                        task = updated;
                                        if task.current > previous
                                            || task.claimable
                                            || task.claimed
                                            || task.locked
                                            || task.skipped
                                        {
                                            break;
                                        }
                                    }
                                }
                                Err(error) => {
                                    errors.push(format!("任务回读失败（{code}）: {error}"));
                                    emit(progress, Some(&code), "failed", "执行后回读进度失败");
                                    break;
                                }
                            }
                        }
                        if task.claimable
                            || task.claimed
                            || task.locked
                            || task.skipped
                            || task.current > previous
                        {
                            // 状态有推进，落入下方领奖/统计逻辑。
                        } else {
                            item.waiting += 1;
                            emit(
                                progress,
                                Some(&code),
                                "waiting",
                                "已执行，官方尚未确认任务进度；请稍后刷新或在官方客户端完成条件后同步",
                            );
                            continue;
                        }
                    } else {
                        emit(
                            progress,
                            Some(&code),
                            "waiting",
                            format!("「{label}」需要在官方客户端完成实际操作，本轮不自动执行"),
                        );
                    }
                }
                if task.claimed {
                    item.completed += 1;
                    continue;
                }
                if task.skipped {
                    item.skipped += 1;
                    emit(
                        progress,
                        Some(&code),
                        "skipped",
                        format!("「{label}」已按规则忽略"),
                    );
                    continue;
                }
                if !task.claimable || task.locked {
                    item.waiting += 1;
                    continue;
                }
                emit(
                    progress,
                    Some(&code),
                    "running",
                    format!("正在领取「{label}」奖励"),
                );
                match claim_reward_for_account(client, base, account, &code).await {
                    Ok((credit, energy, already_claimed)) => {
                        item.completed += 1;
                        if already_claimed {
                            emit(
                                progress,
                                Some(&code),
                                "completed",
                                format!("「{label}」已在其他请求中领取，本次没有新增奖励"),
                            );
                            continue;
                        }
                        item.claimed += 1;
                        item.credit += credit;
                        item.energy += energy;
                        emit(
                            progress,
                            Some(&code),
                            "completed",
                            format!("「{label}」奖励已确认，+{credit} 积分 / +{energy} 能量"),
                        );
                    }
                    Err(error) => {
                        errors.push(format!("领取任务奖励失败（{code}）: {error}"));
                        emit(
                            progress,
                            Some(&code),
                            "failed",
                            format!("「{label}」领奖失败：{error}"),
                        );
                    }
                }
            }
        }
        Err(error) => {
            errors.push(format!("任务列表失败: {error}"));
            emit(
                progress,
                None,
                "failed",
                format!("读取任务列表失败：{error}"),
            );
        }
    }
    let billing = balances::billing_base(base, &account.realm);
    if let Ok(billing) = billing {
        for (path, label) in [
            ("claim-gift", "领取礼包失败"),
            ("claim-compensation", "领取补偿失败"),
        ] {
            emit(
                progress,
                None,
                "running",
                format!(
                    "正在检查{}",
                    if path == "claim-gift" {
                        "礼包"
                    } else {
                        "补偿"
                    }
                ),
            );
            if let Err(error) = try_claim_optional_benefit(
                client,
                &format!("{billing}/billing/meter/{path}"),
                account.token.trim(),
            )
            .await
            {
                errors.push(format!("{label}: {error}"));
                emit(progress, None, "failed", format!("{label}：{error}"));
            }
        }
    }
    if !errors.is_empty() {
        item.error = Some(errors.join("；"));
    }
    emit(
        progress,
        None,
        if item.error.is_some() {
            "failed"
        } else {
            "completed"
        },
        format!(
            "账号作业已结束，领奖 {} 项，忽略 {} 项，待完成 {} 项",
            item.claimed, item.skipped, item.waiting
        ),
    );
    item
}

#[cfg(test)]
pub async fn run_all(
    client: &reqwest::Client,
    base: &str,
    accounts: &[crate::config::AccountConfig],
) -> TaskRunResult {
    let mut items = Vec::new();
    let mut total_credit = 0i64;
    let mut total_energy = 0i64;
    for a in accounts
        .iter()
        .filter(|a| a.enabled && !a.paused && a.realm == "cn" && !a.token.trim().is_empty())
    {
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
                path if path.starts_with("/activity/growth/tasks/") && path.ends_with("/claim") => {
                    assert_eq!(request.headers()["x-client-platform"], "web");
                    assert_eq!(request.headers()["x-user-id"], "official-test-uid");
                    let body = to_bytes(request.into_body(), 8192).await.unwrap();
                    assert!(body.is_empty());
                    let code = path.trim_start_matches("/activity/growth/tasks/").trim_end_matches("/claim");
                    match code {
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
    #[test]
    fn ignored_tasks_match_six_requests_without_excluding_unrelated_experts() {
        for title in [
            "体验「无畏契约拼豆专家」",
            "体验「中国电商运营专家」",
            "体验「商家宣传页与海报设计师」",
            "体验「直播电商教练」",
            "关注「腾讯 WorkBuddy」官方公众号",
            "体验「抖音策略师」",
        ] {
            assert!(skip_reason("future-code", title).is_some(), "{title}");
        }
        assert!(skip_reason("wb_wechat_oa_subscribe_task", "").is_some());
        assert!(skip_reason("expert_5", "使用五个专家").is_none());
        assert!(skip_reason("manual", "关注项目进展").is_none());
    }

    #[tokio::test]
    async fn job_reports_chat5_events_and_claims_only_on_confirmed_progress() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for scenario in ["advance", "stall", "already"] {
            let reads = Arc::new(AtomicUsize::new(0));
            let reports = Arc::new(AtomicUsize::new(0));
            let claims = Arc::new(AtomicUsize::new(0));
            let app_reads = reads.clone();
            let app_reports = reports.clone();
            let app_claims = claims.clone();
            let app = Router::new().fallback(any(move |request: Request| {
                let reads = app_reads.clone();
                let reports = app_reports.clone();
                let claims = app_claims.clone();
                async move {
                    let path = request.uri().path().to_string();
                    let value = match path.as_str() {
                        "/v2/activity/growth/tasks" => {
                            assert_eq!(request.headers()["x-user-id"], "official-test-uid");
                            assert_eq!(request.headers()["x-enterprise-id"], "test-enterprise");
                            let read_index = reads.fetch_add(1, Ordering::SeqCst);
                            // chat_5 进度：already 始终达标；advance 在执行动作后的
                            // 回读达标；stall 始终不推进（验证不重复上报 + 不误领奖）。
                            let chat_current = match scenario {
                                "already" => 1,
                                "advance" if read_index >= 1 => 1,
                                _ => 0,
                            };
                            let mut tasks = vec![serde_json::json!({"task_code":"chat_5","title":"真实对话","progress":null,"current":chat_current,"target":1,"accept_status":"accepted"}),
                                serde_json::json!({"task_code":"manual-operation","title":"真实客户端操作","target":1,"current":0,"accept_status":"accepted"})];
                            for (index, title) in ["无畏契约拼豆专家","中国电商运营专家","商家宣传页与海报设计师","直播电商教练","关注 腾讯 WorkBuddy 官方公众号","抖音策略师"].iter().enumerate() {
                                tasks.push(serde_json::json!({"task_code":format!("ignored-{index}"),"title":title,"target":1,"current":1,"accept_status":"not_accepted"}));
                            }
                            serde_json::json!({"code":0,"data":{"tasks":tasks}})
                        }
                        "/activity/growth/tasks/chat_5/claim" => {
                            assert!(matches!(scenario, "advance" | "already"));
                            assert_eq!(request.headers()["x-client-platform"], "web");
                            assert_eq!(request.headers()["x-user-id"], "official-test-uid");
                            assert!(to_bytes(request.into_body(),8192).await.unwrap().is_empty());
                            claims.fetch_add(1, Ordering::SeqCst);
                            serde_json::json!({"code":0,"data":{"credit":100,"energy":5}})
                        }
                        "/v2/report" => {
                            // chat_5 判据：chat_request_send 事件（billing 域），模型对齐官方口径。
                            assert_eq!(request.headers()["x-user-id"], "official-test-uid");
                            let body: serde_json::Value = serde_json::from_slice(&to_bytes(request.into_body(),8192).await.unwrap()).unwrap();
                            let event = &body[0];
                            assert_eq!(event["eventCode"], "chat_request_send");
                            assert_eq!(event["userId"], "official-test-uid");
                            assert_eq!(event["requestModelId"], "deepseek-v4-flash");
                            assert!(event["conversationId"].as_str().is_some_and(|id| id.starts_with("wb-task-chat5-")));
                            reports.fetch_add(1, Ordering::SeqCst);
                            serde_json::json!({"code":0,"data":{}})
                        }
                        "/v2/billing/meter/daily-checkin" | "/billing/meter/claim-gift" | "/billing/meter/claim-compensation" => serde_json::json!({"code":0,"data":{}}),
                        _ => panic!("Unexpected request including skipped task or fabricated telemetry: {path}"),
                    };
                    (StatusCode::OK, axum::Json(value))
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let events = Arc::new(Mutex::new(Vec::new()));
            let captured = events.clone();
            let result = run_account_with_progress(
                &reqwest::Client::new(),
                &base,
                &AppConfig::default(),
                &account(),
                &move |event| {
                    captured.lock().unwrap().push(event);
                },
            )
            .await;
            server.abort();
            assert_eq!(result.skipped, 6);
            assert_eq!(
                reports.load(Ordering::SeqCst),
                if scenario == "already" { 0 } else { 1 },
                "chat_5 只按差额补报一次事件，进度停滞时不重复上报"
            );
            assert_eq!(
                claims.load(Ordering::SeqCst),
                if matches!(scenario, "advance" | "already") {
                    1
                } else {
                    0
                }
            );
            assert_eq!(
                result.claimed,
                if matches!(scenario, "advance" | "already") {
                    1
                } else {
                    0
                }
            );
            assert_eq!(result.waiting, if scenario == "stall" { 2 } else { 1 });
            let logs = events.lock().unwrap();
            assert_eq!(
                logs.iter()
                    .filter(|event| event.status == "skipped")
                    .count(),
                6
            );
            assert!(logs.last().unwrap().message.contains("作业已结束"));
        }
    }

    #[tokio::test]
    async fn job_logs_redact_upstream_echoed_credentials() {
        let (base, server) = mock_api(
            StatusCode::BAD_REQUEST,
            r#"{"code":10001,"msg":"test-token"}"#,
        )
        .await;
        let logs = Arc::new(Mutex::new(Vec::new()));
        let captured = logs.clone();
        let result =
            run_account_with_progress(&reqwest::Client::new(), &base, &AppConfig::default(), &account(), &move |event| {
                captured.lock().unwrap().push(event)
            })
            .await;
        server.abort();
        assert!(!result.error.unwrap().contains("test-token"));
        assert!(logs
            .lock()
            .unwrap()
            .iter()
            .all(|event| !event.message.contains("test-token")));
    }
}
