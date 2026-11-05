import { useCallback, useEffect, useRef, useState } from "react";
import { taskScanAll } from "../lib/commands";
import { useApp } from "../stores/appStore";
import { useTasks } from "../stores/taskStore";
import { clearExecutionError, reportError } from "../lib/errors";
import type { TaskJobSnapshot, TaskScanItem, TaskView } from "../types";
import AppIcon from "./AppIcon";
import "./panels.css";

const ignoredTasks = ["无畏契约拼豆专家", "中国电商运营专家", "商家宣传页与海报设计师", "直播电商教练", "关注腾讯 WorkBuddy 官方公众号", "抖音策略师"];
const statusLabel: Record<TaskJobSnapshot["status"], string> = { running: "执行中", completed: "作业已完成", partial: "作业结束 · 部分失败", failed: "执行失败" };
const ignored = (task: TaskView) => !!task.skipped || !!task.skip_reason;
const time = (value: number) => new Date(value).toLocaleTimeString("zh-CN", { hour12: false });

export default function GrowthPanel() {
  const { config } = useApp();
  const { job, starting, synchronized, syncError, start } = useTasks();
  const [scan, setScan] = useState<TaskScanItem[]>([]);
  const [scanning, setScanning] = useState(false);
  const [scanError, setScanError] = useState<string | null>(null);
  const [onlyClaimable, setOnlyClaimable] = useState(false);
  const [lastScan, setLastScan] = useState<Date | null>(null);
  const [followLogs, setFollowLogs] = useState(true);
  const logs = useRef<HTMLDivElement>(null);
  const mounted = useRef(false);
  const scanPending = useRef(false);
  const scanQueued = useRef(false);
  const running = job?.status === "running";
  const taskBusy = starting || running || !synchronized;
  const result = job?.result;
  const load = useCallback(async () => {
    if (scanPending.current) { scanQueued.current = true; return; }
    scanPending.current = true;
    setScanning(true); setScanError(null);
    try {
      const items = await taskScanAll();
      if (mounted.current) { setScan(items); setLastScan(new Date()); }
    } catch (error) {
      if (mounted.current) setScanError(String(error));
    } finally {
      scanPending.current = false;
      if (mounted.current) {
        if (scanQueued.current) {
          scanQueued.current = false;
          void load();
        } else { setScanning(false); }
      }
    }
  }, []);
  useEffect(() => {
    mounted.current = true;
    void load();
    return () => { mounted.current = false; };
  }, [load]);
  const finished = job?.finished_at;
  useEffect(() => { if (finished) void load(); }, [finished, load]);
  const lastLog = job?.logs[job.logs.length - 1]?.seq;
  useEffect(() => {
    if (followLogs && logs.current) logs.current.scrollTop = logs.current.scrollHeight;
  }, [lastLog, followLogs]);
  const runAll = async () => {
    clearExecutionError();
    try { await start(); } catch (error) { reportError(error); }
  };
  const all = scan.flatMap((account) => account.tasks);
  const eligible = all.filter((task) => !ignored(task));
  const claimable = eligible.filter((task) => task.claimable).length;
  const claimed = eligible.filter((task) => task.claimed).length;
  const progress = eligible.length ? Math.round(claimed / eligible.length * 100) : 0;
  const waiting = result?.items.reduce((count, item) => count + (item.waiting ?? 0), 0) ?? 0;
  const skipped = result?.items.reduce((count, item) => count + (item.skipped ?? 0), 0) ?? 0;
  return <div className="wb-panel">
    <div className="wb-page-heading"><div><h1 className="wb-page-title">让每一份成长，都有收获</h1><p className="wb-page-description">执行账号成长任务，集中查看真实进度、领取结果和操作日志。</p></div><button className="btn btn-outline btn-sm" disabled={scanning} onClick={() => void load()}><AppIcon name="refresh" size={18} />刷新任务</button></div>
    <div className="wb-action-grid">
      <section className="acct-card wb-action-card wb-action-featured"><div className="wb-icon-tile wb-icon-mint"><AppIcon name="growth" size={34} /></div><h2>一键全做</h2><p>为启用的国内账号签到、执行支持的任务并领取已达标奖励。对话会使用账号额度，指定 6 项任务自动忽略。</p><button className="btn btn-primary btn-sm" disabled={taskBusy || !config.accounts.some((account) => account.realm === "cn" && account.enabled && !account.paused && account.token.trim())} onClick={() => void runAll()}>{starting ? "正在启动…" : running ? "任务正在执行" : !synchronized ? "正在确认任务状态…" : "执行并自动领奖"}</button></section>
      <section className="acct-card wb-action-card"><div className="wb-icon-tile wb-icon-blue"><AppIcon name="requests" size={34} /></div><h2>查看成长任务</h2><p>同步成长中心的真实任务进度。未达标任务显示待完成，作业执行状态单独展示。</p><button className="btn btn-outline btn-sm" disabled={scanning} onClick={() => { setOnlyClaimable(false); void load(); }}>{scanning ? "正在同步…" : "同步任务进度"}</button></section>
      <section className="acct-card wb-action-card"><div className="wb-icon-tile wb-icon-amber"><AppIcon name="gift" size={34} /></div><h2>发现待领奖励</h2><p>筛选已经达标、尚未领取的任务奖励。自动忽略的任务不计入待领奖励。</p><button className="btn btn-outline btn-sm" disabled={scanning} onClick={() => { setOnlyClaimable(true); void load(); }}>查看待领 {claimable ? `（${claimable}）` : ""}</button></section>
    </div>
    <section className="acct-card wb-growth-job">
      <div className="wb-section-heading"><div className="flex flex-wrap items-center gap-3"><div className="wb-icon-tile wb-icon-mint wb-icon-small"><AppIcon name="growth" size={24} /></div><h2>任务作业</h2><span className="wb-muted text-xs">卡片任务与一键全做的统一执行日志</span></div><span className={`wb-soft-badge ${job?.status === "failed" || job?.status === "partial" ? "text-error" : ""}`} role="status">{job ? statusLabel[job.status] : "暂无作业"}</span></div>
      {syncError && <p role="alert" className="text-warning text-sm mt-3">暂时无法同步后台状态，已保留上次进度并自动重试。状态确认前禁止重复启动。{syncError}</p>}
      <details className="wb-task-exclusions"><summary>自动忽略的 6 项任务</summary><ul>{ignoredTasks.map((name) => <li key={name}>{name}</li>)}</ul></details>
      {job ? <>
        <div className="wb-growth-progress"><div className="flex justify-between gap-3 text-xs"><span>已处理账号 <strong>{job.completed} / {job.total}</strong>{running && job.current_uid ? ` · ${config.accounts.find((account) => account.uid === job.current_uid)?.nickname || "当前账号"}` : ""}</span><span className="wb-muted">开始于 {time(job.started_at)}{job.finished_at ? ` · 结束于 ${time(job.finished_at)}` : ""}</span></div><progress className="progress progress-primary mt-3 h-2" value={job.completed} max={Math.max(job.total, 1)} /></div>
        <p className="text-sm mt-3">已接受 {result?.items.reduce((count, item) => count + item.accepted, 0)} 项 · 已领取 {result?.items.reduce((count, item) => count + item.claimed, 0)} 项 · 已忽略 {skipped} 项 · 待完成 {waiting} 项 · 获得 {result?.total_credit ?? 0} 积分 / {result?.total_energy ?? 0} 能量</p>
        {!running && waiting > 0 && <p className="wb-muted text-xs mt-2">本次作业已结束，仍有任务需要在官方客户端完成或等待官方进度更新。详情见下方日志。</p>}
        <div className="wb-task-log-heading"><h3>执行日志</h3><label className="wb-muted text-xs"><input className="checkbox checkbox-primary checkbox-xs" type="checkbox" checked={followLogs} onChange={(event) => setFollowLogs(event.target.checked)} /> 跟随最新日志</label></div>
        <div ref={logs} className="wb-task-logs" role="log" aria-label="任务执行日志" aria-live="polite" aria-relevant="additions">
          {job.logs.length ? job.logs.map((entry) => <div key={`${job.id}-${entry.seq}`} className={`wb-task-log-line ${entry.status === "error" || entry.status === "failed" ? "is-error" : ""}`}><time>{time(entry.time)}</time><span>{entry.nickname ? `${entry.nickname} · ` : ""}{entry.message}</span></div>) : <p className="wb-muted text-xs">作业已启动，等待执行日志…</p>}
        </div>
        <p className="wb-muted text-xs mt-2">保留本次作业最近 500 条日志；切换页面后仍可继续查看。</p>
      </> : <p className="wb-muted text-sm mt-4">点击「执行并自动领奖」，或在账号卡片确认执行任务。进度和日志会显示在这里。</p>}
    </section>
    <section className="acct-card wb-growth-job">
      <div className="wb-toolbar"><h2>任务清单</h2><div className="wb-segment-control"><button className={!onlyClaimable ? "active" : ""} onClick={() => setOnlyClaimable(false)}>全部任务</button><button className={onlyClaimable ? "active" : ""} onClick={() => setOnlyClaimable(true)}>待领奖励 <span>{claimable}</span></button></div></div>
      {scanError && <p role="alert" className="text-error text-sm mt-3">同步任务失败，可点击刷新任务重试。{scanError}</p>}
      <div className="wb-growth-progress"><div className="flex justify-between gap-3 text-xs"><span>已领取进度 <strong className="text-primary">{progress}%</strong> · 忽略 {all.length - eligible.length} 项</span><span className="wb-muted">{claimed} / {eligible.length} 项{lastScan ? ` · 更新于 ${time(lastScan.getTime())}` : ""}</span></div><progress className="progress progress-primary mt-3 h-2" value={claimed} max={Math.max(eligible.length, 1)} /></div>
      {!scan.length && <div className="wb-empty wb-empty-small"><AppIcon name="growth" size={48} /><h3>{scanning ? "正在同步任务" : "还没有任务记录"}</h3><p>{scanning ? "正在读取账号的成长中心。" : "导入并启用国内账号后，点击「同步任务进度」。"}</p></div>}
      <div className="wb-task-accounts">{scan.map((account) => {
        const shown = account.tasks.filter((task) => !onlyClaimable || (!ignored(task) && task.claimable));
        const runResult = result?.items.find((item) => item.uid === account.uid);
        return <div key={account.uid} className="wb-task-account"><div className="flex flex-wrap items-center gap-2"><span className="wb-mini-avatar">{account.nickname.slice(0, 1)}</span><strong className="text-sm">{account.nickname}</strong><span className="wb-muted text-xs">{shown.length} 项任务</span>{runResult && <span className="ml-auto text-xs text-primary">+{runResult.credit} 积分</span>}</div>
          {account.error && <p className="text-error text-xs mt-3">{account.error}</p>}
          {!shown.length && !account.error && <p className="wb-muted text-xs mt-3">{onlyClaimable ? "暂无待领取的奖励" : "暂无任务"}</p>}
          <div className="wb-task-grid">{shown.map((task) => <div key={task.task_code} className={`wb-task-item ${ignored(task) ? "wb-task-ignored" : ""}`}><div className="flex justify-between gap-3"><strong className="text-xs">{task.title || task.task_code}</strong><span className={`wb-task-state ${!ignored(task) && task.claimable ? "is-claimable" : ""}`}>{ignored(task) ? "已忽略" : task.claimed ? "已领取" : task.claimable ? "可领取" : task.locked ? "未解锁" : "待完成"}</span></div><p className="wb-muted text-xs mt-2 line-clamp-2">{ignored(task) ? task.skip_reason || "按设置自动忽略，不执行或领取" : task.description || task.task_desc || "完成任务，获取成长奖励。"}</p><div className="flex justify-between mt-3 text-xs"><span className="text-primary">+{task.credit} 积分</span><span className="wb-muted">{task.current} / {task.target}</span></div><progress className="progress progress-primary mt-2 h-1" value={task.current} max={Math.max(task.target, 1)} /></div>)}</div>
        </div>;
      })}</div>
    </section>
  </div>;
}
