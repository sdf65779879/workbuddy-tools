//! WorkBuddy 登录态字段加解密（AES-256-GCM）。
//!
//! 参考 cockpit-tools 的实现：WorkBuddy 5.6+ 把登录态文件
//! `workbuddy-desktop.info` 里的敏感字段用 AES-256-GCM 加密，格式为
//! `{"$wbEncrypted": 1, "envelope": "<base64>"}`。
//!
//! 加密密钥通过运行 WorkBuddy 官方 Electron 客户端获取：
//!   ELECTRON_RUN_AS_NODE=1 WorkBuddy.exe -e "<script>"
//! 脚本调用 `process._linkedBinding('electron_browser_workbuddy_storage').loggerGet()`
//! 拿到 `atRestSecretKey`（base64，32 字节），密钥 = sha256(atRestSecretKey 字符串)。

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

const KEY_TIMEOUT: Duration = Duration::from_secs(5);
const KEY_TTL: Duration = Duration::from_secs(60);
const PREPARING: &str = "WorkBuddy 官方加密密钥正在准备，请稍后重试";
const KEY_MISMATCH: &str = "WorkBuddy 官方加密密钥已变化，正在重新准备，请稍后重试";
const INVALID: &str =
    "WorkBuddy 官方加密字段校验失败，已停止覆盖，请更新或重新登录官方客户端后重试";
const UNSUPPORTED: &str = "WorkBuddy 官方加密格式不受支持，已停止覆盖";

const KEY_SCRIPT: &str = r#"
try {
  const c = require('crypto');
  const p = JSON.parse(process._linkedBinding('electron_browser_workbuddy_storage').loggerGet());
  if (p.version !== 1 || typeof p.atRestSecretKey !== 'string') process.exit(2);
  const b = Buffer.from(p.atRestSecretKey, 'base64');
  if (b.length !== 32 || b.toString('base64') !== p.atRestSecretKey || b.every(x => x === 0)) process.exit(2);
  const key = c.createHash('sha256').update(p.atRestSecretKey, 'utf8').digest();
  process.stdout.write(key.toString('base64'));
  key.fill(0); b.fill(0);
} catch (_) { process.exit(2); }
"#;

struct OfficialKey {
    bytes: [u8; 32],
    id: String,
}

impl OfficialKey {
    fn new(bytes: [u8; 32]) -> Self {
        let id = format!("{:x}", Sha256::digest(bytes))[..16].to_owned();
        Self { bytes, id }
    }

    fn aad(&self) -> Vec<u8> {
        let mut bytes = b"WB-AAD\0\x01".to_vec();
        for value in ["WBEV1", "sym-v1"] {
            bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
            bytes.extend_from_slice(value.as_bytes());
        }
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&(self.id.len() as u32).to_be_bytes());
        bytes.extend_from_slice(self.id.as_bytes());
        bytes.extend_from_slice(&[2, 0, 0]);
        bytes
    }

    fn open(&self, wrapper: &Value) -> Result<String, String> {
        let obj = wrapper.as_object().ok_or(INVALID)?;
        if obj.len() != 2 || obj.get("$wbEncrypted") != Some(&json!(1)) {
            return Err(UNSUPPORTED.into());
        }
        let encoded = obj.get("envelope").and_then(Value::as_str).ok_or(INVALID)?;
        let envelope: Envelope =
            serde_json::from_slice(&decode_base64(encoded)?).map_err(|_| INVALID.to_string())?;
        if envelope.suite != 1 {
            return Err(UNSUPPORTED.into());
        }
        if envelope.key_id != self.id {
            return Err(KEY_MISMATCH.into());
        }
        let nonce = decode_base64(&envelope.nonce)?;
        let tag = decode_base64(&envelope.auth_tag)?;
        if nonce.len() != 12 || tag.len() != 16 {
            return Err(INVALID.into());
        }
        let mut ciphertext = decode_base64(&envelope.ciphertext)?;
        ciphertext.extend_from_slice(&tag);
        let cipher = Aes256Gcm::new_from_slice(&self.bytes).map_err(|_| INVALID)?;
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &self.aad(),
                },
            )
            .map_err(|_| INVALID)?;
        String::from_utf8(plaintext).map_err(|_| INVALID.into())
    }

    fn seal(&self, plaintext: &str) -> Result<Value, String> {
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let cipher = Aes256Gcm::new_from_slice(&self.bytes).map_err(|_| INVALID)?;
        let sealed = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: &self.aad(),
                },
            )
            .map_err(|_| INVALID)?;
        let split = sealed.len() - 16;
        let envelope = Envelope {
            suite: 1,
            key_id: self.id.clone(),
            nonce: STANDARD.encode(nonce),
            auth_tag: STANDARD.encode(&sealed[split..]),
            ciphertext: STANDARD.encode(&sealed[..split]),
        };
        let bytes = serde_json::to_vec(&envelope).map_err(|_| INVALID)?;
        Ok(json!({ "$wbEncrypted": 1, "envelope": STANDARD.encode(bytes) }))
    }
}

