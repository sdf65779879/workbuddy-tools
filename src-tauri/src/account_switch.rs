//! 切换 WorkBuddy 桌面版当前使用的账号。
//!
//! 参考 cockpit-tools 的切号机制：真正的登录态存放在
//! `workbuddy-desktop.info` 文件里（Windows 为
//! `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`），
//! 其中 accessToken / refreshToken / nickname 等敏感字段用 AES-256-GCM 加密。
//!
//! 切换流程：
//!   1. 读取并解密现有登录态（保留 account/accounts/allAccounts 结构）；
//!   2. 用目标账号的 accessToken（+ refreshToken / uid / nickname / email）替换；
//!   3. 重新加密写回（原子写 + 回读校验）。

use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::config::AccountConfig;
use crate::workbuddy_auth_crypto;

#[derive(Debug, Clone, Serialize)]
pub struct SwitchResult {
    pub ok: bool,
    pub uid: String,
    pub nickname: String,
    pub auth_file: String,
    pub message: String,
}

const AUTH_FILE_NAME: &str = "workbuddy-desktop.info";

/// 定位登录态文件路径（跨平台）。
pub fn auth_file_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "无法定位用户主目录".to_string())?;

    #[cfg(target_os = "macos")]
    {
        return Ok(home
            .join("Library")
            .join("Application Support")
            .join("CodeBuddyExtension")
            .join("Data")
            .join("Public")
            .join("auth")
            .join(AUTH_FILE_NAME));
    }

    #[cfg(target_os = "windows")]
    {
        let local = std::env::var("LOCALAPPDATA")
            .ok()
            .map(PathBuf::from)
            .or_else(|| Some(home.join("AppData").join("Local")));
        if let Some(base) = local {
            return Ok(base
                .join("CodeBuddyExtension")
                .join("Data")
                .join("Public")
                .join("auth")
                .join(AUTH_FILE_NAME));
        }
    }

    #[cfg(target_os = "linux")]
    {
        return Ok(home
            .join(".local")
            .join("share")
            .join("CodeBuddyExtension")
            .join("Data")
            .join("Public")
            .join("auth")
            .join(AUTH_FILE_NAME));
    }

    #[allow(unreachable_code)]
    Err("当前平台不支持定位 WorkBuddy 登录态路径".to_string())
}

/// 构造一个「当前账号」的 account 对象（合并已有 account 字段）。
fn build_account_value(
    account: &AccountConfig,
    official_uid: &str,
    existing: Option<&Value>,
) -> Value {
    let mut obj = existing
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    obj.insert("uid".to_string(), json!(official_uid));
    obj.insert("nickname".to_string(), json!(account.nickname));
    if !account.email.is_empty() {
        obj.insert("email".to_string(), json!(account.email));
    }
    if !account.enterprise_id.is_empty() {
        obj.insert("enterpriseId".to_string(), json!(account.enterprise_id));
    }
    if !account.enterprise_name.is_empty() {
        obj.insert("enterpriseName".to_string(), json!(account.enterprise_name));
    }
    obj.entry("type".to_string())
        .or_insert_with(|| json!("personal"));
    obj.entry("accountType".to_string())
        .or_insert_with(|| json!(""));
    obj.entry("idp".to_string()).or_insert_with(|| json!(""));
    obj.entry("oneidAccountId".to_string())
        .or_insert_with(|| json!(""));
    obj.entry("areaInfoComplete".to_string())
        .or_insert_with(|| json!(false));
    obj.entry("isCurrentOneIdEnterprise".to_string())
        .or_insert_with(|| json!(false));
    obj.entry("isFirstLogin".to_string())
        .or_insert_with(|| json!(false));
    obj.insert("lastLogin".to_string(), json!(true));
    obj.entry("pluginEnabled".to_string())
        .or_insert_with(|| json!(true));
    obj.entry("deployStatus".to_string())
        .or_insert_with(|| json!({ "statusCode": 0, "statusMsg": "", "detailMsg": "" }));
    obj.entry("sso".to_string())
        .or_insert_with(|| json!({ "domain": "", "domainModifiedTimes": 0 }));

    Value::Object(obj)
}

