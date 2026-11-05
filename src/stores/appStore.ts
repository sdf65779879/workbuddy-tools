import { create } from "zustand";
import * as cmd from "../lib/commands";
import { DEFAULT_CONFIG, type AccountConfig, type AppConfig, type BalanceResult, type GatewayStatus, type KeepaliveStatus } from "../types";

interface AppState {
  config: AppConfig;
  status: GatewayStatus | null;
  loading: boolean;
  error: string | null;
  notice: string | null;
  balances: BalanceResult[];
  balanceRefreshing: boolean;
  balanceSyncedAt: number | null;
  balanceError: string | null;
  keepaliveStatuses: KeepaliveStatus[];
  keepaliveError: string | null;

  load: () => Promise<void>;
  save: (cfg: AppConfig, expectedAccount?: AccountConfig) => Promise<void>;
  start: () => Promise<void>;
  stop: () => Promise<void>;
  refreshStatus: () => Promise<void>;
  refreshAccountBalances: () => Promise<void>;
  syncKeepaliveStatus: () => Promise<void>;
  extractToken: () => Promise<string | null>;
  setNotice: (n: string | null) => void;
  clearError: () => void;
}

let balanceRequest: { config: AppConfig; pending: Promise<void> } | null = null;

export const useApp = create<AppState>((set, get) => ({
  config: DEFAULT_CONFIG,
  status: null,
  loading: true,
  error: null,
  notice: null,
  balances: [],
  balanceRefreshing: false,
  balanceSyncedAt: null,
  balanceError: null,
  keepaliveStatuses: [],
  keepaliveError: null,

  load: async () => {
    set({ loading: true, error: null });
    try {
      const [config, status] = await Promise.all([
        cmd.getConfig(),
        cmd.gatewayStatus(),
      ]);
      set({ config, status });
      if (config.auto_start && !status.running) {
        await cmd.startGateway().catch((error) => set({ error: `自动启动网关失败：${String(error)}` }));
        set({ status: await cmd.gatewayStatus().catch(() => null) });
      }
    } catch (e) {
      set({ error: String(e) });
    } finally {
      set({ loading: false });
    }
  },

  save: async (cfg, expectedAccount) => {
    set({ error: null, notice: null });
    try {
      const saved = await cmd.saveConfig(cfg, expectedAccount);
      set({ config: saved, notice: "saved" });
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  start: async () => {
    set({ error: null });
    try {
      const status = await cmd.startGateway();
      set({ status, notice: "running" });
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  stop: async () => {
    set({ error: null });
    try {
      const status = await cmd.stopGateway();
      set({ status, notice: "stopped" });
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  refreshStatus: async () => {
    try {
      const status = await cmd.gatewayStatus();
      set({ status });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  refreshAccountBalances: async () => {
    const { config } = get();
    if (!config.accounts.length) return;
    if (balanceRequest) {
      const existing = balanceRequest;
      await existing.pending;
      if (existing.config.accounts !== config.accounts || existing.config.base_url !== config.base_url) {
        await get().refreshAccountBalances();
      }
      return;
    }
    set({ balanceRefreshing: true, balanceError: null });
    const pending = (async () => {
      try {
        const balances = await cmd.refreshBalances();
        if (!get().config.accounts.length) return;
        const failed = balances.filter((account) => account.error);
        set({ balances, balanceSyncedAt: Date.now(), balanceError: failed.length
          ? failed.map((account) => `${account.nickname}：${account.error}`).join("；") : null });
        window.dispatchEvent(new Event("wb-pool-updated"));
      } catch (error) {
        if (get().config.accounts.length) set({ balanceError: String(error), balances: [] });
      } finally {
        set({ balanceRefreshing: false });
        balanceRequest = null;
      }
    })();
    balanceRequest = { config, pending };
    return pending;
  },

  syncKeepaliveStatus: async () => {
    try {
      const before = get();
      const statuses = await cmd.keepaliveStatus();
      const renewed = statuses.some((status) => status.last_success_at != null &&
        status.last_success_at > (before.keepaliveStatuses.find((previous) => previous.uid === status.uid)?.last_success_at ?? 0));
      const latest = renewed ? await cmd.getConfig() : null;
      set((current) => {
        let changed = false;
        const failures = statuses.filter((status) => {
          if (status.in_progress || status.last_attempt_at == null || !status.error) return false;
          const previous = current.keepaliveStatuses.find((item) => item.uid === status.uid);
          return !previous || previous.last_attempt_at == null || status.last_attempt_at > previous.last_attempt_at ||
            (status.last_attempt_at === previous.last_attempt_at && (previous.in_progress || status.error !== previous.error));
        }).map((status) => `${current.config.accounts.find((account) => account.uid === status.uid)?.nickname || status.uid}：${status.error}`);
        const accounts = current.config.accounts.map((account) => {
          const baseline = before.config.accounts.find((item) => item.uid === account.uid);
          const incoming = latest?.accounts.find((item) => item.uid === account.uid);
          if (!baseline || !incoming || baseline.token !== account.token || baseline.refresh_token !== account.refresh_token) return account;
          if (incoming.token === account.token && incoming.refresh_token === account.refresh_token && incoming.expires_at === account.expires_at) return account;
          changed = true;
          return { ...account, token: incoming.token, refresh_token: incoming.refresh_token, expires_at: incoming.expires_at };
        });
        return { keepaliveStatuses: statuses, keepaliveError: null,
          ...(failures.length ? { error: `后台自动续期失败：\n${failures.join("\n")}`, notice: null } : {}),
          ...(changed ? { config: { ...current.config, accounts } } : {}) };
      });
    } catch (error) {
      set({ keepaliveError: String(error) });
    }
  },

  extractToken: async () => {
    try {
      const token = await cmd.extractTokenFromLogs();
      if (token) {
        set((s) => ({ config: { ...s.config, token }, notice: "extractOk" }));
      }
      return token;
    } catch (e) {
      set({ error: String(e) });
      return null;
    }
  },

  setNotice: (n) => set({ notice: n }),
  clearError: () => set({ error: null }),
}));
