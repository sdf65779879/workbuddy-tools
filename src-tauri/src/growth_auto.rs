//! 成长任务「一键完成可自动化任务」的行为动作实现。
//!
//! 移植自 codebuddy_sub2api（internal/panel/autotask.go + internal/upstream/desktop.go
//! + report.go + travel.go）。核心结论（官方实测口径）：
//!   - 多数「需客户端操作」的任务判据不是独立端点，而是同一 POST /v2/report 通道上的
//!     **桌面指纹事件链**（copilot.tencent.com/v2/report，UA 带 CLI 前缀、extName=workbuddy-desktop）。
//!   - chat_5 的判据是「累计 N 条 chat_request_send 事件上报」（billing 域 codebuddy.cn/v2/report），
//!     纯事件补报即可点亮，无需真实对话（真实对话反而白耗配额）。
//!   - expert_5 / Expert_team_use_3 / Expert_lighthouse 需要真实专家市场列表 + 召唤链 +
//!     真实 chat（拿服务端 requestId）+ expert_actual_use 事件；自造 id/requestId 不计数。
//!
//! 所有动作幂等：已达标/已领取的任务由调用方（tasks.rs）在进入前跳过。

use crate::{
    balances,
    config::{AccountConfig, AppConfig},
    tasks::{self, TaskView},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// 桌面端指纹 UA（与官方客户端实测一致，5.5.6 内嵌 CLI 2.137.1）。
const DESKTOP_UA: &str = "WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1";
/// 连续上报之间的间隔（对齐上游脚本实测 1.05s，避免风控）。
const REPORT_GAP_MS: u64 = 1050;

// ---------------------------------------------------------------------------
// 域 / 设备标识辅助
// ---------------------------------------------------------------------------

fn chat_base(configured_base: &str, realm: &str) -> Result<String, String> {
    tasks::growth_base(configured_base, realm)
}

fn web_base(configured_base: &str, realm: &str) -> Result<String, String> {
    tasks::web_base(configured_base, realm)
}

fn now_ms() -> i64 {
    crate::reqlog::now_ms()
}

fn official_uid(account: &AccountConfig) -> Result<String, String> {
    balances::upstream_user_id(account)
        .map(|cow| cow.into_owned())
        .ok_or_else(|| "账号缺少官方用户 ID，无法上报".to_string())
}

/// 由 uid 稳定派生 36 位 hex 设备/会话标识（幂等：同账号每次相同，模拟固定设备）。
fn derive_id(uid: &str, salt: &str) -> String {
    let hash = Sha256::digest(format!("{salt}:{uid}").as_bytes());
    hash[..18]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// 桌面指纹事件上报
// ---------------------------------------------------------------------------

/// 公共桌面指纹字段（注入每个事件，业务字段可覆盖同名键）。
fn desktop_fingerprint(account: &AccountConfig) -> Value {
    let uid = balances::upstream_user_id(account)
        .map(|cow| cow.into_owned())
        .unwrap_or_default();
    let nickname = account.nickname.clone();
    let now = now_ms();
    json!({
        "timezone": "Asia/Shanghai",
        "reportDelay": 2000,
        "userId": uid,
        "username": nickname,
        "userNickname": nickname,
        "product": "SaaS",
        "releaseDate": 1789036585355_i64,
        "commit": "5f9692923c93033111c51ad7b003eb80204a9b75",
        "ideName": "WorkBuddy",
        "ideType": "WorkBuddy",
        "ideVersion": "5.5.6",
        "machineId": derive_id(&uid, "machine"),
        "sessionId": derive_id(&uid, "session"),
        "extName": "workbuddy-desktop",
        "extVersion": "5.5.6",
        "os": "win32",
        "arch": "x64",
        "osVersion": "10.0.26220",
        "cpuCores": 20,
        "memorySize": 24,
        "timestamp": now,
        "presentAt": now,
    })
}

/// 以桌面客户端指纹向 copilot.tencent.com/v2/report 批量上报事件。
async fn report_desktop_events(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
    events: &[Value],
) -> Result<(), String> {
    if events.is_empty() {
        return Err("桌面事件上报：无事件".into());
    }
    let fp = desktop_fingerprint(account);
    let arr: Vec<Value> = events
        .iter()
        .map(|ev| {
            let mut merged = fp.clone();
            if let Some(obj) = ev.as_object() {
                for (key, value) in obj {
                    merged[key] = value.clone();
                }
            }
            merged
        })
        .collect();
    let base_url = chat_base(base, &account.realm)?;
    let uid = official_uid(account)?;
    let response = client
        .post(format!("{base_url}/v2/report"))
        .bearer_auth(account.token.trim())
        .header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json;charset=UTF-8")
        .header("User-Agent", DESKTOP_UA)
        .header("X-Domain", &base_url)
        .header("X-Product", "SaaS")
        .header(
            "X-Request-ID",
            format!("{}{}", derive_id(&uid, "req"), now_ms() % 1_000_000),
        )
        .header("X-User-Id", &uid)
        .json(&arr)
        .send()
        .await
        .map_err(|_| "桌面事件上报请求失败或超时")?;
    let (status, text) = tasks::response_text(response).await?;
    tasks::parse_api_response(status, &text)?;
    Ok(())
}

/// 以 Web 端指纹向 www.workbuddy.cn/v2/report 上报单事件（页面行为类任务）。
async fn report_web_event(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
    event_code: &str,
    page_url: &str,
    element_id: &str,
    element_name: &str,
) -> Result<(), String> {
    let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";
    let uid = official_uid(account)?;
    let ev = json!({
        "eventCode": event_code,
        "timestamp": now_ms(),
        "reportDelay": 0,
        "pageURL": page_url,
        "elementId": element_id,
        "elementName": element_name,
        "os": "Win32",
        "arch": "",
        "osVersion": "10.0",
        "userAgent": ua,
        "machineId": derive_id(&uid, "webmachine"),
        "userId": uid,
        "userNickname": account.nickname,
        "enterpriseId": account.enterprise_id,
    });
    let base_url = web_base(base, &account.realm)?;
    let response = client
        .post(format!("{base_url}/v2/report"))
        .bearer_auth(account.token.trim())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("x-client-platform", "web")
        .header("Origin", &base_url)
        .header("Referer", page_url)
        .header("User-Agent", ua)
        .header("X-User-Id", &uid)
        .json(&vec![ev])
        .send()
        .await
        .map_err(|_| "Web 事件上报请求失败或超时")?;
    let (status, text) = tasks::response_text(response).await?;
    tasks::parse_api_response(status, &text)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 桌面事件序列构造函数（纯函数，返回业务载荷，公共指纹由上报时注入）
// ---------------------------------------------------------------------------

/// 构造一次「桌面端成功对话」完整事件链（agent_task_created → chat_message_send →
/// chat_request_send → chat_message_response(isSuccessful) → chat_message_status →
/// chat_request_response）。实测点亮 RichMeow_Chat。
fn desktop_chat_sequence(
    conversation_id: &str,
    request_id: &str,
    message_id: &str,
    model_id: &str,
    model_name: &str,
) -> Vec<Value> {
    let mk = |code: &str, extra: Value| {
        let mut ev = json!({ "eventCode": code });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                ev[k] = v.clone();
            }
        }
        ev
    };
    vec![
        mk(
            "agent_task_created",
            json!({
                "source": "LOCAL", "name": "working", "task_target": "local", "mode": "craft",
                "requestModelId": model_id, "requestModelName": model_name,
                "has_repo": false, "repo_type": "none", "workspace_type": "empty",
                "has_connector": false, "connector_types": [],
                "has_mention": false, "mention_types": [],
                "has_template": false, "action": "", "template_name": "",
                "has_expert": false, "expert_id": "", "expert_name": "", "expert_industry_id": "",
                "has_skill": false, "skill_names": [],
                "conversationId": conversation_id, "messageId": message_id,
                "buddyId": "", "buddyName": "",
            }),
        ),
        mk(
            "chat_message_send",
            json!({
                "messageId": format!("{message_id}-assistant"), "historyCount": 0,
                "isContextTruncated": false, "currentStepCount": 1,
                "traceId": request_id, "rootRequestId": request_id,
                "parentConversationId": conversation_id,
                "agentName": "cli", "agentType": "main",
            }),
        ),
        mk(
            "chat_request_send",
            json!({
                "inputLength": 24, "isPlan": false, "isAutoExecuteTerminal": false,
                "isAutoModify": false, "codebaseEnable": false, "maxToken": 0,
                "maxSteps": 500, "temperature": 0, "maxRetries": 0,
                "mentionContexts": [], "knowledgeId": [], "knowledgeName": [],
                "codebaseId": "", "mentionContextCount": 0, "command": "",
                "recommendId": "", "skillId": "", "skillCount": 0, "totalCount": 0,
                "traceId": request_id, "rootRequestId": request_id,
                "parentConversationId": conversation_id,
                "agentName": "cli", "agentType": "main",
                "codebuddy.session_id": conversation_id,
                "codebuddy.conversation_request_id": request_id,
            }),
        ),
        mk(
            "chat_message_response",
            json!({
                "messageId": format!("{message_id}-assistant"), "responseModelId": model_id,
                "inputToken": 120, "outputToken": 80, "totalToken": 200,
                "cachedTokens": 0, "cachedWriteTokens": 0, "cachedMissTokens": 0,
                "isSuccessful": true, "messageErrorCode": "", "finishReason": "stop",
                "firstTokenAt": now_ms(), "traceId": request_id,
                "conversationId": conversation_id,
                "rootRequestId": request_id, "parentConversationId": conversation_id,
                "agentName": "cli", "agentType": "main",
                "codebuddy.session_id": conversation_id,
                "codebuddy.conversation_request_id": request_id,
            }),
        ),
        mk(
            "chat_message_status",
            json!({
                "messageId": format!("{message_id}-assistant"), "messageErrorCode": "0",
                "traceId": request_id, "rootRequestId": request_id,
                "parentConversationId": conversation_id,
                "agentName": "cli", "agentType": "main",
            }),
        ),
        mk(
            "chat_request_response",
            json!({
                "mode": "craft", "toolCallCount": 0,
                "inputToken": 120, "outputToken": 80, "totalToken": 200,
                "cachedTokens": 0, "cachedWriteTokens": 0, "cachedMissTokens": 0,
                "isSuccessful": true, "messageErrorCode": "", "finishReason": "stop",
                "rootRequestId": request_id, "parentConversationId": conversation_id,
            }),
        ),
    ]
}

/// 构造「进入 Buddy 应用」五连事件（discover → show → enter_click → auth_confirm → bind_skip）。
fn desktop_buddy_app_sequence(buddy_id: &str, buddy_name: &str) -> Vec<Value> {
    let mk = |code: &str, extra: Value| {
        let mut ev = json!({ "eventCode": code, "mode": "LOCAL", "buddyId": buddy_id, "buddyName": buddy_name });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                ev[k] = v.clone();
            }
        }
        ev
    };
    vec![
        mk("buddyapp_discover_click", json!({})),
        mk("buddyapp_show", json!({ "elementId": buddy_id, "elementName": buddy_name, "position": 2 })),
        mk("buddyapp_enter_click", json!({ "elementId": buddy_id, "elementName": buddy_name, "position": 2, "isFirstPage": "1" })),
        mk("buddyapp_auth_confirm_click", json!({ "elementId": buddy_id, "elementName": buddy_name })),
        mk("buddyapp_bindaccount_skip_click", json!({ "elementId": buddy_id, "elementName": buddy_name })),
    ]
}

