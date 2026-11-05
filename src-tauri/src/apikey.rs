//! API key 管理：创建 / 校验 / 配额 / 限速 / 过期 / IP 限制，JSON 文件持久化。
//! 与主密钥（config.master_key）并存；两者都为空时不鉴权（与旧语义一致）。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::sync::Mutex;
use uuid::Uuid;

pub const STATUS_ACTIVE: &str = "active";
pub const STATUS_INACTIVE: &str = "inactive";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub name: String,
    pub key: String,
    pub status: String,
    pub created_at: i64,
    #[serde(default)]
    pub last_used_at: Option<i64>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub ip_whitelist: Vec<String>,
    #[serde(default)]
    pub ip_blacklist: Vec<String>,
    #[serde(default)]
    pub quota: f64,
    #[serde(default)]
    pub quota_used: f64,
    #[serde(default)]
    pub rate_limit_5h: f64,
    #[serde(default)]
    pub rate_limit_1d: f64,
    #[serde(default)]
    pub rate_limit_7d: f64,
    #[serde(default)]
    pub usage_5h: f64,
    #[serde(default)]
    pub usage_1d: f64,
    #[serde(default)]
    pub usage_7d: f64,
    #[serde(default)]
    pub window_5h_start: Option<i64>,
    #[serde(default)]
    pub window_1d_start: Option<i64>,
    #[serde(default)]
    pub window_7d_start: Option<i64>,
    #[serde(default)]
    pub request_count: i64,
}

impl ApiKey {
    pub fn active(&self) -> bool {
        self.status == STATUS_ACTIVE
    }
    pub fn expired(&self, now_ms: i64) -> bool {
        self.expires_at.map(|e| now_ms > e).unwrap_or(false)
    }
    pub fn quota_exhausted(&self) -> bool {
        self.quota > 0.0 && self.quota_used >= self.quota
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateRequest {
    pub name: String,
    #[serde(default)]
    pub custom_key: String,
    #[serde(default)]
    pub ip_whitelist: Vec<String>,
    #[serde(default)]
    pub ip_blacklist: Vec<String>,
    #[serde(default)]
    pub quota: f64,
    #[serde(default)]
    pub expires_in_days: i64,
    #[serde(default)]
    pub rate_limit_5h: f64,
    #[serde(default)]
    pub rate_limit_1d: f64,
    #[serde(default)]
    pub rate_limit_7d: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub ip_whitelist: Option<Vec<String>>,
    #[serde(default)]
    pub ip_blacklist: Option<Vec<String>>,
    #[serde(default)]
    pub quota: Option<f64>,
    #[serde(default)]
    pub reset_quota: bool,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub clear_expiry: bool,
    #[serde(default)]
    pub rate_limit_5h: Option<f64>,
    #[serde(default)]
    pub rate_limit_1d: Option<f64>,
    #[serde(default)]
    pub rate_limit_7d: Option<f64>,
    #[serde(default)]
    pub reset_rate_limit_usage: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyResult {
    pub key: Option<ApiKey>,
    pub matched: bool,
    pub reason: String,
}

pub struct Store {
    path: String,
    keys: Mutex<Vec<ApiKey>>,
}

impl Store {
    #[allow(dead_code)]
    pub fn new(path: String) -> Result<Self, String> {
        let s = Self {
            path,
            keys: Mutex::new(Vec::new()),
        };
        s.load()?;
        Ok(s)
    }

    /// 加载失败时从空开始（不因 apikeys 文件损坏导致应用起不来）。
    pub fn new_or_empty(path: String) -> Self {
        let s = Self {
            path,
            keys: Mutex::new(Vec::new()),
        };
        let _ = s.load();
        s
    }

    fn load(&self) -> Result<(), String> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("读取 apikeys: {e}")),
        };
        if raw.trim().is_empty() {
            return Ok(());
        }
        let mut list: Vec<ApiKey> =
            serde_json::from_str(&raw).map_err(|e| format!("解析 apikeys: {e}"))?;
        for k in &mut list {
            if k.status.is_empty() {
                k.status = STATUS_ACTIVE.to_string();
            }
        }
        *self.keys.lock().unwrap() = list;
        Ok(())
    }

