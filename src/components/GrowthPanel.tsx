import { useEffect, useState } from "react";
import { taskRunAll, taskScanAll } from "../lib/commands";
import { useApp } from "../stores/appStore";
import { clearExecutionError, reportError } from "../lib/errors";
import type { TaskRunResult, TaskScanItem } from "../types";
import AppIcon from "./AppIcon";
import "./panels.css";

export default function GrowthPanel() {
  const { config } = useApp();
  const [scan, setScan] = useState<TaskScanItem[]>([]);
  const [result, setResult] = useState<TaskRunResult | null>(null);
  const [busy, setBusy] = useState<"scan" | "run" | null>(null);
  const [onlyClaimable, setOnlyClaimable] = useState(false);
  const [lastScan, setLastScan] = useState<Date | null>(null);
  const load = async () => {
    setBusy("scan"); clearExecutionError();
    try {
      const items = await taskScanAll();
      setScan(items); setLastScan(new Date());
      const failures = items.filter((item) => item.error);
      if (failures.length) reportError(failures.map((item) => `${item.nickname}：${item.error}`).join("\n"));
    } catch (e) { reportError(e); } finally { setBusy(null); }
  };
  useEffect(() => { void load(); }, []);
  const runAll = async () => {
    setBusy("run"); clearExecutionError();
    try {
      const next = await taskRunAll();
      setResult(next);
      window.dispatchEvent(new Event("wb-pool-updated"));
      const failures = next.items.filter((item) => item.error).map((item) => `${item.nickname}：${item.error}`);
      try {
        const items = await taskScanAll();
        setScan(items); setLastScan(new Date());
        failures.push(...items.filter((item) => item.error).map((item) => `${item.nickname}：${item.error}`));
      } catch (e) { failures.push(`刷新任务失败：${String(e)}`); }
      if (failures.length) reportError([...new Set(failures)].join("\n"));
    } catch (e) { reportError(e); } finally { setBusy(null); }
  };
  const all = scan.flatMap((a) => a.tasks);
  const claimable = all.filter((task) => task.claimable).length;
  const claimed = all.filter((task) => task.claimed).length;
  const progress = all.length ? Math.round(claimed / all.length * 100) : 0;
  return <div className="wb-panel">
    <div className="wb-page-heading"><div><h1 className="wb-page-title">让每一份成长，都有收获</h1><p className="wb-page-description">管理成长任务与可领取奖励，完成后集中查看每个账号的结果。</p></div><button className="btn btn-outline btn-sm" disabled={!!busy} onClick={() => void load()}><AppIcon name="refresh" size={18} />刷新任务</button></div>
    <div className="wb-action-grid">
      <section className="acct-card wb-action-card wb-action-featured"><div className="wb-icon-tile wb-icon-mint"><AppIcon name="growth" size={34} /></div><h2>一键全做</h2><p>批量签到、接受未接任务、领取已达标奖励，并尝试领取礼包与补偿。</p><button className="btn btn-primary btn-sm" disabled={!!busy || !config.accounts.some((a) => a.enabled && !a.paused)} onClick={() => void runAll()}>{busy === "run" ? "正在执行…" : "执行并自动领奖"}</button></section>
      <section className="acct-card wb-action-card"><div className="wb-icon-tile wb-icon-blue"><AppIcon name="requests" size={34} /></div><h2>查看成长任务</h2><p>同步成长中心的真实任务进度，查看哪些已完成、哪些还需要继续。</p><button className="btn btn-outline btn-sm" disabled={!!busy} onClick={() => { setOnlyClaimable(false); void load(); }}>{busy === "scan" ? "正在同步…" : "同步任务进度"}</button></section>
      <section className="acct-card wb-action-card"><div className="wb-icon-tile wb-icon-amber"><AppIcon name="gift" size={34} /></div><h2>发现待领奖励</h2><p>筛选已经达标、尚未领取的任务奖励，准备好后使用「一键全做」领取。</p><button className="btn btn-outline btn-sm" disabled={!!busy} onClick={() => { setOnlyClaimable(true); void load(); }}>查看待领 {claimable ? `（${claimable}）` : ""}</button></section>
    </div>
    <section className="acct-card wb-growth-job">
      <div className="wb-section-heading"><div className="flex flex-wrap items-center gap-3"><div className="wb-icon-tile wb-icon-mint wb-icon-small"><AppIcon name="growth" size={24} /></div><h2>任务作业</h2><span className="wb-muted text-xs">账号进度与执行结果</span></div><span className="wb-soft-badge">{scan.length} 个账号已查 · {claimable} 项可领</span></div>
      <p className="wb-muted text-sm mt-3">行为类任务的进度由实际使用产生；已满足条件的奖励可在执行作业时领取。</p>
      <div className="wb-growth-progress"><div className="flex justify-between text-xs"><span>已领取进度 <strong className="text-primary">{progress}%</strong></span><span className="wb-muted">{claimed} / {all.length} 项{lastScan ? ` · 更新于 ${lastScan.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" })}` : ""}</span></div><progress className="progress progress-primary mt-3 h-2" value={claimed} max={Math.max(all.length, 1)} /></div>
      {result && <div role="status" className="wb-operation-status"><AppIcon name="success" size={22} /><span>本次已接受 {result.items.reduce((n, i) => n + i.accepted, 0)} 项，领取 {result.items.reduce((n, i) => n + i.claimed, 0)} 项 · 获得 {result.total_credit} 积分 / {result.total_energy} 能量</span></div>}
      <div className="wb-toolbar mt-4"><span className="text-sm font-semibold">任务清单</span><div className="wb-segment-control"><button className={!onlyClaimable ? "active" : ""} onClick={() => setOnlyClaimable(false)}>全部任务</button><button className={onlyClaimable ? "active" : ""} onClick={() => setOnlyClaimable(true)}>待领奖励 <span>{claimable}</span></button></div></div>
      {!scan.length && <div className="wb-empty wb-empty-small"><AppIcon name="growth" size={48} /><h3>{busy ? "正在同步任务" : "还没有任务记录"}</h3><p>{busy ? "正在读取每个账号的成长中心。" : "导入并启用国内账号后，点击「同步任务进度」。"}</p></div>}
      <div className="wb-task-accounts">{scan.map((a) => {
        const shown = a.tasks.filter((task) => !onlyClaimable || task.claimable);
        const runResult = result?.items.find((r) => r.uid === a.uid);
        return <div key={a.uid} className="wb-task-account"><div className="flex flex-wrap items-center gap-2"><span className="wb-mini-avatar">{a.nickname.slice(0, 1)}</span><strong className="text-sm">{a.nickname}</strong><span className="wb-muted text-xs">{shown.length} 项任务</span>{runResult && <span className="ml-auto text-xs text-primary">+{runResult.credit} 积分</span>}</div>
          {(a.error || runResult?.error) && <p className="text-error text-xs mt-3">{a.error || runResult?.error}</p>}
          {!shown.length && !a.error && <p className="wb-muted text-xs mt-3">{onlyClaimable ? "暂无待领取的奖励" : "暂无任务"}</p>}
          <div className="wb-task-grid">{shown.map((task) => <div key={task.task_code} className="wb-task-item"><div className="flex justify-between gap-3"><strong className="text-xs">{task.title || task.task_code}</strong><span className={`wb-task-state ${task.claimable ? "is-claimable" : ""}`}>{task.claimed ? "已领取" : task.claimable ? "可领取" : task.locked ? "未解锁" : "进行中"}</span></div><p className="wb-muted text-xs mt-2 line-clamp-2">{task.description || task.task_desc || "完成任务，获取成长奖励。"}</p><div className="flex justify-between mt-3 text-xs"><span className="text-primary">+{task.credit} 积分</span><span className="wb-muted">{task.current} / {task.target}</span></div><progress className="progress progress-primary mt-2 h-1" value={task.current} max={Math.max(task.target, 1)} /></div>)}</div>
        </div>;
      })}</div>
    </section>
  </div>;
}

