export interface ModelMapping {
  codex_name: string;
  workbuddy_id: string;
  multiplier: number;
}

export type WireApi = "responses" | "chat";

export interface AccountConfig {
  uid: string;
  workbuddy_uid?: string | null;
  login_ip?: string | null;
  import_ip?: { ip: string; observed_at: number } | null;
  nickname: string;
  token: string;
  refresh_token: string;
  email: string;
  enterprise_id: string;
  enterprise_name: string;
  domain: string;
  expires_at: number | null;
  realm: "cn" | "global";
  enabled: boolean;
  paused: boolean;
}

export interface BrowserLoginStart {
  session_id: string;
  url: string;
  realm: "cn" | "global";
  expires_at: number;
  poll_interval_ms: number;
}

export interface BrowserLoginPoll {
  status: "pending" | "completed" | "expired";
  uid?: string | null;
  nickname?: string | null;
  message?: string | null;
}

export interface PoolConfig {
  strategy: "least_recent" | "weighted_random" | "order" | "random";
  max_in_flight: number;
  breaker_threshold: number;
  soft_cooldown_secs: number;
  soft_cooldown_max_secs: number;
  hard_cooldown_hour: number;
  credit_floor: number;
  sticky_enabled: boolean;
  sticky_ttl_secs: number;
}

export interface LoggingConfig {
  archive_enabled: boolean;
  retention_days: number;
  archive_max_mb: number;
}

export interface ScheduleConfig {
  keepalive_enabled: boolean;
  checkin_enabled: boolean;
  checkin_hours: number[];
  balance_refresh_minutes: number;
}

export interface KeepaliveStatus {
  uid: string;
  in_progress: boolean;
  last_attempt_at: number | null;
  last_success_at: number | null;
  next_attempt_at: number | null;
  consecutive_failures: number;
  error: string | null;
}

export interface AppConfig {
  listen_port: number;
  wire_api: WireApi;
  auto_start: boolean;
  set_default_provider: boolean;
  base_url: string;
  master_key: string;
  token: string;
  accounts: AccountConfig[];
  pool: PoolConfig;
  logging: LoggingConfig;
  schedule: ScheduleConfig;
  models: ModelMapping[];
}

export interface GatewayStatus {
  running: boolean;
  port: number;
  base_url: string;
  models: string[];
}

export interface TestResult {
  ok: boolean;
  status: number;
  latency_ms: number;
  content: string;
  error: string | null;
}

// ---- API Keys ----
export interface ApiKey {
  id: string;
  name: string;
  key: string;
  status: "active" | "inactive";
  created_at: number;
  last_used_at: number | null;
  expires_at: number | null;
  ip_whitelist: string[];
  ip_blacklist: string[];
  quota: number;
  quota_used: number;
  rate_limit_5h: number;
  rate_limit_1d: number;
  rate_limit_7d: number;
  usage_5h: number;
  usage_1d: number;
  usage_7d: number;
  window_5h_start: number | null;
  window_1d_start: number | null;
  window_7d_start: number | null;
  request_count: number;
}

export interface ApiKeyCreateRequest {
  name: string;
  custom_key: string;
  ip_whitelist: string[];
  ip_blacklist: string[];
  quota: number;
  expires_in_days: number;
  rate_limit_5h: number;
  rate_limit_1d: number;
  rate_limit_7d: number;
}

export interface ApiKeyUpdateRequest {
  name?: string;
  status?: string;
  ip_whitelist?: string[];
  ip_blacklist?: string[];
  quota?: number;
  reset_quota?: boolean;
  expires_at?: number;
  clear_expiry?: boolean;
  rate_limit_5h?: number;
  rate_limit_1d?: number;
  rate_limit_7d?: number;
  reset_rate_limit_usage?: boolean;
}

// ---- 账号池 ----
export interface AccountStatusView {
  uid: string;
  nickname: string;
  realm: string;
  enabled: boolean;
  paused: boolean;
  token_len: number;
  credits: number | null;
  remaining_credits?: number | null;
  total_credits?: number | null;
  used_credits?: number | null;
  unlimited?: boolean;
  cooling: boolean;
  cool_kind: string;
  cool_remaining_secs: number;
  reason: string;
  soft_streak: number;
  consecutive_fails: number;
  success_count: number;
  err_total: number;
  in_flight: number;
  checkin_done: boolean;
}

export interface BalanceResult {
  uid: string;
  nickname: string;
  credits: number | null;
  error: string | null;
  remaining_credits?: number | null;
  total_credits?: number | null;
  used_credits?: number | null;
  unlimited?: boolean;
  fetched_at?: number | null;
}

export interface SwitchResult {
  ok: boolean;
  uid: string;
  nickname: string;
  auth_file: string;
  message: string;
}

export interface RefreshResult {
  ok: boolean;
  uid: string;
  nickname: string;
  message: string;
}

// ---- 用量 ----
export interface ModelStat {
  model: string;
  requests: number;
  total_tokens: number;
  credit: number;
}

export interface AccountStat {
  uid: string;
  requests: number;
  total_tokens: number;
  credit: number;
}

export interface UsageSnapshot {
  saved_at: number;
  total_requests: number;
  total_errors: number;
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  avg_latency_ms: number;
  total_credit: number;
  by_model: ModelStat[];
  by_account: AccountStat[];
}

export interface UsageQuery {
  from_ms?: number;
  to_ms?: number;
  uid?: string;
  model?: string;
  granularity?: "hour" | "day";
  timezone_offset_minutes?: number;
}