    fn persist(&self) -> Result<(), String> {
        let list = self.keys.lock().unwrap();
        let out = serde_json::to_string_pretty(&*list).map_err(|e| e.to_string())?;
        if let Some(dir) = std::path::Path::new(&self.path).parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
        }
        let tmp = format!("{}.tmp", self.path);
        std::fs::write(&tmp, out).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    pub fn list(&self) -> Vec<ApiKey> {
        let keys = self.keys.lock().unwrap();
        let mut out: Vec<ApiKey> = keys.iter().cloned().collect();
        out.reverse(); // 新的在前
        out
    }

    pub fn count(&self) -> usize {
        self.keys.lock().unwrap().len()
    }

    pub fn create(&self, req: CreateRequest) -> Result<ApiKey, String> {
        let name = req.name.trim().to_string();
        if name.is_empty() {
            return Err("名称不能为空".to_string());
        }
        if let Err(bad) = validate_ip_patterns(&req.ip_whitelist) {
            return Err(format!("IP 白名单格式非法: {bad}"));
        }
        if let Err(bad) = validate_ip_patterns(&req.ip_blacklist) {
            return Err(format!("IP 黑名单格式非法: {bad}"));
        }

        let key = if !req.custom_key.is_empty() {
            validate_custom_key(&req.custom_key)?;
            req.custom_key.clone()
        } else {
            format!("sk-{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
        };

        let mut k = ApiKey {
            id: Uuid::new_v4().simple().to_string(),
            name,
            key,
            status: STATUS_ACTIVE.to_string(),
            created_at: now_ms(),
            last_used_at: None,
            expires_at: None,
            ip_whitelist: trim_ips(&req.ip_whitelist),
            ip_blacklist: trim_ips(&req.ip_blacklist),
            quota: req.quota,
            quota_used: 0.0,
            rate_limit_5h: req.rate_limit_5h,
            rate_limit_1d: req.rate_limit_1d,
            rate_limit_7d: req.rate_limit_7d,
            usage_5h: 0.0,
            usage_1d: 0.0,
            usage_7d: 0.0,
            window_5h_start: None,
            window_1d_start: None,
            window_7d_start: None,
            request_count: 0,
        };
        if req.expires_in_days > 0 {
            k.expires_at = Some(now_ms() + req.expires_in_days * 86_400_000);
        }

        let mut keys = self.keys.lock().unwrap();
        if keys.iter().any(|x| x.key == k.key) {
            return Err("该 key 已存在".to_string());
        }
        keys.push(k.clone());
        drop(keys);
        self.persist()?;
        Ok(k)
    }

    pub fn update(&self, id: &str, req: UpdateRequest) -> Result<ApiKey, String> {
        let mut keys = self.keys.lock().unwrap();
        let Some(k) = keys.iter_mut().find(|x| x.id == id) else {
            return Err("key not found".to_string());
        };
        if let Some(name) = &req.name {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err("名称不能为空".to_string());
            }
            k.name = name;
        }
        if let Some(status) = &req.status {
            if status != STATUS_ACTIVE && status != STATUS_INACTIVE {
                return Err("非法状态".to_string());
            }
            k.status = status.clone();
        }
        if let Some(w) = &req.ip_whitelist {
            if let Err(bad) = validate_ip_patterns(w) {
                return Err(format!("IP 白名单格式非法: {bad}"));
            }
            k.ip_whitelist = trim_ips(w);
        }
        if let Some(b) = &req.ip_blacklist {
            if let Err(bad) = validate_ip_patterns(b) {
                return Err(format!("IP 黑名单格式非法: {bad}"));
            }
            k.ip_blacklist = trim_ips(b);
        }
        if let Some(q) = req.quota {
            k.quota = q;
        }
        if req.reset_quota {
            k.quota_used = 0.0;
        }
        if req.clear_expiry {
            k.expires_at = None;
        } else if let Some(e) = req.expires_at {
            k.expires_at = if e == 0 { None } else { Some(e) };
        }
        if let Some(v) = req.rate_limit_5h {
            k.rate_limit_5h = v;
        }
        if let Some(v) = req.rate_limit_1d {
            k.rate_limit_1d = v;
        }
        if let Some(v) = req.rate_limit_7d {
            k.rate_limit_7d = v;
        }
        if req.reset_rate_limit_usage {
            k.usage_5h = 0.0;
            k.usage_1d = 0.0;
            k.usage_7d = 0.0;
            k.window_5h_start = None;
            k.window_1d_start = None;
            k.window_7d_start = None;
        }
        let out = k.clone();
        drop(keys);
        self.persist()?;
        Ok(out)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        let mut keys = self.keys.lock().unwrap();
        let before = keys.len();
        keys.retain(|x| x.id != id);
        if keys.len() == before {
            return Err("key not found".to_string());
        }
        drop(keys);
        self.persist()
    }