impl Drop for OfficialKey {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Envelope {
    suite: u32,
    key_id: String,
    nonce: String,
    auth_tag: String,
    ciphertext: String,
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>, String> {
    if encoded.len() > 16 * 1024 * 1024 {
        return Err(INVALID.into());
    }
    let bytes = STANDARD.decode(encoded).map_err(|_| INVALID)?;
    if STANDARD.encode(&bytes) != encoded {
        return Err(INVALID.into());
    }
    Ok(bytes)
}

#[derive(Default)]
struct KeyState {
    key: Option<Arc<OfficialKey>>,
    loaded_at: Option<Instant>,
    loading: bool,
    failure: Option<(Instant, String)>,
}
static KEY_STATE: LazyLock<(Mutex<KeyState>, Condvar)> =
    LazyLock::new(|| (Mutex::new(KeyState::default()), Condvar::new()));

/// 定位 WorkBuddy 桌面版可执行文件路径。
///
/// 优先级：
///   1. 用户自定义环境变量 `WORKBUDDY_EXE_PATH`（直接指向 WorkBuddy.exe）；
///   2. 官方环境变量推导：WorkBuddy 运行时自带 `WORKBUDDY_RESOURCES_PATH`
///      （如 `S:\WorkBuddy\resources`），其父目录即安装根目录；
///   3. 常见候选路径；
///   4. 注册表 Uninstall 探测。
pub fn resolve_workbuddy_exec_path() -> Result<PathBuf, String> {
    // 1. 用户自定义（避免与官方 WORKBUDDY_APP_PATH 冲突）
    if let Ok(custom) = std::env::var("WORKBUDDY_EXE_PATH") {
        let p = PathBuf::from(custom.trim());
        if p.is_file() && is_workbuddy_exe_name(&p) {
            return Ok(p);
        }
    }

    // 2. 官方环境变量推导安装根目录
    if let Ok(res) = std::env::var("WORKBUDDY_RESOURCES_PATH") {
        let root = PathBuf::from(res.trim());
        let exe = root.parent().map(|r| r.join("WorkBuddy.exe"));
        if let Some(p) = exe {
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    // WORKBUDDY_APP_PATH 通常指向 resources/app.asar，取其父目录的父目录
    if let Ok(app) = std::env::var("WORKBUDDY_APP_PATH") {
        let app = PathBuf::from(app.trim());
        if let Some(resources) = app.parent() {
            if let Some(root) = resources.parent() {
                let exe = root.join("WorkBuddy.exe");
                if exe.is_file() {
                    return Ok(exe);
                }
            }
        }
    }

    // 3. 常见候选路径
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join("Programs")
                .join("WorkBuddy")
                .join("WorkBuddy.exe"),
        );
    }
    if let Ok(pf) = std::env::var("PROGRAMFILES") {
        candidates.push(PathBuf::from(pf).join("WorkBuddy").join("WorkBuddy.exe"));
    }
    for c in candidates {
        if c.is_file() {
            return Ok(c);
        }
    }

    // 4. 从注册表 Uninstall 探测（Windows）
    #[cfg(target_os = "windows")]
    if let Some(p) = detect_from_registry() {
        return Ok(p);
    }

    // 5. 兜底：已知目录
    #[cfg(target_os = "windows")]
    if let Some(p) = detect_running_workbuddy_path() {
        return Ok(p);
    }

    Err(
        "未找到 WorkBuddy 桌面版可执行文件，请设置 WORKBUDDY_EXE_PATH 环境变量指向 WorkBuddy.exe"
            .to_string(),
    )
}

fn is_workbuddy_exe_name(p: &PathBuf) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("workbuddy.exe"))
}