/// 构造「定时任务创建成功」事件（automation_1 判据）。
fn desktop_automation_create_event(name: &str) -> Value {
    json!({
        "eventCode": "automated_task_create_suc", "name": name,
        "source": "manually", "modelId": "fast-model", "modelIsThinking": true,
        "connectorCount": 0, "skills": "", "skillCount": 0,
        "scheduleType": "once", "mode": "LOCAL",
    })
}

/// 构造「使用模板创建任务」事件组（template_5 判据）。
fn desktop_template_use_sequence(
    conversation_id: &str,
    request_id: &str,
    template_id: &str,
    template_name: &str,
) -> Vec<Value> {
    let mut events = desktop_chat_sequence(
        conversation_id,
        request_id,
        &format!("msg-{template_id}"),
        "fast-model",
        "fast-model",
    );
    events.push(json!({
        "eventCode": "agent_task_created_with_template", "mode": "working",
        "isCustomModel": false, "id": template_id, "name": template_name, "requestId": request_id,
    }));
    events.push(json!({ "eventCode": "template_used", "template_id": template_id, "task_mode": "working" }));
    events
}

/// 构造「灵感案例做同款」事件组（playbook_prompt 判据）。
fn desktop_playbook_prompt_sequence(
    conversation_id: &str,
    request_id: &str,
    case_id: &str,
    case_name: &str,
) -> Vec<Value> {
    let mut events = desktop_chat_sequence(
        conversation_id,
        request_id,
        "msg-pb",
        "fast-model",
        "fast-model",
    );
    let payload = json!({ "id": case_id, "name": case_name, "type": "document", "categoryId": "", "categoryName": "" });
    events.push(json!({
        "eventCode": "web_element_click", "pageName": "playbook_detail",
        "elementId": "playbook_ctaClick", "elementName": case_name, "source": "discover",
    }));
    let mut cta = json!({ "eventCode": "playbook_cta_click", "source": "discover", "position": 0 });
    for (k, v) in payload.as_object().unwrap() {
        cta[k] = v.clone();
    }
    events.push(cta);
    let mut send = json!({ "eventCode": "playbook_prompt_send", "conversationId": conversation_id, "requestId": request_id });
    for (k, v) in payload.as_object().unwrap() {
        send[k] = v.clone();
    }
    events.push(send);
    events
}

