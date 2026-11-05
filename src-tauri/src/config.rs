use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMapping {
    /// 在 Codex 里显示/使用的模型名
    pub codex_name: String,
    /// WorkBuddy 上游真实的模型 id
    pub workbuddy_id: String,
    /// 积分倍率（估算消耗用，0 = 未知）
    #[serde(default)]
    pub multiplier: f64,
}

/// 单个 WorkBuddy 账号（账号池成员）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    /// Stable local key used by the pool, usage and request history.
    pub uid: String,
    /// Official WorkBuddy account UID, separate from older generated local keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workbuddy_uid: Option<String>,
    pub nickname: String,
    /// access token（Bearer token，登录态里的 accessToken）
    pub token: String,
    /// refresh token（续期用，可选）
    #[serde(default)]
    pub refresh_token: String,
    /// 邮箱（可选）
    #[serde(default)]
    pub email: String,
    /// Last login IP explicitly observed in account/login data; never an egress estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_ip: Option<String>,
    /// Public egress of the importing app, never the remote authorization browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_ip: Option<crate::import_ip::ImportIp>,
    /// 企业 ID（可选）
    #[serde(default)]
    pub enterprise_id: String,
    /// 企业名（可选）
    #[serde(default)]
    pub enterprise_name: String,
    /// 登录域（可选）
    #[serde(default)]
    pub domain: String,
    /// token 过期时间（毫秒时间戳，可选）
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// cn / global
    pub realm: String,
    pub enabled: bool,
    pub paused: bool,
}

impl AccountConfig {
    pub fn valid_workbuddy_uid(uid: &str) -> bool {
        !uid.is_empty()
            && uid.len() <= 256
            && !uid.starts_with("acct-")
            && uid != "local-account"
            && uid.bytes().all(|byte| byte.is_ascii_graphic())
    }

    pub fn official_uid(&self) -> Option<&str> {
        let uid = self.workbuddy_uid.as_deref().unwrap_or(&self.uid).trim();
        Self::valid_workbuddy_uid(uid).then_some(uid)
    }
}

/// 账号池调度参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolConfig {
    /// least_recent / weighted_random (legacy) / order / random
    pub strategy: String,
    /// 单账号最大在途请求数（0 = 不限）
    pub max_in_flight: usize,
    /// 连续失败熔断阈值
    pub breaker_threshold: u32,
    /// 429 软冷却基础秒数
    pub soft_cooldown_secs: u64,
    /// 429 软冷却封顶秒数
    pub soft_cooldown_max_secs: u64,
    /// 余额不足硬冷却到「次日该小时」（默认 4 点）
    pub hard_cooldown_hour: u32,
    /// 积分保底：低于该值不接流量（未知积分不拦截）
    pub credit_floor: i64,
    /// 会话粘性
    pub sticky_enabled: bool,
    pub sticky_ttl_secs: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            strategy: "least_recent".to_string(),
            max_in_flight: 3,
            breaker_threshold: 3,
            soft_cooldown_secs: 600,
            soft_cooldown_max_secs: 7200,
            hard_cooldown_hour: 4,
            credit_floor: 100,
            sticky_enabled: true,
            sticky_ttl_secs: 1800,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub archive_enabled: bool,
    pub retention_days: u32,
    pub archive_max_mb: u64,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            archive_enabled: true,
            retention_days: 7,
            archive_max_mb: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleConfig {
    pub keepalive_enabled: bool,
    pub checkin_enabled: bool,
    pub checkin_hours: Vec<u32>,
    pub balance_refresh_minutes: u32,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            keepalive_enabled: true,
            checkin_enabled: false,
            checkin_hours: vec![9, 21],
            balance_refresh_minutes: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub listen_port: u16,
    /// responses / chat
    pub wire_api: String,
    pub auto_start: bool,
    pub set_default_provider: bool,
    pub base_url: String,
    /// 主密钥（兼容旧的单 api_key；空 = 不用）
    pub master_key: String,
    /// 旧版单账号 token（迁移用，保存时清空）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,

    pub accounts: Vec<AccountConfig>,
    pub pool: PoolConfig,
    pub logging: LoggingConfig,
    pub schedule: ScheduleConfig,
    pub models: Vec<ModelMapping>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            listen_port: 15722,
            wire_api: "responses".to_string(),
            auto_start: false,
            set_default_provider: false,
            base_url: "https://www.workbuddy.cn".to_string(),
            master_key: String::new(),
            token: String::new(),
            accounts: Vec::new(),
            pool: PoolConfig::default(),
            logging: LoggingConfig::default(),
            schedule: ScheduleConfig::default(),
            models: default_models(),
        }
    }
}