/// 从注册表 HKCU\...\Uninstall 探测 WorkBuddy 安装路径（DisplayIcon 字段）。
#[cfg(target_os = "windows")]
fn detect_from_registry() -> Option<PathBuf> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let uninstall = hkcu
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Uninstall")
        .ok()?;
    for key in uninstall.enum_keys().flatten() {
        let sub = uninstall.open_subkey(&key).ok()?;
        let name: String = sub.get_value("DisplayName").unwrap_or_default();
        if !name.to_lowercase().contains("workbuddy") {
            continue;
        }
        // DisplayIcon 形如 "S:\WorkBuddy\WorkBuddy.exe,0"
        let icon: String = sub.get_value("DisplayIcon").unwrap_or_default();
        if !icon.is_empty() {
            let exe = icon.split(',').next().unwrap_or("").trim();
            let p = PathBuf::from(exe);
            if is_workbuddy_exe(&p) {
                return Some(p);
            }
        }
        // InstallLocation 兜底
        let loc: String = sub.get_value("InstallLocation").unwrap_or_default();
        if !loc.is_empty() {
            let p = PathBuf::from(&loc).join("WorkBuddy.exe");
            if is_workbuddy_exe(&p) {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn is_workbuddy_exe(p: &PathBuf) -> bool {
    p.is_file()
        && p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("workbuddy.exe"))
}

#[cfg(target_os = "windows")]
fn detect_running_workbuddy_path() -> Option<PathBuf> {
    // 兜底：尝试从注册表 App Paths 或已知目录
    let known = [
        r"C:\Program Files\WorkBuddy\WorkBuddy.exe",
        r"C:\Program Files (x86)\WorkBuddy\WorkBuddy.exe",
    ];
    for k in known {
        let p = PathBuf::from(k);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn load_official_key() -> Result<OfficialKey, String> {
    let executable = resolve_workbuddy_exec_path()?;
    let mut command = Command::new(&executable);
    command
        .env("ELECTRON_RUN_AS_NODE", "1")
        .env_remove("NODE_OPTIONS")
        .args(["-e", KEY_SCRIPT])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }

    let output = command.output().map_err(|e| {
        format!(
            "无法运行 WorkBuddy 官方客户端获取密钥 ({}) : {}",
            executable.display(),
            e
        )
    })?;
    if !output.status.success() {
        return Err(format!(
            "当前 WorkBuddy 客户端未提供可用的官方密钥接口（退出码 {}），请更新客户端后重试",
            output.status
        ));
    }
    if output.stdout.len() != 44 {
        return Err(format!(
            "WorkBuddy 官方密钥接口返回异常（长度 {}，预期 44）",
            output.stdout.len()
        ));
    }
    let encoded = std::str::from_utf8(&output.stdout).map_err(|_| INVALID)?;
    let decoded = decode_base64(encoded)?;
    let key = <[u8; 32]>::try_from(decoded.as_slice())
        .map(OfficialKey::new)
        .map_err(|_| INVALID.to_string())?;
    Ok(key)
}

fn finish_load(result: Result<OfficialKey, String>) -> Result<(), String> {
    let (lock, changed) = &*KEY_STATE;
    let mut state = lock.lock().map_err(|_| INVALID)?;
    state.loading = false;
    let result = match result {
        Ok(key) => {
            state.key = Some(Arc::new(key));
            state.loaded_at = Some(Instant::now());
            state.failure = None;
            Ok(())
        }
        Err(error) => {
            state.failure = Some((Instant::now(), error.clone()));
            Err(error)
        }
    };
    changed.notify_all();
    result
}

/// 阻塞式准备密钥（供切号等显式操作调用）。
pub fn prepare() -> Result<(), String> {
    let (lock, changed) = &*KEY_STATE;
    let mut state = lock.lock().map_err(|_| INVALID)?;
    if state.loading {
        let (next, wait) = changed
            .wait_timeout_while(state, KEY_TIMEOUT + Duration::from_secs(1), |s| s.loading)
            .map_err(|_| INVALID)?;
        state = next;
        if wait.timed_out() && state.loading {
            return Err(PREPARING.into());
        }
    }
    if let Some((at, error)) = &state.failure {
        if at.elapsed() < Duration::from_secs(1) {
            return Err(error.clone());
        }
    }
    if state.loaded_at.is_some_and(|at| at.elapsed() < KEY_TTL) && state.key.is_some() {
        return Ok(());
    }
    state.loading = true;
    drop(state);
    finish_load(load_official_key())
}

fn cached_key() -> Result<Arc<OfficialKey>, String> {
    let (lock, _) = &*KEY_STATE;
    let mut state = lock.lock().map_err(|_| INVALID)?;
    if let Some(key) = &state.key {
        return Ok(key.clone());
    }
    if let Some((at, error)) = &state.failure {
        if at.elapsed() < Duration::from_secs(1) {
            return Err(error.clone());
        }
    }
    if !state.loading {
        state.loading = true;
        drop(state);
        if std::thread::Builder::new()
            .name("workbuddy-auth-key".into())
            .spawn(|| {
                let _ = finish_load(load_official_key());
            })
            .is_err()
        {
            let _ = finish_load(Err(PREPARING.into()));
        }
    }
    Err(PREPARING.into())
}

pub fn contains_encrypted_wrapper(value: &Value) -> bool {
    match value {
        Value::Object(obj) => {
            obj.contains_key("$wbEncrypted") || obj.values().any(contains_encrypted_wrapper)
        }
        Value::Array(items) => items.iter().any(contains_encrypted_wrapper),
        _ => false,
    }
}

fn decode_field(value: &mut Value, name: &str, key: &OfficialKey) -> Result<(), String> {
    if let Some(field) = value.get_mut(name) {
        if field.get("$wbEncrypted").is_some() {
            *field = Value::String(key.open(field)?);
        }
    }
    Ok(())
}

fn decode_with_key(value: &mut Value, key: &OfficialKey) -> Result<(), String> {
    if let Some(auth) = value.get_mut("auth") {
        for name in ["accessToken", "refreshToken"] {
            decode_field(auth, name, key)?;
        }
    }
    if let Some(account) = value.get_mut("account") {
        for name in ["phoneNumber", "departmentFullName", "nickname"] {
            decode_field(account, name, key)?;
        }
    }
    for name in ["accounts", "allAccounts"] {
        if let Some(items) = value.get_mut(name).and_then(Value::as_array_mut) {
            for item in items {
                for field in ["phoneNumber", "departmentFullName", "nickname"] {
                    decode_field(item, field, key)?;
                }
            }
        }
    }
    if contains_encrypted_wrapper(value) {
        return Err(UNSUPPORTED.into());
    }
    Ok(())
}

pub fn decode_session(value: &Value) -> Result<Value, String> {
    let mut decoded = value.clone();
    if contains_encrypted_wrapper(value) {
        let key = cached_key()?;
        if let Err(error) = decode_with_key(&mut decoded, &key) {
            if error == KEY_MISMATCH {
                // 密钥不匹配时清缓存，下次重试
                let (lock, _) = &*KEY_STATE;
                if let Ok(mut state) = lock.lock() {
                    state.key = None;
                    state.loaded_at = None;
                }
            }
            return Err(error);
        }
    }
    Ok(decoded)
}

fn encode_account(value: &mut Value, key: &OfficialKey) -> Result<(), String> {
    for name in ["phoneNumber", "departmentFullName", "nickname"] {
        encode_field(value, name, key)?;
    }
    Ok(())
}

fn encode_field(value: &mut Value, name: &str, key: &OfficialKey) -> Result<(), String> {
    if let Some(field) = value.get_mut(name) {
        if let Some(plaintext) = field.as_str() {
            *field = key.seal(plaintext)?;
        } else if !field.is_null() {
            return Err(INVALID.into());
        }
    }
    Ok(())
}

fn encode_with_key(value: &Value, key: &OfficialKey) -> Result<Value, String> {
    if contains_encrypted_wrapper(value) {
        return Err(INVALID.into());
    }
    let mut encoded = value.clone();
    if let Some(auth) = encoded.get_mut("auth") {
        for name in ["accessToken", "refreshToken"] {
            encode_field(auth, name, key)?;
        }
    }
    if let Some(account) = encoded.get_mut("account") {
        encode_account(account, key)?;
    }
    for name in ["accounts", "allAccounts"] {
        if let Some(items) = encoded.get_mut(name).and_then(Value::as_array_mut) {
            for item in items {
                encode_account(item, key)?;
            }
        }
    }
    Ok(encoded)
}

pub fn encode_session(value: &Value) -> Result<Value, String> {
    encode_with_key(value, cached_key()?.as_ref())
}

/// 供切号流程使用：确保密钥已准备（阻塞式），然后返回 encode/decode 可用。
pub fn prepare_for_switch() -> Result<(), String> {
    prepare()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 来自 cockpit-tools 的官方 Node crypto fixture，密钥 [7;32]、nonce [3;12]。
    // 期望明文 "fixture-token-测试"。
    fn fixture() -> Value {
        let envelope = r#"{"suite":1,"keyId":"4bb06f8e4e3a7715","nonce":"AwMDAwMDAwMDAwMD","authTag":"8mXI80XfjtTaNENkKD5E5Q==","ciphertext":"Q5fbdy9aO28OLyg5hXoZuWbUT3o="}"#;
        json!({"$wbEncrypted":1,"envelope":STANDARD.encode(envelope)})
    }

    #[test]
    fn matches_official_node_framing() {
        assert_eq!(
            OfficialKey::new([7; 32]).open(&fixture()).unwrap(),
            "fixture-token-测试"
        );
    }

    #[test]
    fn rejects_wrong_key() {
        assert!(OfficialKey::new([8; 32]).open(&fixture()).is_err());
    }

    #[test]
    fn seal_open_roundtrip() {
        let key = OfficialKey::new([7; 32]);
        let sealed = key.seal("hello-测试-token").unwrap();
        assert_eq!(key.open(&sealed).unwrap(), "hello-测试-token");
        // 每次 seal 用新 nonce，密文不同
        assert_ne!(key.seal("x").unwrap(), key.seal("x").unwrap());
    }
}