/// 构造「设计创意画布」事件组（create_canvas 判据）。
fn desktop_design_canvas_sequence(conversation_id: &str, request_id: &str) -> Vec<Value> {
    let mut events = desktop_chat_sequence(
        conversation_id,
        request_id,
        "msg-canvas",
        "fast-model",
        "fast-model",
    );
    events.push(json!({
        "eventCode": "wbx_design_canvas_task_create", "conversationId": conversation_id,
        "requestId": request_id, "source": "summon_keyword", "cost": 12000, "isSuccessful": true,
    }));
    events.push(json!({
        "eventCode": "wbx_design_canvas_open", "conversationId": conversation_id,
        "requestId": request_id, "id": format!("ardot-file-{}", &request_id[request_id.len().saturating_sub(8)..]),
        "source": "summon_keyword", "type": "page", "cost": 13000, "isSuccessful": true,
    }));
    events
}

// ---------------------------------------------------------------------------
// 专家市场 / 带专家真实对话
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct MarketExpert {
    expert_id: String,
    expert_type: String,
    display_name_zh: String,
    profession_zh: String,
    version: String,
    categories: Vec<String>,
}

impl MarketExpert {
    fn category(&self) -> &str {
        self.categories.first().map(String::as_str).unwrap_or("expert-all")
    }
    fn version_or(&self) -> &str {
        if self.version.is_empty() {
            "1.0.0"
        } else {
            &self.version
        }
    }
}