    /// 校验 Bearer token。返回 (结果, 是否放行)。
    pub fn verify(
        &self,
        token: &str,
        master_key: &str,
        client_ip: &str,
        now_ms: i64,
    ) -> (VerifyResult, bool) {
        if let Some(k) = self.match_key(token) {
            let (res, ok) = self.check_managed(&k, client_ip, now_ms);
            return (res, ok);
        }
        if !master_key.is_empty() && digest(token) == digest(master_key) {
            return (
                VerifyResult {
                    key: None,
                    matched: true,
                    reason: String::new(),
                },
                true,
            );
        }
        if master_key.is_empty() && self.count() == 0 {
            return (
                VerifyResult {
                    key: None,
                    matched: true,
                    reason: String::new(),
                },
                true,
            );
        }
        (
            VerifyResult {
                key: None,
                matched: false,
                reason: "invalid api key".to_string(),
            },
            false,
        )
    }

    fn match_key(&self, token: &str) -> Option<ApiKey> {
        let keys = self.keys.lock().unwrap();
        keys.iter()
            .find(|k| digest(&k.key) == digest(token))
            .cloned()
    }

    fn check_managed(&self, k: &ApiKey, client_ip: &str, now_ms: i64) -> (VerifyResult, bool) {
        let res = VerifyResult {
            key: Some(k.clone()),
            matched: false,
            reason: String::new(),
        };
        if !k.active() {
            return (
                VerifyResult {
                    reason: "key is disabled".into(),
                    ..res
                },
                false,
            );
        }
        if k.expired(now_ms) {
            return (
                VerifyResult {
                    reason: "key has expired".into(),
                    ..res
                },
                false,
            );
        }
        if k.quota_exhausted() {
            return (
                VerifyResult {
                    reason: "key quota exhausted".into(),
                    ..res
                },
                false,
            );
        }
        if !ip_allowed(k, client_ip) {
            return (
                VerifyResult {
                    reason: "client IP not allowed".into(),
                    ..res
                },
                false,
            );
        }
        if let Some(rl) = rate_limit_exceeded(k, now_ms) {
            return (VerifyResult { reason: rl, ..res }, false);
        }
        (
            VerifyResult {
                matched: true,
                ..res
            },
            true,
        )
    }