/// 构造 auth 对象（合并已有 auth 字段）。
fn build_auth_value(account: &AccountConfig, existing: Option<&Value>) -> Value {
    let mut obj = existing
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    obj.insert("accessToken".to_string(), json!(account.token));
    // 仅当账号数据里有 refresh_token 时才覆盖，否则保留现有值（避免丢失续期凭据）
    if !account.refresh_token.trim().is_empty() {
        obj.insert("refreshToken".to_string(), json!(account.refresh_token));
    }
    obj.entry("tokenType".to_string())
        .or_insert_with(|| json!("Bearer"));
    if !account.domain.is_empty() {
        obj.insert("domain".to_string(), json!(account.domain));
    }
    obj.entry("domain".to_string()).or_insert_with(|| json!(""));
    if let Some(expires_at) = account.expires_at {
        obj.insert("expiresAt".to_string(), json!(expires_at));
    }
    obj.insert("lastRefreshTime".to_string(), json!(now_ms));
    obj.entry("scope".to_string())
        .or_insert_with(|| json!("openid profile offline_access email"));

    Value::Object(obj)
}

/// 构造新的登录态 session（保留其它账号）。
fn build_session(account: &AccountConfig, official_uid: &str, existing: &Value) -> Value {
    let existing_obj = existing.as_object();
    let matches_target =
        |item: &Value| item.get("uid").and_then(Value::as_str) == Some(official_uid);
    let current_account = existing.get("account").filter(|item| matches_target(item));
    let target_account = current_account.or_else(|| {
        ["accounts", "allAccounts"].into_iter().find_map(|key| {
            existing
                .get(key)?
                .as_array()?
                .iter()
                .find(|item| matches_target(item))
        })
    });
    let account_value = build_account_value(account, official_uid, target_account);
    let auth_value = build_auth_value(account, current_account.and_then(|_| existing.get("auth")));

    let merge_current = |items: Option<&Value>| -> Vec<Value> {
        let mut found = false;
        let mut merged: Vec<Value> = items
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|mut item| {
                if matches_target(&item) {
                    if found {
                        return None;
                    }
                    found = true;
                    return Some(account_value.clone());
                }
                if account.uid != official_uid
                    && item.get("uid").and_then(Value::as_str) == Some(account.uid.as_str())
                {
                    return None;
                }
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("lastLogin".to_string(), json!(false));
                }
                Some(item)
            })
            .collect();
        if !found {
            merged.push(account_value.clone());
        }
        merged
    };

    let accounts = merge_current(existing_obj.and_then(|o| o.get("accounts")));
    let all_accounts = merge_current(existing_obj.and_then(|o| o.get("allAccounts")));

    let mut session = existing_obj.cloned().unwrap_or_default();
    session.insert("account".to_string(), account_value);
    session.insert("auth".to_string(), auth_value);
    session.insert("accounts".to_string(), Value::Array(accounts));
    session.insert("allAccounts".to_string(), Value::Array(all_accounts));
    Value::Object(session)
}

/// 执行账号切换。
pub fn switch_account(account: &AccountConfig) -> Result<SwitchResult, String> {
    let official_uid = account
        .official_uid()
        .ok_or_else(|| "此账号缺少真实 WorkBuddy UID，请先在账号保险柜补录 UID".to_string())?;
    let auth_file = auth_file_path()?;

    // 1. 读取现有登录态
    let existing = if auth_file.exists() {
        Some(
            std::fs::read_to_string(&auth_file)
                .map_err(|e| format!("读取 WorkBuddy 登录态失败: {}", e))?,
        )
    } else {
        None
    };

    let existing_json = existing
        .as_deref()
        .map(serde_json::from_str::<Value>)
        .transpose()
        .map_err(|e| format!("现有 WorkBuddy 登录态不是有效 JSON: {}", e))?;

    // 2. 判断是否需要加密
    let encrypt = existing_json
        .as_ref()
        .is_some_and(workbuddy_auth_crypto::contains_encrypted_wrapper);

    // 3. 解密现有登录态（如果需要）
    let decoded_existing = if encrypt {
        workbuddy_auth_crypto::prepare_for_switch()?;
        existing_json
            .as_ref()
            .map(workbuddy_auth_crypto::decode_session)
            .transpose()?
    } else {
        existing_json.clone()
    };

    // 4. 构造新 session
    let session = match decoded_existing.as_ref() {
        Some(existing) => build_session(account, official_uid, existing),
        None => build_session(account, official_uid, &json!({})),
    };

    // 5. 重新加密（如果需要）
    let session = if encrypt {
        workbuddy_auth_crypto::encode_session(&session)?
    } else {
        session
    };

    let content =
        serde_json::to_string_pretty(&session).map_err(|e| format!("序列化登录态失败: {}", e))?;

    // 6. 写入（先确保目录存在）
    if let Some(dir) = auth_file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建登录态目录失败: {}", e))?;
    }

    // 先备份
    if auth_file.exists() {
        let backup = auth_file.with_extension("info.bak");
        let _ = std::fs::copy(&auth_file, &backup);
    }

    std::fs::write(&auth_file, &content)
        .map_err(|e| format!("写入 WorkBuddy 登录态失败: {}", e))?;

    // 7. 回读校验
    let written = std::fs::read_to_string(&auth_file)
        .map_err(|e| format!("校验 WorkBuddy 登录态失败: {}", e))?;
    let written_json: Value = serde_json::from_str(&written)
        .map_err(|e| format!("校验 WorkBuddy 登录态 JSON 失败: {}", e))?;
    let written_json = workbuddy_auth_crypto::decode_session(&written_json)?;
    let written_token = written_json
        .get("auth")
        .and_then(|a| a.get("accessToken"))
        .and_then(|v| v.as_str());
    let written_uid = written_json.pointer("/account/uid").and_then(Value::as_str);
    if written_token != Some(account.token.as_str()) || written_uid != Some(official_uid) {
        return Err(format!(
            "校验 WorkBuddy 登录态失败，未写入目标账号: {}",
            auth_file.display()
        ));
    }

    Ok(SwitchResult {
        ok: true,
        uid: account.uid.clone(),
        nickname: account.nickname.clone(),
        auth_file: auth_file.display().to_string(),
        message: format!(
            "已切换到账号「{}」，已写入登录态。重启 WorkBuddy 后生效。",
            account.nickname
        ),
    })
}