/// 拉取专家市场真实专家列表（expertType: "agent" 单专家 / "team" 专家团）。
async fn market_expert_list(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
    expert_type: &str,
) -> Result<Vec<MarketExpert>, String> {
    let base_url = chat_base(base, &account.realm)?;
    let uid = official_uid(account)?;
    let mut body = json!({ "page": 1, "page_size": 20, "sort_by": "reco_rank", "sort_order": "desc" });
    if !expert_type.is_empty() {
        body["expert_type"] = json!(expert_type);
    }
    let response = client
        .post(format!(
            "{base_url}/portal/operation-platform/market/expert/list"
        ))
        .bearer_auth(account.token.trim())
        .header("Content-Type", "application/json")
        .header("User-Agent", DESKTOP_UA)
        .header("X-Domain", &base_url)
        .header("X-Product", "SaaS")
        .header("X-User-Id", &uid)
        .json(&body)
        .send()
        .await
        .map_err(|_| "专家市场列表请求失败或超时")?;
    let (status, text) = tasks::response_text(response).await?;
    let value = tasks::parse_api_response(status, &text)?;
    let experts = value
        .get("experts")
        .or_else(|| value.get("data").and_then(|d| d.get("experts")))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for item in experts {
        let s = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        out.push(MarketExpert {
            expert_id: s("expert_id"),
            expert_type: s("expert_type"),
            display_name_zh: s("display_name_zh"),
            profession_zh: s("profession_zh"),
            version: s("version"),
            categories: item
                .get("categories")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| c.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    Ok(out)
}

/// 服务端 requestId 形状（cmb- 前缀 32hex 或裸 32hex）。
fn is_server_request_id(id: &str) -> bool {
    let hex = id.strip_prefix("cmb-").unwrap_or(id);
    hex.len() == 32 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 发一条真实桌面指纹 chat 请求（可带 X-Expert-Id），从 SSE 流解析服务端 requestId。
/// expert_actual_use 等 JOIN 事件的 requestId 必须是该服务端 id。
async fn chat_with_expert(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
    expert_id: &str,
) -> Result<(String, String), String> {
    let conversation_id = format!("wb-task-conv-{}", now_ms());
    let url = crate::gateway::request::chat_url(base, &account.realm)?;
    let body = json!({
        "model": "fast-model",
        "messages": [
            {"role": "system", "content": "You are a helpful assistant. 当前处于中文环境，使用简体中文回答。"},
            {"role": "user", "content": "1+1等于几？直接回答。"}
        ],
        "agent": "cli",
        "temperature": 1,
        "stream": true,
        "stream_options": {"include_usage": true}
    });
    let mut request = crate::gateway::request::chat_post(client, &url, account, &body)
        .header("X-Conversation-ID", &conversation_id);
    if !expert_id.is_empty() {
        request = request.header("X-Expert-Id", expert_id);
    }
    let response = request
        .send()
        .await
        .map_err(|_| "专家对话请求失败或超时")?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("专家对话失败 HTTP {status}"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| "读取专家对话响应失败")?;
    let text = String::from_utf8_lossy(&bytes);
    // 从 SSE 抓第一个符合形状的 data.id 作为服务端 requestId。
    let mut search_from = 0usize;
    while let Some(rel) = text[search_from..].find("\"id\":\"") {
        let abs = search_from + rel;
        let rest = &text[abs + 6..];
        if let Some(end) = rest.find('"') {
            let id = &rest[..end];
            if is_server_request_id(id) {
                return Ok((conversation_id, id.to_string()));
            }
            search_from = abs + 1;
        } else {
            break;
        }
    }
    Err("SSE 中未找到服务端 requestId".into())
}

/// 构造「召唤平台专家」事件组（web_element_click + expert_summon_click + expert_summoned）。
fn desktop_expert_summon_sequence(expert: &MarketExpert) -> Vec<Value> {
    let cat = expert.category();
    let ver = expert.version_or();
    vec![
        json!({
            "eventCode": "web_element_click", "source": expert.expert_id, "type": cat, "version": ver,
            "elementId": "expert_summon_click", "elementName": "立即召唤",
            "pageURL": "/C:/Program%20Files/WorkBuddy/resources/app.asar/renderer/index.html",
        }),
        json!({
            "eventCode": "expert_summon_click", "id": expert.expert_id, "name": expert.display_name_zh,
            "expertTitle": expert.profession_zh, "type": "expert-all", "position": 0,
            "expertType": expert.expert_type, "version": ver, "mode": "LOCAL",
        }),
        json!({
            "eventCode": "expert_summoned", "id": expert.expert_id, "name": expert.display_name_zh,
            "expertTitle": expert.profession_zh, "type": "expert-all",
        }),
    ]
}

/// 构造「专家真实使用」事件（expert_5/Expert_team_use_3 判据）。requestId 必须为服务端 id。
fn desktop_expert_actual_use(expert: &MarketExpert, conversation_id: &str, request_id: &str) -> Value {
    let cat = expert.category();
    let ver = expert.version_or();
    json!({
        "eventCode": "expert_actual_use",
        "id": expert.expert_id, "name": expert.display_name_zh, "expertTitle": expert.profession_zh,
        "type": cat, "expertType": expert.expert_type, "source": "builtin", "version": ver,
        "cost": 9000, "characterCount": 14,
        "conversationId": conversation_id, "requestId": request_id,
        "messageId": format!("msg-{}", &request_id[request_id.len().saturating_sub(8)..]),
        "requestModelId": "fast-model", "requestModelName": "fast-model",
    })
}

// ---------------------------------------------------------------------------
// 各任务动作
// ---------------------------------------------------------------------------

/// chat_5：按差额补报 chat_request_send（不做真实对话）。
async fn run_chat5(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
    task: &TaskView,
) -> Result<String, String> {
    let target = if task.target > 0 { task.target } else { 5 };
    let need = (target - task.current).max(0);
    if need <= 0 {
        return Ok("进度已达标，无需上报".into());
    }
    for i in 0..need {
        let cid = format!("wb-task-chat5-{}-{}", now_ms(), i);
        tasks::report_chat_activity(
            client,
            base,
            account,
            &cid,
            "deepseek-v4-flash",
            "DeepSeek V4 Flash",
        )
        .await
        .map_err(|e| format!("上报第 {}/{} 条失败: {}", i + 1, need, e))?;
        if i < need - 1 {
            tokio::time::sleep(Duration::from_millis(REPORT_GAP_MS)).await;
        }
    }
    Ok(format!("已补报 {need} 条对话事件"))
}

/// first_buddy：上报解锁 → 同意协议 → 领取第一只 Buddy。
async fn run_first_buddy(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> Result<String, String> {
    tasks::report_chat_activity(
        client,
        base,
        account,
        &format!("wb-task-adopt-{}", now_ms()),
        "deepseek-v4-flash",
        "DeepSeek V4 Flash",
    )
    .await
    .map_err(|e| format!("前置上报: {e}"))?;
    tokio::time::sleep(Duration::from_millis(REPORT_GAP_MS)).await;
    let base_url = chat_base(base, &account.realm)?;
    // 同意协议（幂等）。
    let agreement = client
        .post(format!("{base_url}/activity/growth/buddy/agreement"))
        .bearer_auth(account.token.trim())
        .header("User-Agent", "WorkBuddy/5.5.4")
        .header("X-CodeBuddy-Request", "1")
        .header("Content-Type", "application/json")
        .json(&json!({ "agree": true }))
        .send()
        .await
        .map_err(|_| "同意协议请求失败")?;
    let (status, text) = tasks::response_text(agreement).await?;
    tasks::parse_api_response(status, &text).map_err(|e| format!("同意协议: {e}"))?;
    // 领取 Buddy。
    let first = client
        .post(format!("{base_url}/activity/growth/buddy/first"))
        .bearer_auth(account.token.trim())
        .header("User-Agent", "WorkBuddy/5.5.4")
        .header("X-CodeBuddy-Request", "1")
        .header("Content-Type", "application/json")
        .json(&json!({}))
        .send()
        .await
        .map_err(|_| "领取 Buddy 请求失败")?;
    let (status, text) = tasks::response_text(first).await?;
    match tasks::parse_api_response(status, &text) {
        Ok(_) => Ok("已领取 Buddy（+300 分 +8 能量）".into()),
        Err(_) if status.as_u16() == 400 && text.contains("first_buddy") => {
            Ok("前置已上报，但领养门槛未过（上游要求当日活跃），请稍后重试".into())
        }
        Err(e) => Err(format!("领取 Buddy: {e}")),
    }
}

/// Model_chat_GLM5.2：真实 glm-5.2 对话 + 对齐模型上报。
async fn run_model_chat(
    client: &reqwest::Client,
    base: &str,
    cfg: &AppConfig,
    account: &AccountConfig,
) -> Result<String, String> {
    tasks::real_conversation(client, base, cfg, account, "glm-5.2")
        .await
        .map_err(|e| format!("对话请求: {e}"))?;
    tokio::time::sleep(Duration::from_millis(REPORT_GAP_MS)).await;
    tasks::report_chat_activity(
        client,
        base,
        account,
        &format!("wb-task-glm52-{}", now_ms()),
        "glm-5.2",
        "GLM-5.2",
    )
    .await
    .map_err(|e| format!("进度上报失败: {e}"))?;
    Ok("已完成 glm-5.2 对话并上报".into())
}

/// RichMeow_Chat（桌面端对话1次）：桌面指纹完整对话事件链。
async fn run_rich_meow(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> Result<String, String> {
    let ms = now_ms();
    let conv = format!("wb-task-rm-{ms}");
    let req = format!("wb-task-rm-req-{ms}");
    let msg = format!("req-{ms}-user");
    let events = desktop_chat_sequence(&conv, &req, &msg, "fast-model", "fast-model");
    report_desktop_events(client, base, account, &events).await?;
    Ok("已按桌面端指纹上报完整对话事件链".into())
}

/// Buddy_App / Buddy_App_QQ：进入 Buddy 应用五连事件。
async fn run_buddy_app(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> Result<String, String> {
    let events = desktop_buddy_app_sequence("cb_y5Dy46tPQGGWtueMxXbe", "企鹅教师助手");
    report_desktop_events(client, base, account, &events).await?;
    Ok("已上报 buddyapp 进入五连事件（同时覆盖 Buddy_App 与 Buddy_App_QQ）".into())
}

/// automation_1：定时任务创建成功事件。
async fn run_automation_create(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> Result<String, String> {
    let event = desktop_automation_create_event("wb-task 自动化");
    report_desktop_events(client, base, account, &[event]).await?;
    Ok("已上报定时任务创建事件".into())
}

/// Library_read：资料库介绍阅读事件（web 域）。
async fn run_library_read(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> Result<String, String> {
    const DOC_URL: &str = "https://www.workbuddy.cn/space/d/o0KWYeynteVv06UnAZqIFm";
    report_web_event(
        client,
        base,
        account,
        "web_element_click",
        DOC_URL,
        "library_doc_intro_click",
        "WorkBuddy资料库介绍",
    )
    .await?;
    Ok("已上报资料库介绍阅读事件".into())
}

/// template_5：5 组模板事件。
async fn run_template_use(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> Result<String, String> {
    let templates = [
        ("1", "深度研究"),
        ("2", "周报生成"),
        ("3", "竞品分析"),
        ("4", "活动策划"),
        ("5", "代码评审"),
    ];
    for (i, (tid, tname)) in templates.iter().enumerate() {
        let ms = now_ms();
        let conv = format!("wb-task-tpl-{ms}-{i}");
        let req = format!("wb-task-tpl-req-{ms}-{i}");
        let events = desktop_template_use_sequence(&conv, &req, tid, tname);
        report_desktop_events(client, base, account, &events)
            .await
            .map_err(|e| format!("第 {} 组模板事件上报失败: {}", i + 1, e))?;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    Ok("已上报 template_used ×5".into())
}

/// playbook_prompt：灵感案例发送 Prompt 事件组。
async fn run_playbook_prompt(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> Result<String, String> {
    let ms = now_ms();
    let conv = format!("wb-task-pb-{ms}");
    let req = format!("wb-task-pb-req-{ms}");
    let events = desktop_playbook_prompt_sequence(&conv, &req, "pm-gtm-launch-plan", "新产品上市 GTM 发布计划一页纸");
    report_desktop_events(client, base, account, &events).await?;
    Ok("已上报 playbook_cta_click + playbook_prompt_send".into())
}

/// create_canvas：设计创意画布事件组（+300 分）。
async fn run_create_canvas(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> Result<String, String> {
    let ms = now_ms();
    let conv = format!("wb-task-canvas-{ms}");
    let req = format!("wb-task-canvas-req-{ms}");
    let events = desktop_design_canvas_sequence(&conv, &req);
    report_desktop_events(client, base, account, &events).await?;
    Ok("已上报 wbx_design_canvas_task_create/open".into())
}

/// expert_5（5 个平台专家）/ Expert_team_use_3（3 个专家团）的公共实现。
async fn run_expert_batch(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
    expert_type: &str,
    count: usize,
) -> Result<String, String> {
    let experts = market_expert_list(client, base, account, expert_type).await?;
    if experts.is_empty() {
        return Err("专家市场列表为空".into());
    }
    let mut ok = 0usize;
    for (i, expert) in experts.iter().enumerate() {
        if ok >= count {
            break;
        }
        // 召唤链。
        let summon = desktop_expert_summon_sequence(expert);
        if let Err(_) = report_desktop_events(client, base, account, &summon).await {
            continue;
        }
        // 真实 chat → 服务端 requestId。
        let (conv, req) = match chat_with_expert(client, base, account, &expert.expert_id).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        // 使用事件（JOIN 服务端 requestId）+ chat 链。
        let mut events = desktop_chat_sequence(
            &conv,
            &req,
            &format!("msg-{}", &req[req.len().saturating_sub(8)..]),
            "fast-model",
            "fast-model",
        );
        events.push(desktop_expert_actual_use(expert, &conv, &req));
        if let Err(_) = report_desktop_events(client, base, account, &events).await {
            continue;
        }
        ok += 1;
        if i < experts.len() - 1 {
            tokio::time::sleep(Duration::from_secs(6)).await;
        }
    }
    Ok(format!("已对 {ok} 位真实专家完成召唤+使用链（类型 {expert_type}）"))
}

/// Expert_lighthouse（腾讯轻量云专家）：召唤 + 真实对话 + LOCAL 模式使用事件。
async fn run_expert_lighthouse(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> Result<String, String> {
    const LH_ID: &str = "ex_2cvvUZQhDyeJ";
    let mut expert = MarketExpert {
        expert_id: LH_ID.to_string(),
        expert_type: "agent".to_string(),
        display_name_zh: "腾讯轻量云专家".to_string(),
        profession_zh: "腾讯轻量云专家".to_string(),
        version: "1.0.2".to_string(),
        categories: Vec::new(),
    };
    if let Ok(experts) = market_expert_list(client, base, account, "agent").await {
        if let Some(found) = experts.into_iter().find(|e| e.expert_id == LH_ID) {
            expert = found;
        }
    }
    let summon = desktop_expert_summon_sequence(&expert);
    report_desktop_events(client, base, account, &summon)
        .await
        .map_err(|e| format!("召唤链: {e}"))?;
    let (conv, req) = chat_with_expert(client, base, account, LH_ID).await?;
    let mut events = desktop_chat_sequence(
        &conv,
        &req,
        &format!("msg-{}", &req[req.len().saturating_sub(8)..]),
        "fast-model",
        "fast-model",
    );
    for ev in events.iter_mut() {
        if ev.get("eventCode").and_then(Value::as_str) == Some("agent_task_created") {
            ev["has_expert"] = json!(true);
            ev["expert_id"] = json!(expert.expert_id);
            ev["expert_name"] = json!(expert.display_name_zh);
            ev["expert_industry_id"] = json!("");
        }
    }
    let mut actual_use = desktop_expert_actual_use(&expert, &conv, &req);
    actual_use["mode"] = json!("LOCAL");
    actual_use["type"] = json!("");
    actual_use["cost"] = json!(0);
    events.push(actual_use);
    report_desktop_events(client, base, account, &events)
        .await
        .map_err(|e| format!("使用事件: {e}"))?;
    Ok("已上报轻量云专家召唤+使用链（真实对话 requestId）".into())
}

/// skill_1（尝鲜热门技能）：真实对话 + skill_info 事件。
async fn run_skill_fresh(
    client: &reqwest::Client,
    base: &str,
    account: &AccountConfig,
) -> Result<String, String> {
    let (conv, req) = chat_with_expert(client, base, account, "").await?;
    let msg_id = format!("msg-{}", &req[req.len().saturating_sub(8)..]);
    let mut events = desktop_chat_sequence(&conv, &req, &msg_id, "fast-model", "fast-model");
    for ev in events.iter_mut() {
        if ev.get("eventCode").and_then(Value::as_str) == Some("chat_message_response") {
            ev["finishReason"] = json!("tool_calls");
        }
    }
    events.push(json!({
        "eventCode": "skill_info",
        "id": "润泽小馆·日报撰写",
        "skillId": "skill_2097350077599879168",
        "skillVersion": "1.0.0",
        "toolStatus": "success",
        "fileCount": 56,
        "source": "workbuddy-desktop",
        "conversationId": conv, "requestId": req, "messageId": msg_id,
        "requestModelId": "fast-model", "requestModelName": "fast-model",
        "traceId": req,
    }));
    report_desktop_events(client, base, account, &events).await?;
    Ok("已上报真实对话 + skill_info 技能加载事件".into())
}

// ---------------------------------------------------------------------------
// 统一分发入口
// ---------------------------------------------------------------------------

/// 判断某任务 code 是否有可自动化动作。
pub fn auto_action_supported(code: &str) -> bool {
    matches!(
        code,
        "chat_5"
            | "first_buddy"
            | "Model_chat_GLM5.2"
            | "RichMeow_Chat"
            | "Buddy_App"
            | "Buddy_App_QQ"
            | "automation_1"
            | "Library_read"
            | "template_5"
            | "playbook_prompt"
            | "create_canvas"
            | "expert_5"
            | "Expert_team_use_3"
            | "Expert_lighthouse"
            | "skill_1"
    )
}

/// 执行某任务的行为动作，返回结果消息。不负责回读进度与领奖（由 tasks.rs 统一处理）。
pub async fn run_task_action(
    client: &reqwest::Client,
    base: &str,
    cfg: &AppConfig,
    account: &AccountConfig,
    task: &TaskView,
) -> Result<String, String> {
    match task.task_code.as_str() {
        "chat_5" => run_chat5(client, base, account, task).await,
        "first_buddy" => run_first_buddy(client, base, account).await,
        "Model_chat_GLM5.2" => run_model_chat(client, base, cfg, account).await,
        "RichMeow_Chat" => run_rich_meow(client, base, account).await,
        "Buddy_App" | "Buddy_App_QQ" => run_buddy_app(client, base, account).await,
        "automation_1" => run_automation_create(client, base, account).await,
        "Library_read" => run_library_read(client, base, account).await,
        "template_5" => run_template_use(client, base, account).await,
        "playbook_prompt" => run_playbook_prompt(client, base, account).await,
        "create_canvas" => run_create_canvas(client, base, account).await,
        "expert_5" => run_expert_batch(client, base, account, "agent", 5).await,
        "Expert_team_use_3" => run_expert_batch(client, base, account, "team", 3).await,
        "Expert_lighthouse" => run_expert_lighthouse(client, base, account).await,
        "skill_1" => run_skill_fresh(client, base, account).await,
        other => Err(format!("任务 {other} 无自动化动作")),
    }
}