impl AppConfig {
    /// 把 Codex 请求的模型名解析成 WorkBuddy 上游模型 id。
    pub fn resolve_model(&self, requested: &str) -> String {
        let bare = requested
            .split_once(':')
            .map(|(_, rest)| rest)
            .unwrap_or(requested);
        if let Some(m) = self
            .models
            .iter()
            .find(|m| m.codex_name.eq_ignore_ascii_case(bare))
        {
            return m.workbuddy_id.clone();
        }
        if !bare.trim().is_empty() && !bare.contains('/') {
            return bare.to_string();
        }
        self.models
            .first()
            .map(|m| m.workbuddy_id.clone())
            .unwrap_or_else(|| "fast-model".to_string())
    }

    /// 模型倍率（估算积分用）。
    pub fn multiplier_of(&self, upstream_model: &str) -> f64 {
        self.models
            .iter()
            .find(|m| m.workbuddy_id == upstream_model)
            .map(|m| m.multiplier)
            .unwrap_or(0.0)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.base_url.starts_with("http://") && !self.base_url.starts_with("https://") {
            return Err("Base URL 必须以 http:// 或 https:// 开头".to_string());
        }
        if self.listen_port == 0 {
            return Err("监听端口无效".to_string());
        }
        if self.wire_api != "responses" && self.wire_api != "chat" {
            return Err("wire_api 只支持 responses 或 chat".to_string());
        }
        if self.models.is_empty() {
            return Err("至少需要一条模型映射".to_string());
        }
        for a in &self.accounts {
            if a.token.trim().is_empty() {
                return Err(format!("账号 {} 的 Token 不能为空", a.nickname));
            }
            if a.realm != "cn" && a.realm != "global" {
                return Err(format!("账号 {} 的 realm 只支持 cn 或 global", a.nickname));
            }
            if a.workbuddy_uid.is_some() && a.official_uid().is_none() {
                return Err(format!("账号 {} 的 WorkBuddy UID 无效", a.nickname));
            }
        }
        Ok(())
    }
}

pub fn default_models() -> Vec<ModelMapping> {
    vec![
        m("workbuddy-fast", "fast-model", 0.15),
        m("workbuddy-balanced", "balanced-model", 0.50),
        m("workbuddy-deep", "deep-model", 1.00),
        m("deepseek-v4-pro", "deepseek-v4-pro", 0.51),
        m("deepseek-v4-flash", "deepseek-v4-flash", 0.17),
        m("deepseek-v3-2", "deepseek-v3-2-volc", 0.29),
        m("glm-5.3", "glm-5.3", 0.69),
        m("glm-5.3-flash", "glm-5.3-flash", 0.08),
        m("kimi-k3", "kimi-k3-1", 1.62),
        m("kimi-k2-thinking", "kimi-k2-thinking", 0.54),
        m("minimax-m2.5", "minimax-m2.5", 0.18),
        m("minimax-m2.7", "minimax-m2.7", 0.26),
        m("hy4-preview", "hy4-preview-x", 0.29),
    ]
}

fn m(codex_name: &str, workbuddy_id: &str, multiplier: f64) -> ModelMapping {
    ModelMapping {
        codex_name: codex_name.to_string(),
        workbuddy_id: workbuddy_id.to_string(),
        multiplier,
    }
}

/// 数据目录：~/.workbuddy-codex-gateway
pub fn data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".workbuddy-codex-gateway")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn apikeys_path() -> PathBuf {
    data_dir().join("apikeys.json")
}

pub fn usage_path() -> PathBuf {
    data_dir().join("usage.json")
}

pub fn request_log_dir() -> PathBuf {
    data_dir().join("request-logs")
}

pub fn load_config() -> Result<AppConfig, String> {
    let path = config_path();
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取配置失败 {}: {e}", path.display()))?;
    let mut cfg: AppConfig =
        serde_json::from_str(&text).map_err(|e| format!("解析配置失败: {e}"))?;
    // 旧版单 token 迁移为账号池中的第一个账号
    if cfg.accounts.is_empty() && !cfg.token.trim().is_empty() {
        cfg.accounts.push(AccountConfig {
            uid: "local-account".to_string(),
            workbuddy_uid: None,
            nickname: "默认账号".to_string(),
            token: cfg.token.clone(),
            refresh_token: String::new(),
            email: String::new(),
            login_ip: None,
            import_ip: None,
            enterprise_id: String::new(),
            enterprise_name: String::new(),
            domain: String::new(),
            expires_at: None,
            realm: "cn".to_string(),
            enabled: true,
            paused: false,
        });
    }
    cfg.token.clear();
    Ok(cfg)
}

pub fn save_config(cfg: &AppConfig) -> Result<(), String> {
    save_config_at(&config_path(), cfg)
}

pub(crate) fn save_config_at(path: &std::path::Path, cfg: &AppConfig) -> Result<(), String> {
    cfg.validate()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|e| format!("序列化配置失败: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入配置失败 {}: {e}", path.display()))
}
