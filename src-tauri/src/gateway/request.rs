//! Shared WorkBuddy chat routing and identity headers for gateway requests and probes.

use crate::{balances::upstream_user_id, config::AccountConfig};
use reqwest::{Client, RequestBuilder, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};

const CLIENT_VERSION: &str = "5.5.4";
const CLI_VERSION: &str = "2.137.1";

pub(crate) struct ChatSession {
    conversation_id: String,
    request_id: String,
    trace_id: String,
}

impl ChatSession {
    /// Keep one conversation/turn identity while an API request rotates accounts.
    pub(crate) fn new(conversation_id: Option<&str>) -> Self {
        let conversation_id = conversation_id
            .filter(|id| {
                !id.trim().is_empty() && reqwest::header::HeaderValue::from_str(id).is_ok()
            })
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let request_id = uuid::Uuid::new_v4().simple().to_string();
        Self {
            conversation_id,
            trace_id: request_id.clone(),
            request_id,
        }
    }

    pub(crate) fn with_headers(
        conversation_id: Option<&str>,
        headers: &reqwest::header::HeaderMap,
    ) -> Self {
        let header = |key| {
            headers
                .get(key)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        let mut session = Self::new(header("x-conversation-id").or(conversation_id));
        if let Some(request_id) = header("x-conversation-request-id") {
            session.request_id = request_id.to_string();
        }
        session.trace_id = header("x-trace-id")
            .unwrap_or(&session.request_id)
            .to_string();
        session
    }
}

pub(crate) fn conversation_id(body: &Value) -> Option<&str> {
    [
        "/metadata/conversation_id",
        "/metadata/conversationId",
        "/conversation_id",
        "/conversationId",
        "/session_id",
        "/conversation",
    ]
    .iter()
    .filter_map(|path| body.pointer(path).and_then(Value::as_str))
    .map(str::trim)
    .find(|id| !id.is_empty())
}

fn account_stable_id(uid: &str, purpose: &str) -> String {
    let hash = Sha256::digest(format!("wb2a:{purpose}:{uid}").as_bytes());
    hash[..18]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_trace_id(id: &str) -> bool {
    matches!(id.len(), 16 | 32) && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn chat_url(configured_base: &str, realm: &str) -> Result<String, String> {
    if !matches!(realm, "cn" | "global") {
        return Err("账号区域必须为 cn 或 global".into());
    }
    let parsed = Url::parse(configured_base).map_err(|_| "聊天服务地址无效".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("聊天服务地址必须为 HTTP 或 HTTPS 地址".into());
    }
    let official = matches!(
        parsed.host_str(),
        Some(
            "www.workbuddy.cn"
                | "workbuddy.cn"
                | "www.codebuddy.cn"
                | "codebuddy.cn"
                | "copilot.tencent.com"
                | "www.workbuddy.ai"
                | "workbuddy.ai"
        )
    );
    let base = if official {
        if realm == "global" {
            "https://www.workbuddy.ai"
        } else {
            "https://copilot.tencent.com"
        }
    } else {
        configured_base.trim_end_matches('/')
    };
    Ok(format!("{base}/v2/chat/completions"))
}

/// Never forwards refresh credentials or generated local account indices.
pub(crate) fn chat_post(
    client: &Client,
    url: &str,
    account: &AccountConfig,
    body: &Value,
) -> RequestBuilder {
    let conversation_id = conversation_id(body);
    chat_post_with_session(
        client,
        url,
        account,
        body,
        &ChatSession::new(conversation_id),
    )
}

pub(crate) fn chat_post_with_session(
    client: &Client,
    url: &str,
    account: &AccountConfig,
    body: &Value,
    session: &ChatSession,
) -> RequestBuilder {
    let global = account.realm == "global";
    let origin = if global {
        "https://www.workbuddy.ai"
    } else {
        "https://www.codebuddy.cn"
    };
    let platform = if global { "WorkBuddy AI" } else { "WorkBuddy" };
    let user_agent =
        format!("WorkBuddy/{CLIENT_VERSION} {platform}/{CLIENT_VERSION} CLI/{CLI_VERSION}");
    let message_id = uuid::Uuid::new_v4().simple().to_string();
    let b3_trace = if valid_trace_id(&session.request_id) {
        &session.request_id
    } else {
        &message_id
    };
    // The global chat service expects an initial system message. Preserve any
    // existing system instructions; CN requests retain their original message order.
    let prepared = if global
        && body
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|messages| {
                !messages.is_empty()
                    && !messages[0]
                        .get("role")
                        .and_then(Value::as_str)
                        .is_some_and(|role| role.trim().eq_ignore_ascii_case("system"))
            }) {
        let mut prepared = body.clone();
        prepared["messages"].as_array_mut().unwrap().insert(
            0,
            serde_json::json!({"role":"system","content":"You are a helpful assistant."}),
        );
        Some(prepared)
    } else {
        None
    };
    let mut request = client
        .post(url)
        .bearer_auth(account.token.trim())
        .header("Accept", "application/json, text/event-stream")
        .header("X-Requested-With", "XMLHttpRequest")
        .header("Origin", origin)
        .header("Referer", format!("{origin}/"))
        .header("User-Agent", user_agent)
        .header("X-CodeBuddy-Request", "1")
        .header("X-Agent-Purpose", "conversation")
        .header("X-IDE-Name", "WorkBuddy")
        .header("X-IDE-Type", "WorkBuddy")
        .header("X-IDE-Version", CLIENT_VERSION)
        .header("X-Product", "WorkBuddy")
        .header("Accept-Language", if global { "en-US" } else { "zh-CN" })
        .header("X-Conversation-ID", &session.conversation_id)
        .header("X-Conversation-Request-ID", &session.request_id)
        .header("X-Conversation-Message-ID", &message_id)
        .header("X-Request-ID", &message_id)
        .header("X-Root-Request-ID", &session.request_id)
        .header("X-Trace-ID", &session.trace_id)
        .header("X-B3-TraceId", b3_trace)
        .header("X-B3-SpanId", &message_id[..16])
        .header("X-B3-Sampled", "1")
        .json(prepared.as_ref().unwrap_or(body));
    if let Some(uid) = upstream_user_id(account) {
        request = request
            .header("X-User-Id", uid.as_ref())
            .header("X-Machine-ID", account_stable_id(uid.as_ref(), "machine"))
            .header("X-Session-ID", account_stable_id(uid.as_ref(), "session"));
    } else {
        request = request.header("X-No-User-Id", "1");
    }
    if global {
        request = request
            .header("X-No-Enterprise-Id", "1")
            .header("X-Domain", "www.workbuddy.ai");
    } else {
        if !account.enterprise_id.trim().is_empty() {
            request = request.header("X-Enterprise-Id", account.enterprise_id.trim());
        } else {
            request = request.header("X-No-Enterprise-Id", "1");
        }
        if !account.domain.trim().is_empty() {
            request = request.header("X-Domain", account.domain.trim());
        } else {
            request = request.header("X-No-Department-Info", "1");
        }
    }
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn fake_account() -> AccountConfig {
        serde_json::from_value(serde_json::json!({
            "uid":"acct-local-test", "workbuddy_uid":"official-test-uid", "nickname":"Test",
            "token":"test-token", "refresh_token":"must-not-be-sent", "realm":"cn",
            "enterprise_id":"test-enterprise", "domain":"test-domain", "enabled":true,"paused":false
        }))
        .unwrap()
    }

    #[test]
    fn official_chat_hosts_follow_account_realm_and_custom_proxies_are_preserved() {
        assert_eq!(
            chat_url("https://www.workbuddy.cn", "cn").unwrap(),
            "https://copilot.tencent.com/v2/chat/completions"
        );
        assert_eq!(
            chat_url("https://copilot.tencent.com", "global").unwrap(),
            "https://www.workbuddy.ai/v2/chat/completions"
        );
        assert_eq!(
            chat_url("http://127.0.0.1:4567/proxy/", "cn").unwrap(),
            "http://127.0.0.1:4567/proxy/v2/chat/completions"
        );
        assert!(chat_url("file:///tmp/test", "cn").is_err());
        assert!(chat_url("https://www.workbuddy.cn", "invalid").is_err());
    }

    #[test]
    fn chat_headers_keep_real_identity_and_never_send_refresh_tokens() {
        let account = fake_account();
        let body = serde_json::json!({"stream":true});
        let request = chat_post(&Client::new(), "http://127.0.0.1:1", &account, &body)
            .build()
            .unwrap();
        let headers = request.headers();
        assert_eq!(headers["authorization"], "Bearer test-token");
        assert_eq!(headers["x-user-id"], "official-test-uid");
        assert_eq!(headers["x-enterprise-id"], "test-enterprise");
        assert_eq!(headers["x-domain"], "test-domain");
        assert_eq!(headers["x-codebuddy-request"], "1");
        for key in ["x-product", "x-ide-name", "x-ide-type"] {
            assert_eq!(headers[key], "WorkBuddy");
        }
        assert_eq!(headers["x-ide-version"], CLIENT_VERSION);
        assert_eq!(headers["x-agent-purpose"], "conversation");
        assert!(headers["accept"]
            .to_str()
            .unwrap()
            .contains("text/event-stream"));
        assert!(!headers.contains_key("x-refresh-token"));
        assert_eq!(
            serde_json::from_slice::<Value>(request.body().unwrap().as_bytes().unwrap()).unwrap(),
            body
        );
    }

    #[test]
    fn global_and_unknown_identity_headers_follow_reference_contract() {
        let mut account = fake_account();
        account.realm = "global".into();
        account.workbuddy_uid = None;
        let request = chat_post(
            &Client::new(),
            "http://127.0.0.1:1",
            &account,
            &serde_json::json!({}),
        )
        .build()
        .unwrap();
        let headers = request.headers();
        assert!(!headers.contains_key("x-user-id"));
        assert!(!headers.contains_key("x-machine-id"));
        assert!(!headers.contains_key("x-session-id"));
        assert_eq!(headers["x-no-user-id"], "1");
        assert_eq!(headers["x-no-enterprise-id"], "1");
        assert!(!headers.contains_key("x-enterprise-id"));
        assert_eq!(headers["x-domain"], "www.workbuddy.ai");
        assert_eq!(headers["accept-language"], "en-US");
    }

    #[test]
    fn rotation_keeps_turn_headers_and_changes_message_identity() {
        let client = Client::new();
        let account = fake_account();
        let body = serde_json::json!({"stream":true});
        let session = ChatSession::new(Some("client-conversation"));
        let first =
            chat_post_with_session(&client, "http://127.0.0.1:1", &account, &body, &session)
                .build()
                .unwrap();
        let second =
            chat_post_with_session(&client, "http://127.0.0.1:1", &account, &body, &session)
                .build()
                .unwrap();
        assert_eq!(first.headers()["x-conversation-id"], "client-conversation");
        for key in [
            "x-conversation-id",
            "x-conversation-request-id",
            "x-root-request-id",
            "x-trace-id",
        ] {
            assert_eq!(first.headers()[key], second.headers()[key]);
        }
        assert_ne!(
            first.headers()["x-request-id"],
            second.headers()["x-request-id"]
        );
        assert_eq!(
            first.headers()["x-conversation-message-id"],
            first.headers()["x-request-id"]
        );
        let anonymous = chat_post(&client, "http://127.0.0.1:1", &account, &body)
            .build()
            .unwrap();
        assert_eq!(anonymous.headers()["x-conversation-id"].len(), 32);
    }

    #[test]
    fn account_headers_are_stable_for_real_identity_and_isolated_between_accounts() {
        let client = Client::new();
        let mut account = fake_account();
        let body = serde_json::json!({"stream":true});
        let make = |account: &AccountConfig| {
            chat_post(&client, "http://127.0.0.1:1", account, &body)
                .build()
                .unwrap()
        };
        let first = make(&account);
        // A changed local index does not change the upstream account identity.
        account.uid = "acct-another-local-index".into();
        let second = make(&account);
        for key in ["x-machine-id", "x-session-id"] {
            assert_eq!(first.headers()[key], second.headers()[key]);
            assert_eq!(first.headers()[key].len(), 36);
            assert!(first.headers()[key]
                .as_bytes()
                .iter()
                .all(u8::is_ascii_hexdigit));
        }
        assert_ne!(
            first.headers()["x-machine-id"],
            first.headers()["x-session-id"]
        );
        account.workbuddy_uid = Some("different-official-test-uid".into());
        let other = make(&account);
        assert_ne!(
            first.headers()["x-machine-id"],
            other.headers()["x-machine-id"]
        );
        assert_ne!(
            first.headers()["x-session-id"],
            other.headers()["x-session-id"]
        );
    }

    #[test]
    fn session_preserves_metadata_camel_case_and_inbound_turn_headers() {
        let body = serde_json::json!({"metadata":{"conversationId":"metadata-dialog"},
            "conversation_id":"root-snake", "conversationId":"root-camel"});
        assert_eq!(conversation_id(&body), Some("metadata-dialog"));
        assert_eq!(
            conversation_id(&serde_json::json!({"conversationId":"camel-dialog"})),
            Some("camel-dialog")
        );
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-conversation-request-id",
            "client-turn-with-dashes".parse().unwrap(),
        );
        headers.insert("x-trace-id", "client-trace".parse().unwrap());
        let session = ChatSession::with_headers(conversation_id(&body), &headers);
        let request = chat_post_with_session(
            &Client::new(),
            "http://127.0.0.1:1",
            &fake_account(),
            &body,
            &session,
        )
        .build()
        .unwrap();
        let sent = request.headers();
        assert_eq!(sent["x-conversation-id"], "metadata-dialog");
        assert_eq!(sent["x-conversation-request-id"], "client-turn-with-dashes");
        assert_eq!(sent["x-trace-id"], "client-trace");
        assert_eq!(sent["x-root-request-id"], "client-turn-with-dashes");
        assert_eq!(sent["x-b3-traceid"], sent["x-request-id"]);
        assert_eq!(sent["x-b3-traceid"].len(), 32);
    }

    #[test]
    fn global_system_fallback_is_account_specific_and_does_not_change_client_messages() {
        let client = Client::new();
        let mut account = fake_account();
        let original =
            serde_json::json!({"messages":[{"role":"user","content":"ping"}],"stream":true});
        let sent = |account: &AccountConfig, body: &Value| {
            let request = chat_post(&client, "http://127.0.0.1:1", account, body)
                .build()
                .unwrap();
            serde_json::from_slice::<Value>(request.body().unwrap().as_bytes().unwrap()).unwrap()
        };
        assert_eq!(sent(&account, &original), original);
        account.realm = "global".into();
        let prepared = sent(&account, &original);
        assert_eq!(
            prepared["messages"][0],
            serde_json::json!({"role":"system","content":"You are a helpful assistant."})
        );
        assert_eq!(prepared["messages"][1], original["messages"][0]);
        assert_eq!(original["messages"].as_array().unwrap().len(), 1);
        let instructed = serde_json::json!({"messages":[{"role":"system","content":"User instructions"},{"role":"user","content":"ping"}]});
        assert_eq!(sent(&account, &instructed), instructed);
    }
}