fn read_current_session() -> Result<Option<Value>, String> {
    let auth_file = auth_file_path()?;
    if !auth_file.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&auth_file)
        .map_err(|e| format!("读取 WorkBuddy 登录态失败: {}", e))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("解析 WorkBuddy 登录态失败: {}", e))?;

    // 显式读取：若含加密字段，先阻塞准备密钥
    if workbuddy_auth_crypto::contains_encrypted_wrapper(&value) {
        workbuddy_auth_crypto::prepare_for_switch()?;
    }

    workbuddy_auth_crypto::decode_session(&value).map(Some)
}

pub fn read_current_access_token() -> Result<Option<String>, String> {
    Ok(read_current_session()?.and_then(|session| {
        session
            .pointer("/auth/accessToken")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }))
}

fn account_from_session(session: &Value) -> Result<Option<AccountConfig>, String> {
    let field = |path: &str| {
        session
            .pointer(path)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
    };
    let token = field("/auth/accessToken");
    if token.is_empty() {
        return Ok(None);
    }
    let uid = field("/account/uid");
    if !AccountConfig::valid_workbuddy_uid(uid) {
        return Err(
            "当前 WorkBuddy 登录态缺少真实 UID，请在官方客户端重新登录后导入，或手动填写真实 UID"
                .into(),
        );
    }
    let domain = field("/auth/domain");
    let global = match field("/auth/realm") {
        "global" => true,
        "cn" => false,
        _ => matches!(
            domain,
            "www.workbuddy.ai" | "workbuddy.ai" | "copilot.codebuddy.ai"
        ),
    };
    let expires_at = session
        .pointer("/auth/expiresAt")
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|value| *value > 0);
    Ok(Some(AccountConfig {
        uid: uid.to_owned(),
        workbuddy_uid: Some(uid.to_owned()),
        nickname: field("/account/nickname").to_owned(),
        token: token.to_owned(),
        refresh_token: field("/auth/refreshToken").to_owned(),
        email: field("/account/email").to_owned(),
        login_ip: crate::browser_auth::observed_login_ip(session),
        import_ip: None,
        enterprise_id: field("/account/enterpriseId").to_owned(),
        enterprise_name: field("/account/enterpriseName").to_owned(),
        domain: domain.to_owned(),
        expires_at,
        realm: if global { "global" } else { "cn" }.to_owned(),
        enabled: true,
        paused: false,
    }))
}

