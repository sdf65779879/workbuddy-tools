import { useState } from "react";
import { checkinOne, getConfig, refreshWorkbuddyToken } from "../lib/commands";
import { useApp } from "../stores/appStore";
import { checkinMessage } from "../lib/checkin";
import { clearExecutionError, reportError } from "../lib/errors";
import type { TaskRunItem } from "../types";
import AppIcon from "./AppIcon";
import "./panels.css";

export default function CheckinPanel() {
  const { config, save, setNotice, balances, balanceError, balanceRefreshing, refreshAccountBalances,
    keepaliveStatuses, keepaliveError, syncKeepaliveStatus } = useApp();
  const [busy, setBusy] = useState<string | null>(null);
  const [results, setResults] = useState<{ name: string; message: string; ok: boolean }[]>([]);
  const [savingKeepalive, setSavingKeepalive] = useState(false);
  const accounts = config.accounts.filter((a) => a.enabled && !a.paused);
  const known = balances.filter((balance) => !balance.error && config.accounts.some((a) => a.uid === balance.uid));
  const unlimitedCount = known.filter((balance) => balance.unlimited).length;
  const finite = known.filter((balance) => !balance.unlimited && (balance.remaining_credits ?? balance.credits) !== null);
  const credits = finite.length ? finite.reduce((sum, balance) => sum + (balance.remaining_credits ?? balance.credits ?? 0), 0) : null;
  const execute = async (action: "checkin" | "refresh" | "keepalive") => {
    setBusy(action); clearExecutionError(); setResults([]);
    const rows: { name: string; message: string; ok: boolean }[] = [];
    try {
      if (action === "refresh") {
        await refreshAccountBalances();
        const { balances: refreshed, balanceError: refreshError } = useApp.getState();
        if (refreshError && !refreshed.length) throw new Error(refreshError);
        for (const r of refreshed) {
          const remaining = r.remaining_credits ?? r.credits;
          rows.push({ name: r.nickname, message: r.error || (r.unlimited ? "积分不限量" : `剩余 ${remaining !== null ? remaining.toLocaleString("zh-CN", { maximumFractionDigits: 8 }) : "未知"} 积分`), ok: !r.error });
        }
      } else {
        for (const a of accounts.filter((a) => action !== "checkin" || a.realm === "cn")) {
          try {
            if (action === "checkin") {
              const r: TaskRunItem = await checkinOne(a.uid);
              rows.push({ name: a.nickname, message: r.error || checkinMessage(r.checkin), ok: !r.error });
            } else {
              if (!a.refresh_token) { rows.push({ name: a.nickname, message: "未配置 Refresh Token，已跳过", ok: false }); continue; }
              const r = await refreshWorkbuddyToken(a.uid);
              rows.push({ name: a.nickname, message: r.message, ok: r.ok });
            }
          } catch (e) { rows.push({ name: a.nickname, message: String(e), ok: false }); }
          setResults([...rows]);
        }
        if (action === "keepalive") {
          useApp.setState({ config: await getConfig() });
          await syncKeepaliveStatus();
        }
      }
      setResults(rows);
      if (action !== "refresh") await refreshAccountBalances();
      window.dispatchEvent(new Event("wb-pool-updated"));
      const failed = rows.filter((row) => !row.ok);
      const failures = failed.map((row) => `${row.name}：${row.message}`);
      const refreshFailure = useApp.getState().balanceError;
      if (action !== "refresh" && refreshFailure) failures.push(`刷新积分失败：${refreshFailure}`);
      if (failures.length) reportError(failures.join("\n"));
      else setNotice(`操作完成 · 成功 ${rows.length}/${rows.length}`);
    } catch (e) { reportError(e); } finally { setBusy(null); }
  };
  const changeSchedule = async (enabled: boolean) => {
    clearExecutionError();
    try { const current = useApp.getState().config; await save({ ...current, schedule: { ...current.schedule, checkin_enabled: enabled } }); window.dispatchEvent(new Event("wb-automation-change")); }
    catch (e) { reportError(e); }
  };
  const changeKeepalive = async (enabled: boolean) => {
    setSavingKeepalive(true);
    clearExecutionError();
    try {
      const current = useApp.getState().config;
      await save({ ...current, schedule: { ...current.schedule, keepalive_enabled: enabled } });
      await syncKeepaliveStatus();
    } catch (error) {
      reportError(error);
    } finally {
      setSavingKeepalive(false);
    }
  };
  const renewalTime = (value: number | null) => value == null ? "—" : new Date(value).toLocaleString("zh-CN", { timeZone: "Asia/Shanghai", hour12: false });
  const done = results.filter((r) => r.ok).length;
  return <div className="wb-panel">
    <div className="wb-page-heading"><div><h1 className="wb-page-title">日常运维，轻松搞定</h1><p className="wb-page-description">签到、积分与登录续期，批量照顾好每一个账号。</p></div><span className="wb-soft-badge"><span className="wb-status-dot" />{accounts.length} 个可用账号</span></div>
    {balanceError && <p className="wb-muted text-xs mb-4" role="status" title={balanceError}>部分账号积分暂未同步，可点击刷新积分重试。</p>}
    <div className="wb-action-grid">
      <section className="acct-card wb-action-card wb-action-featured"><div className="wb-icon-tile wb-icon-mint"><AppIcon name="checkin" size={34} /></div><h2>一键全号签到</h2><p>为所有启用的国内账号执行签到，逐号返回签到结果。</p><button className="btn btn-primary btn-sm" disabled={!!busy || !accounts.some((a) => a.realm === "cn")} onClick={() => void execute("checkin")}>{busy === "checkin" ? "正在签到…" : "执行签到"}</button></section>
      <section className="acct-card wb-action-card"><div className="wb-icon-tile wb-icon-blue"><AppIcon name="credits" size={34} /></div><h2>刷新全部积分</h2><p>拉取每个账号的实时剩余积分，在保险柜中查看余额。</p><button className="btn btn-outline btn-sm" disabled={!!busy || balanceRefreshing || !config.accounts.length} onClick={() => void execute("refresh")}>{busy === "refresh" ? "正在刷新…" : "刷新积分"}</button></section>
      <section className="acct-card wb-action-card"><div className="wb-icon-tile wb-icon-amber"><AppIcon name="tokens" size={34} /></div><h2>保活全部账号</h2><p>使用 Refresh Token 续期网关凭证，缺少续期凭证的账号会跳过。</p><button className="btn btn-outline btn-sm" disabled={!!busy || !accounts.length} onClick={() => void execute("keepalive")}>{busy === "keepalive" ? "正在续期…" : "执行保活"}</button></section>
    </div>
    <section className="acct-card wb-automation-card">
      <div className="wb-section-heading"><div className="flex items-center gap-3"><div className="wb-icon-tile wb-icon-mint wb-icon-small"><AppIcon name="settings" size={24} /></div><h2>自动化</h2><span className="wb-muted text-xs">工具运行期间生效</span></div><span className="wb-soft-badge">{config.accounts.length} 个托管账号{credits !== null ? ` · 积分 ${credits.toLocaleString("zh-CN", { maximumFractionDigits: 8 })}` : ""}{unlimitedCount > 0 ? ` · ${unlimitedCount} 个不限量` : ""}</span></div>
      <div className="wb-automation-options"><label><input className="checkbox checkbox-primary checkbox-sm" type="checkbox" checked={config.schedule.keepalive_enabled !== false} disabled={savingKeepalive} onChange={(event) => void changeKeepalive(event.target.checked)} /><span>后台自动续期 <small>每 30 秒检查，临近过期提前续期，失败后自动重试</small></span></label><label><input className="checkbox checkbox-primary checkbox-sm" type="checkbox" checked={config.schedule.checkin_enabled} onChange={(e) => void changeSchedule(e.target.checked)} /><span>每天自动签到 <small>{config.schedule.checkin_hours.length ? config.schedule.checkin_hours.map((h) => `${String(h).padStart(2, "0")}:00`).join(" / ") : "每日一次"}</small></span></label></div>
      {keepaliveError && <p className="text-error text-xs" role="status">续期状态读取失败：{keepaliveError}</p>}
      <div className={`wb-operation-status ${results.length && done < results.length ? "wb-operation-warning" : ""}`} role="status"><AppIcon name={busy ? "refresh" : "success"} size={21} /><span>{busy ? `正在处理，请稍候… 已返回 ${results.length} 个账号结果` : results.length ? `操作完成 · ${done} 个成功，${results.length - done} 个失败或跳过` : accounts.length ? "准备就绪，选择上方操作开始。" : "导入账号后，即可执行批量操作和自动化。"}</span></div>
    </section>
    {!!keepaliveStatuses.length && <section className="acct-card wb-form-card">
      <div className="wb-section-heading"><h2>账号自动续期</h2><span className="wb-muted text-xs">应用运行期间在后台执行 · 时间为北京时间</span></div>
      <div className="overflow-x-auto mt-4"><table className="table table-sm"><thead><tr><th>账号</th><th>状态</th><th>上次成功</th><th>下次检查 / 重试</th></tr></thead><tbody>
        {keepaliveStatuses.map((status) => {
          const account = config.accounts.find((account) => account.uid === status.uid);
          const disabled = config.schedule.keepalive_enabled === false || !account?.enabled || account.paused;
          const message = status.in_progress ? "正在续期…" : disabled ? "已暂停" : !account?.refresh_token ? "需要导入 Refresh Token" : status.error || "自动续期已就绪";
          return <tr key={status.uid}><td>{account?.nickname || status.uid}</td><td className={status.error && !disabled ? "text-warning" : ""}>{message}</td><td>{renewalTime(status.last_success_at)}</td><td>{disabled || !account?.refresh_token ? "—" : renewalTime(status.next_attempt_at)}</td></tr>;
        })}
      </tbody></table></div>
    </section>}
    {!!results.length && <section className="acct-card wb-form-card"><div className="wb-section-heading"><h2>本次执行结果</h2><span className="wb-muted text-xs">{results.length} 个账号</span></div><div className="wb-result-list">{results.map((r, i) => <div key={`${r.name}-${i}`} className="wb-result-row"><span className={`wb-result-dot ${r.ok ? "" : "is-warning"}`} /><strong>{r.name}</strong><span className={r.ok ? "wb-muted" : "text-warning"}>{r.message}</span></div>)}</div></section>}
  </div>;
}