export interface UsagePoint {
  time_ms: number;
  requests: number;
  errors: number;
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  credit: number;
}

export interface UsageBreakdown {
  requests: number;
  errors: number;
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  avg_latency_ms: number;
  credit: number;
  cache_hit_tokens: number;
  cache_miss_tokens: number;
}

export interface UsageDashboard extends Omit<UsageSnapshot, "by_model" | "by_account"> {
  window_from_ms: number | null;
  window_to_ms: number | null;
  granularity: "hour" | "day";
  success_rate: number;
  avg_ttfb_ms: number | null;
  avg_tokens_per_second: number | null;
  cache_hit_tokens: number;
  cache_miss_tokens: number;
  cache_hit_rate: number | null;
  live_rpm: number;
  live_tpm: number;
  by_model: (UsageBreakdown & { model: string })[];
  by_account: (UsageBreakdown & { uid: string; realm: string })[];
  series: UsagePoint[];
}

// ---- 运行日志 ----
export interface LogEvent {
  time: number;
  request_id: string;
  path: string;
  account: string;
  account_uid?: string;
  model: string;
  status: number;
  ok: boolean;
  outcome: string;
  duration_ms: number;
  ttfb_ms: number | null;
  attempts: number | null;
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  credit: number;
  credit_known: boolean;
  client_ip: string;
  user_agent: string;
}

export interface ArchiveStats {
  enabled: boolean;
  dir: string;
  files: number;
  bytes: number;
  dropped_writes: number;
  last_error: string;
}

export interface ReqlogSnapshot {
  started_at_ms: number;
  completed: number;
  in_flight: number;
  succeeded: number;
  failed: number;
  success_rate: number;
  http_success_rate: number;
  avg_duration_ms: number;
  recent: LogEvent[];
  archive: ArchiveStats;
}

export interface LogFilter {
  outcome: string;
  account: string;
  account_uid?: string;
  model: string;
  model_exact?: string;
  client_ip: string;
  from_ms: number;
  to_ms: number;
}

// ---- 任务中心 ----
export interface TaskView {
  task_code: string;
  title: string;
  description: string;
  task_desc: string;
  credit: number;
  energy: number;
  has_reward: boolean;
  task_type: string;
  locked: boolean;
  target: number;
  current: number;
  accept_status: string;
  status: string;
  claimed: boolean;
  claimable: boolean;
}

export interface TaskRunItem {
  uid: string;
  nickname: string;
  checkin: string;
  accepted: number;
  claimed: number;
  credit: number;
  energy: number;
  error: string | null;
}

export interface TaskRunResult {
  items: TaskRunItem[];
  total_credit: number;
  total_energy: number;
}

export interface TaskScanItem {
  uid: string;
  nickname: string;
  tasks: TaskView[];
  error: string | null;
}

export const DEFAULT_POOL: PoolConfig = {
  strategy: "least_recent",
  max_in_flight: 3,
  breaker_threshold: 3,
  soft_cooldown_secs: 600,
  soft_cooldown_max_secs: 7200,
  hard_cooldown_hour: 4,
  credit_floor: 100,
  sticky_enabled: true,
  sticky_ttl_secs: 1800,
};

export const DEFAULT_LOGGING: LoggingConfig = {
  archive_enabled: true,
  retention_days: 7,
  archive_max_mb: 100,
};

export const DEFAULT_SCHEDULE: ScheduleConfig = {
  keepalive_enabled: true,
  checkin_enabled: false,
  checkin_hours: [9, 21],
  balance_refresh_minutes: 5,
};

export const DEFAULT_CONFIG: AppConfig = {
  listen_port: 15722,
  wire_api: "responses",
  auto_start: false,
  set_default_provider: false,
  base_url: "https://www.workbuddy.cn",
  master_key: "",
  token: "",
  accounts: [],
  pool: DEFAULT_POOL,
  logging: DEFAULT_LOGGING,
  schedule: DEFAULT_SCHEDULE,
  models: [
    { codex_name: "workbuddy-fast", workbuddy_id: "fast-model", multiplier: 0.15 },
    { codex_name: "workbuddy-balanced", workbuddy_id: "balanced-model", multiplier: 0.5 },
    { codex_name: "workbuddy-deep", workbuddy_id: "deep-model", multiplier: 1.0 },
    { codex_name: "deepseek-v4-pro", workbuddy_id: "deepseek-v4-pro", multiplier: 0.51 },
    { codex_name: "deepseek-v4-flash", workbuddy_id: "deepseek-v4-flash", multiplier: 0.17 },
    { codex_name: "glm-5.3", workbuddy_id: "glm-5.3", multiplier: 0.69 },
    { codex_name: "glm-5.3-flash", workbuddy_id: "glm-5.3-flash", multiplier: 0.08 },
    { codex_name: "kimi-k3", workbuddy_id: "kimi-k3-1", multiplier: 1.62 },
    { codex_name: "kimi-k2-thinking", workbuddy_id: "kimi-k2-thinking", multiplier: 0.54 },
    { codex_name: "minimax-m2.5", workbuddy_id: "minimax-m2.5", multiplier: 0.18 },
    { codex_name: "minimax-m2.7", workbuddy_id: "minimax-m2.7", multiplier: 0.26 },
    { codex_name: "hy4-preview", workbuddy_id: "hy4-preview-x", multiplier: 0.29 },
  ],
};