pub fn read_current_account() -> Result<Option<AccountConfig>, String> {
    match read_current_session()? {
        Some(session) => account_from_session(&session),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_session_replaces_current_account() {
        let existing = json!({
            "account": {"uid": "old-uid", "nickname": "旧号", "email": "old@example.com"},
            "auth": {"accessToken": "old-token", "refreshToken": "old-refresh"},
            "accounts": [{"uid": "old-uid", "nickname": "旧号", "lastLogin": true}, {"uid": "acct-1234", "nickname": "历史条目"}],
            "allAccounts": [{"uid": "old-uid", "nickname": "旧号"}]
        });

        let acct = AccountConfig {
            uid: "acct-1234".into(),
            workbuddy_uid: Some("new-uid".into()),
            nickname: "新号".into(),
            token: "new-token".into(),
            refresh_token: "new-refresh".into(),
            email: "a@b.com".into(),
            login_ip: None,
            import_ip: None,
            enterprise_id: String::new(),
            enterprise_name: String::new(),
            domain: String::new(),
            expires_at: None,
            realm: "cn".into(),
            enabled: true,
            paused: false,
        };

        let session = build_session(&acct, acct.official_uid().unwrap(), &existing);
        assert_eq!(session["auth"]["accessToken"], "new-token");
        assert_eq!(session["auth"]["refreshToken"], "new-refresh");
        assert_eq!(session["account"]["uid"], "new-uid");
        assert_eq!(session["account"]["nickname"], "新号");
        // 旧账号仍在列表里，且 lastLogin 被置 false
        let accounts = session["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2);
        let old = accounts.iter().find(|a| a["uid"] == "old-uid").unwrap();
        assert_eq!(old["lastLogin"], false);
        assert_eq!(acct.uid, "acct-1234");
        assert!(!accounts.iter().any(|a| a["uid"] == "acct-1234"));

        let no_refresh = AccountConfig {
            refresh_token: String::new(),
            email: String::new(),
            ..acct
        };
        let session = build_session(&no_refresh, no_refresh.official_uid().unwrap(), &existing);
        assert!(session.pointer("/auth/refreshToken").is_none());
        assert!(session.pointer("/account/email").is_none());
    }

    #[test]
    fn local_import_reads_official_identity_and_matching_credentials() {
        let mut session = json!({
            "account": {"uid": "wb-user", "nickname": "本人", "enterpriseId": "ent-1", "lastLoginIp": "203.0.113.18"},
            "auth": {"accessToken": "test-access", "refreshToken": "test-refresh", "domain": "www.workbuddy.ai", "expiresAt": "1760000000000"}
        });
        let mut account = account_from_session(&session).unwrap().unwrap();
        assert_eq!(account.official_uid(), Some("wb-user"));
        assert_eq!(account.nickname, "本人");
        assert_eq!(account.token, "test-access");
        assert_eq!(account.refresh_token, "test-refresh");
        assert_eq!(account.enterprise_id, "ent-1");
        assert_eq!(account.expires_at, Some(1760000000000));
        assert_eq!(account.realm, "global");
        assert_eq!(account.login_ip.as_deref(), Some("203.0.113.18"));
        session["account"]
            .as_object_mut()
            .unwrap()
            .remove("lastLoginIp");
        session["account"]["ip"] = json!("203.0.113.99");
        assert!(account_from_session(&session)
            .unwrap()
            .unwrap()
            .login_ip
            .is_none());

        session["account"]["uid"] = json!("acct-1234");
        assert!(account_from_session(&session).is_err());
        account.uid = "acct-1234".into();
        account.workbuddy_uid = None;
        // Reject the generated identity before locating or opening the real auth file.
        assert!(switch_account(&account).unwrap_err().contains("补录 UID"));
    }

    /// 端到端「切号到自身」往返验证。需要已安装 WorkBuddy 客户端，手动运行：
    /// `cargo test --lib account_switch::tests::switch_roundtrip -- --ignored`
    #[test]
    #[ignore = "requires installed WorkBuddy client and writes real auth file"]
    fn switch_roundtrip() {
        let before = read_current_access_token()
            .unwrap()
            .expect("需要已登录的 WorkBuddy");
        // 从现有登录态读出 uid 和 refreshToken（验证切号不丢失 refreshToken）
        let auth_file = auth_file_path().unwrap();
        let text = std::fs::read_to_string(&auth_file).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let decoded = workbuddy_auth_crypto::decode_session(&value).unwrap();
        let uid = decoded["account"]["uid"].as_str().unwrap().to_string();
        let nickname = decoded["account"]["nickname"]
            .as_str()
            .unwrap_or("本机账号")
            .to_string();
        let refresh_before = decoded["auth"]["refreshToken"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let acct = AccountConfig {
            uid,
            workbuddy_uid: None,
            nickname,
            token: before.clone(),
            refresh_token: String::new(), // 故意留空，验证不会清空现有 refreshToken
            email: String::new(),
            enterprise_id: String::new(),
            login_ip: None,
            import_ip: None,
            enterprise_name: String::new(),
            domain: String::new(),
            expires_at: None,
            realm: "cn".into(),
            enabled: true,
            paused: false,
        };

        let result = switch_account(&acct).unwrap();
        assert!(result.ok);
        let after = read_current_access_token().unwrap().unwrap();
        assert_eq!(before, after, "切号到自身后 access token 应保持不变");

        // 验证 refreshToken 未被清空
        let text = std::fs::read_to_string(&auth_file).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let decoded = workbuddy_auth_crypto::decode_session(&value).unwrap();
        let refresh_after = decoded["auth"]["refreshToken"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert_eq!(refresh_before, refresh_after, "切号不应清空 refreshToken");
    }
}
