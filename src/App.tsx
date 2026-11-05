import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { useApp } from "./stores/appStore";
import { useTasks } from "./stores/taskStore";
import { poolStatus } from "./lib/commands";
import { useAutomation } from "./lib/useAutomation";
import { reportError } from "./lib/errors";
import type { AccountStatusView } from "./types";
import AppIcon from "./components/AppIcon";
import Sidebar, { type Page } from "./components/Sidebar";
import OverviewPanel from "./components/OverviewPanel";
import VaultPanel from "./components/VaultPanel";
import CheckinPanel from "./components/CheckinPanel";
import GrowthPanel from "./components/GrowthPanel";
import GatewayPanel from "./components/GatewayPanel";
import SharePanel from "./components/SharePanel";
import ErrorDialog from "./components/ErrorDialog";

export default function App() {
  const { t } = useTranslation();
  const { load, loading, error, notice, setNotice, clearError, config, status, refreshStatus,
    balances, balanceRefreshing, balanceSyncedAt, balanceError, refreshAccountBalances, syncKeepaliveStatus } = useApp();
  const [page, setPage] = useState<Page>("overview");
  const [pool, setPool] = useState<AccountStatusView[]>([]);
  const [poolError, setPoolError] = useState<string | null>(null);
  const [theme, setTheme] = useState(() => localStorage.getItem("theme") === "workbuddy-dark" ? "workbuddy-dark" : "workbuddy-light");
  useAutomation(!loading);
  useEffect(() => {
    if (loading) return;
    const sync = () => void useTasks.getState().sync();
    sync();
    const timer = window.setInterval(sync, 1000);
    return () => window.clearInterval(timer);
  }, [loading]);
  const navigate = (next: Page) => { setPage(next); window.scrollTo({ top: 0, left: 0, behavior: "auto" }); };

  useEffect(() => { void load(); }, [load]);
  const balanceScope = config.accounts.map((account) => account.uid).sort().join("|");
  useEffect(() => {
    if (loading) return;
    if (balanceScope) void refreshAccountBalances();
    else useApp.setState({ balances: [], balanceError: null, balanceSyncedAt: null });
  }, [loading, balanceScope, refreshAccountBalances]);
  useEffect(() => {
    localStorage.setItem("theme", theme);
    document.documentElement.setAttribute("data-theme", theme);
    document.documentElement.style.colorScheme = theme === "workbuddy-dark" ? "dark" : "light";
  }, [theme]);
  useEffect(() => {
    if (!notice) return;
    const id = window.setTimeout(() => setNotice(null), 5000);
    return () => window.clearTimeout(id);
  }, [notice, setNotice]);
  useEffect(() => {
    if (loading) return;
    let alive = true;
    let latestRequest = 0;
    const update = async () => {
      const request = ++latestRequest;
      void syncKeepaliveStatus();
      try {
        const next = await poolStatus();
        if (alive && request === latestRequest) {
          setPool(next);
          setPoolError(null);
        }
      } catch (error) {
        if (alive && request === latestRequest) setPoolError(`读取账号状态失败：${String(error)}`);
      }
      if (alive && request === latestRequest) void refreshStatus();
    };
    void update();
    const timer = window.setInterval(() => void update(), 15000);
    window.addEventListener("wb-pool-updated", update);
    return () => { alive = false; window.clearInterval(timer); window.removeEventListener("wb-pool-updated", update); };
  }, [loading, config.accounts, refreshStatus, syncKeepaliveStatus]);

  const balanceObservations = balances.filter((account) => !account.error && config.accounts.some((current) => current.uid === account.uid));
  const knownBalances = balanceObservations.filter((account) => (account.remaining_credits ?? account.credits) !== null && !account.unlimited);
  const balance = knownBalances.reduce((sum, account) => sum + (account.remaining_credits ?? account.credits ?? 0), 0);
  const totalBalance = sumObservedQuota(knownBalances, "total_credits");
  const usedBalance = sumObservedQuota(knownBalances, "used_credits");
  const unlimitedCount = balanceObservations.filter((account) => account.unlimited).length;
  const quotaNumber = (value: number) => value.toLocaleString("zh-CN", { maximumFractionDigits: 2 });
  const participating = config.accounts.filter((account) => account.enabled && !account.paused && account.token.trim());
  const ready = participating.filter((account) => {
    const state = pool.find((item) => item.uid === account.uid);
    const actual = balanceObservations.find((item) => item.uid === account.uid);
    const expires = account.expires_at ? account.expires_at * (account.expires_at < 1e12 ? 1000 : 1) : null;
    const remaining = actual?.remaining_credits ?? actual?.credits ?? -1;
    return state && !state.cooling && actual && (actual.unlimited || (remaining > 0 && remaining >= config.pool.credit_floor)) && (!expires || expires > Date.now());
  });
  const balanceTime = balanceSyncedAt ? new Date(balanceSyncedAt).toLocaleTimeString("zh-CN", { timeZone: "Asia/Shanghai", hour: "2-digit", minute: "2-digit" }) : null;
  const balanceDetails = [
    totalBalance !== null && `总额 ${quotaNumber(totalBalance)}`,
    usedBalance !== null && `已用 ${quotaNumber(usedBalance)}`,
  ].filter(Boolean).join(" · ");
  const balanceStatus = balanceRefreshing ? "正在同步账号实际额度…"
    : balanceError ? (balanceObservations.length ? `已同步 ${balanceObservations.length} 个 · 部分读取失败` : "额度读取失败 · 点击重试")
    : balanceObservations.length ? `${balanceObservations.length} 个账号${unlimitedCount ? ` · ${unlimitedCount} 个不限量` : ` · ${balanceTime} 已同步`}`
    : config.accounts.length ? "实际额度暂未获取" : "导入账号后同步实际额度";
  const noticeText = notice && ["saved", "running", "stopped"].includes(notice) ? t(`common.${notice}`) : notice;
  const refreshBalances = async () => {
    clearError();
    await refreshAccountBalances();
    const failure = useApp.getState().balanceError;
    if (failure) reportError(failure);
  };

  return <div data-theme={theme} className="wb-app">
    <div className="wb-shell">
      <header className="wb-header">
        <div className="wb-header-main">
          <div className="wb-brand" data-tauri-drag-region>
            <div className="wb-brand-mark"><AppIcon name="brand" size={37} /></div>
            <div data-tauri-drag-region>
              <div className="wb-brand-title">WorkBuddy <span>鑫杭加速</span><i className="wb-live-dot" /></div>
              <p className="wb-brand-description">本地账号保险柜 · 一键切换 · 自动签到 · OpenAI 网关</p>
            </div>
          </div>
          <div className="wb-header-tools">
            <button type="button" className="wb-theme-button" onClick={() => setTheme(theme === "workbuddy-light" ? "workbuddy-dark" : "workbuddy-light")}
              aria-label={theme === "workbuddy-light" ? "切换深色外观" : "切换浅色外观"}>
              <span className={`wb-theme-symbol ${theme === "workbuddy-dark" ? "is-dark" : ""}`} aria-hidden="true" />
              {theme === "workbuddy-light" ? "伪神模式" : "日光模式"}
            </button>
            <div className="wb-header-stat wb-balance-summary" title={balanceError || "通过官方接口同步实际剩余额度"} aria-busy={balanceRefreshing}>
              <span>实际积分额度 <button type="button" disabled={balanceRefreshing || !balanceScope} onClick={() => void refreshBalances()} aria-label="刷新账号实际额度"><AppIcon name={balanceRefreshing ? "refresh" : "credits"} size={18} /></button></span>
              <strong>{knownBalances.length ? quotaNumber(balance) : unlimitedCount ? "∞" : "—"}{knownBalances.length > 0 && unlimitedCount > 0 && <small>+ ∞</small>}<small>分</small></strong>
              {balanceDetails && <p>{balanceDetails}</p>}
              <p className={balanceError ? "wb-balance-error" : ""}>{balanceStatus}</p>
            </div>
            <div className="wb-header-stat wb-account-summary">
              <span>账号状态 <i className={`wb-live-dot ${ready.length ? "" : "is-idle"}`} /></span>
              <strong>{ready.length}<small>/ {config.accounts.length}</small></strong>
              <p>{config.accounts.length ? `${participating.length} 个轮转 · ${ready.length} 个就绪` : "等待添加可用账号"}</p>
            </div>
          </div>
        </div>
        <Sidebar page={page} onNavigate={navigate} />
      </header>

      <main className="wb-main" id="main-content">
        {loading ? <div className="wb-loading" role="status"><span className="loading loading-spinner loading-lg text-primary" /><p>正在打开你的工作空间…</p></div>
          : page === "overview" ? <OverviewPanel onNavigate={navigate} />
          : page === "vault" ? <VaultPanel pool={pool} poolError={poolError} onOpenGrowth={() => navigate("growth")} />
          : page === "checkin" ? <CheckinPanel />
          : page === "growth" ? <GrowthPanel />
          : page === "gateway" ? <GatewayPanel /> : <SharePanel />}
      </main>
      <footer className="wb-footer"><span><i className={`wb-live-dot ${status?.running ? "" : "is-idle"}`} />{status?.running ? `网关运行中 · 127.0.0.1:${status.port}` : "网关未启动"}</span><span>WorkBuddy · 让每一份额度都有用</span><span>本地工作空间 <span className="wb-version">v0.1.0</span></span></footer>
    </div>
    {notice && !error && <div className="wb-toast" role="status"><AppIcon name="success" size={24} /><span>{noticeText}</span><button type="button" onClick={() => setNotice(null)} aria-label="关闭通知">×</button></div>}
    {error && <ErrorDialog error={error} onDismiss={clearError} />}
  </div>;
}

function sumObservedQuota(
  balances: { total_credits?: number | null; used_credits?: number | null }[],
  field: "total_credits" | "used_credits",
) {
  if (!balances.length || balances.some((balance) => balance[field] == null)) return null;
  return balances.reduce((sum, balance) => sum + (balance[field] ?? 0), 0);
}

