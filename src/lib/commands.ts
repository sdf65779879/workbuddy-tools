import { invoke } from "@tauri-apps/api/core";
import { checkinDate, hasCheckedInToday, recordCheckinResult, rememberCheckin } from "./checkin";
import type {
  AccountConfig,
  AccountStatusView,
  ApiKey,
  ApiKeyCreateRequest,
  ApiKeyUpdateRequest,
  AppConfig,
  BalanceResult,
  BrowserLoginStart,
  BrowserLoginPoll,
  GatewayStatus,
  LogEvent,
  LogFilter,
  ReqlogSnapshot,
  RefreshResult,
  TaskRunItem,
  TaskJobSnapshot,
  TaskRunResult,
  TaskScanItem,
  TestResult,
  UsageSnapshot,
  UsageDashboard,
  UsageQuery,
  SwitchResult,
  KeepaliveStatus,
} from "../types";

export const getConfig = () => invoke<AppConfig>("get_config");
export const observeImportIp = () => invoke<AccountConfig["import_ip"]>("observe_import_ip");
export const saveConfig = (cfg: AppConfig, expectedAccount?: AccountConfig) => {
  const expectedCredentials = expectedAccount && {
    uid: expectedAccount.uid,
    token: expectedAccount.token,
    refresh_token: expectedAccount.refresh_token,
    expires_at: expectedAccount.expires_at,
  };
  return invoke<AppConfig>("save_config", { cfg, expectedCredentials });
};
export const testConnection = (cfg: AppConfig) => invoke<TestResult>("test_connection", { cfg });
export const startGateway = () => invoke<GatewayStatus>("start_gateway");
export const stopGateway = () => invoke<GatewayStatus>("stop_gateway");
export const gatewayStatus = () => invoke<GatewayStatus>("gateway_status");
export const extractTokenFromLogs = () => invoke<string | null>("extract_token_from_logs");
export const getDataDir = () => invoke<string>("get_data_dir");

export const startBrowserLogin = (realm: "cn" | "global") =>
  invoke<BrowserLoginStart>("start_browser_login", { realm });
export const pollBrowserLogin = (sessionId: string) =>
  invoke<BrowserLoginPoll>("poll_browser_login", { sessionId });
export const cancelBrowserLogin = (sessionId: string) =>
  invoke<void>("cancel_browser_login", { sessionId });

export const listApiKeys = () => invoke<ApiKey[]>("list_api_keys");
export const createApiKey = (req: ApiKeyCreateRequest) =>
  invoke<ApiKey>("create_api_key", { req });
export const updateApiKey = (id: string, req: ApiKeyUpdateRequest) =>
  invoke<ApiKey>("update_api_key", { id, req });
export const deleteApiKey = (id: string) => invoke<void>("delete_api_key", { id });

export const poolStatus = async () => {
  const date = checkinDate();
  const states = await invoke<AccountStatusView[]>("pool_status");
  const sameDay = date === checkinDate();
  return states.map((state) => {
    if (sameDay && state.checkin_done) rememberCheckin(state.uid);
    return { ...state, checkin_done: (sameDay && state.checkin_done) || hasCheckedInToday(state.uid) };
  });
};
export const refreshBalances = () => invoke<BalanceResult[]>("refresh_balances");

export const usageSnapshot = () => invoke<UsageSnapshot>("usage_snapshot");
export const usageDashboard = (query: UsageQuery) =>
  invoke<UsageDashboard>("usage_dashboard", { query });
export const reqlogSnapshot = () => invoke<ReqlogSnapshot>("reqlog_snapshot");
export const reqlogRead = (limit: number, filter: LogFilter) =>
  invoke<LogEvent[]>("reqlog_read", { limit, filter });

export const taskScanAll = () => invoke<TaskScanItem[]>("task_scan_all");
export const startTaskJob = (uid?: string) => invoke<TaskJobSnapshot>("start_task_job", { uid: uid ?? null });
export const taskJobStatus = () => invoke<TaskJobSnapshot | null>("task_job_status");
export const taskRunAll = async () => {
  const date = checkinDate();
  const result = await invoke<TaskRunResult>("task_run_all");
  if (date === checkinDate()) result.items.forEach(recordCheckinResult);
  window.dispatchEvent(new Event("wb-pool-updated"));
  return result;
};
export const taskRunOne = async (uid: string) => {
  const date = checkinDate();
  const result = await invoke<TaskRunItem>("task_run_one", { uid });
  if (date === checkinDate()) recordCheckinResult(result);
  window.dispatchEvent(new Event("wb-pool-updated"));
  return result;
};
export const checkinOne = async (uid: string) => {
  const date = checkinDate();
  const result = await invoke<TaskRunItem>("checkin_one", { uid });
  if (!result.error && date === checkinDate()) recordCheckinResult(result);
  window.dispatchEvent(new Event("wb-pool-updated"));
  return result;
};
export const keepaliveOne = (uid: string) => invoke<BalanceResult>("keepalive_one", { uid });
export const switchAccount = (uid: string) => invoke<SwitchResult>("switch_account", { uid });
export const readCurrentWorkbuddyToken = () =>
  invoke<string | null>("read_current_workbuddy_token");
export const readCurrentWorkbuddyAccount = () =>
  invoke<AccountConfig | null>("read_current_workbuddy_account");
export const refreshWorkbuddyToken = (uid: string) =>
  invoke<RefreshResult>("refresh_workbuddy_token", { uid });
export const keepaliveStatus = () => invoke<KeepaliveStatus[]>("keepalive_status");