    /// 记账：请求数 +1、用量 +cost、限速窗口滚动、最近使用时间。
    pub fn record_usage(&self, key: &str, cost: f64, now_ms: i64) {
        if key.is_empty() {
            return;
        }
        let mut keys = self.keys.lock().unwrap();
        let Some(k) = keys.iter_mut().find(|x| x.key == key) else {
            return;
        };
        k.request_count += 1;
        k.last_used_at = Some(now_ms);
        if cost > 0.0 {
            k.quota_used += cost;
            roll_window(
                &mut k.usage_5h,
                &mut k.window_5h_start,
                cost,
                5 * 3_600_000,
                now_ms,
            );
            roll_window(
                &mut k.usage_1d,
                &mut k.window_1d_start,
                cost,
                24 * 3_600_000,
                now_ms,
            );
            roll_window(
                &mut k.usage_7d,
                &mut k.window_7d_start,
                cost,
                7 * 24 * 3_600_000,
                now_ms,
            );
        }
        drop(keys);
        let _ = self.persist();
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn digest(s: &str) -> Vec<u8> {
    Sha256::digest(s.as_bytes()).to_vec()
}

fn validate_custom_key(key: &str) -> Result<(), String> {
    if key.len() < 16 {
        return Err("自定义 key 至少 16 个字符".to_string());
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("自定义 key 只能包含字母、数字、下划线、连字符".to_string());
    }
    Ok(())
}

fn validate_ip_patterns(patterns: &[String]) -> Result<(), String> {
    for p in patterns {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        let (net, prefix) = match p.split_once('/') {
            Some((n, p)) => (n, p.parse::<u32>().unwrap_or(u32::MAX)),
            None => (p, if p.contains(':') { 128 } else { 32 }),
        };
        if net.parse::<IpAddr>().is_err() {
            return Err(p.to_string());
        }
        let max = if net.contains(':') { 128 } else { 32 };
        if prefix > max {
            return Err(p.to_string());
        }
    }
    Ok(())
}

fn trim_ips(inp: &[String]) -> Vec<String> {
    inp.iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn ip_allowed(k: &ApiKey, client_ip: &str) -> bool {
    if client_ip.is_empty() {
        return true;
    }
    let Ok(ip) = client_ip.parse::<IpAddr>() else {
        return true;
    };
    if k.ip_blacklist.iter().any(|p| ip_matches(&ip, p)) {
        return false;
    }
    if !k.ip_whitelist.is_empty() && !k.ip_whitelist.iter().any(|p| ip_matches(&ip, p)) {
        return false;
    }
    true
}

fn ip_matches(ip: &IpAddr, pattern: &str) -> bool {
    let (net, prefix) = match pattern.split_once('/') {
        Some((n, p)) => (n, p.parse::<u32>().unwrap_or(0)),
        None => (pattern, if pattern.contains(':') { 128 } else { 32 }),
    };
    let Ok(net_ip) = net.parse::<IpAddr>() else {
        return false;
    };
    match (ip, net_ip) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            if prefix > 32 {
                return false;
            }
            let a = u32::from(*a);
            let b = u32::from(b);
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            (a & mask) == (b & mask)
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => bits_equal(&a.octets(), &b.octets(), prefix),
        _ => false,
    }
}

fn bits_equal(a: &[u8; 16], b: &[u8; 16], prefix: u32) -> bool {
    let full = (prefix / 8) as usize;
    let rem = (prefix % 8) as u8;
    for i in 0..full {
        if a[i] != b[i] {
            return false;
        }
    }
    if rem > 0 {
        let mask = 0xFFu8 << (8 - rem);
        if (a[full] & mask) != (b[full] & mask) {
            return false;
        }
    }
    true
}

fn rate_limit_exceeded(k: &ApiKey, now_ms: i64) -> Option<String> {
    if k.rate_limit_5h > 0.0
        && effective_usage(k.usage_5h, k.window_5h_start, 5 * 3_600_000, now_ms) >= k.rate_limit_5h
    {
        return Some("5h rate limit exceeded".to_string());
    }
    if k.rate_limit_1d > 0.0
        && effective_usage(k.usage_1d, k.window_1d_start, 24 * 3_600_000, now_ms) >= k.rate_limit_1d
    {
        return Some("1d rate limit exceeded".to_string());
    }
    if k.rate_limit_7d > 0.0
        && effective_usage(k.usage_7d, k.window_7d_start, 7 * 24 * 3_600_000, now_ms)
            >= k.rate_limit_7d
    {
        return Some("7d rate limit exceeded".to_string());
    }
    None
}

fn effective_usage(usage: f64, start: Option<i64>, window: i64, now_ms: i64) -> f64 {
    match start {
        Some(s) if now_ms - s < window => usage,
        _ => 0.0,
    }
}

fn roll_window(usage: &mut f64, start: &mut Option<i64>, cost: f64, window: i64, now_ms: i64) {
    if start.map(|s| now_ms - s >= window).unwrap_or(true) {
        *usage = 0.0;
        *start = Some(now_ms);
    }
    *usage += cost;
}
