import { useEffect, useState } from "react";
import { reqlogRead, reqlogSnapshot } from "../lib/commands";
import { useApp } from "../stores/appStore";
import { clearExecutionError, reportError } from "../lib/errors";
import type { LogEvent, LogFilter, ReqlogSnapshot } from "../types";
import AppIcon from "./AppIcon";
import "./panels.css";

const initialFilter: LogFilter = { outcome: "", account: "", account_uid: "", model: "", client_ip: "", from_ms: 0, to_ms: 0 };
export default function SharePanel() {
  const { config, status, setNotice } = useApp();
  const [snap, setSnap] = useState<ReqlogSnapshot | null>(null);
  const [logs, setLogs] = useState<LogEvent[]>([]);
  const [filter, setFilter] = useState(initialFilter);
  const [limit, setLimit] = useState(50);
  const [busy, setBusy] = useState(false);
  const [expanded, setExpanded] = useState<string | null>(null);
  const endpoint = status?.running ? status.base_url : `http://127.0.0.1:${config.listen_port}/v1`;
  const load = async (active = filter, count = limit) => {
    setBusy(true); clearExecutionError();
    try { const [s, l] = await Promise.all([reqlogSnapshot(), reqlogRead(count, active)]); setSnap(s); setLogs(l); }
    catch (e) { reportError(e); } finally { setBusy(false); }
  };
  useEffect(() => { void load(); }, []);
  const copy = async (value: string) => {
    clearExecutionError();
    try { await navigator.clipboard.writeText(value); setNotice("已复制到剪贴板"); } catch (e) { reportError(e); }
  };
  const accountName = (uid: string) => config.accounts.find((a) => a.uid === uid)?.nickname || uid || "—";
  return <div className="wb-panel share-panel">
    <div className="wb-page-heading"><div><h1 className="wb-page-title">分享连接，掌握每次请求</h1><p className="wb-page-description">复制接口信息，检索运行记录，追踪网关的真实使用情况。</p></div><button className="btn btn-outline btn-sm" disabled={busy} onClick={() => void load()}><AppIcon name="refresh" size={18} />{busy ? "刷新中…" : "刷新日志"}</button></div>
    <div className="wb-share-grid"><section className="acct-card wb-form-card"><div className="wb-section-heading"><div className="flex items-center gap-3"><div className="wb-icon-tile wb-icon-mint wb-icon-small"><AppIcon name="share" size={25} /></div><h2>分享你的连接</h2></div><span className="wb-soft-badge">本地接口</span></div><p className="wb-muted text-sm mt-4 leading-7">将接口地址填入本机 AI 客户端，即可使用 OpenAI 兼容网关。</p><div className="wb-share-endpoint"><code>{endpoint}</code><button className="btn btn-outline btn-sm" onClick={() => void copy(endpoint)}>复制地址</button></div><button className="btn btn-sm wb-btn-tint mt-3" onClick={() => void copy(`WorkBuddy 本地账号工具箱\n支持账号管理、签到、成长任务与 OpenAI 兼容网关。\n本机接口：${endpoint}\n在 Cherry Studio / ChatBox / Cline 中填写此地址及单独提供的 API Key 即可连接。`)}><AppIcon name="share" size={18} />复制分享文案</button></section>
    <section className="acct-card wb-form-card"><div className="wb-section-heading"><div className="flex items-center gap-3"><div className="wb-icon-tile wb-icon-blue wb-icon-small"><AppIcon name="requests" size={25} /></div><h2>运行概览</h2></div><span className="wb-muted text-xs">本次运行</span></div><div className="wb-gateway-metrics"><div className="stat-tile"><span>已完成请求</span><strong>{snap?.completed.toLocaleString() ?? "—"}</strong></div><div className="stat-tile"><span>成功率</span><strong className="text-primary">{snap?.completed ? `${snap.success_rate.toFixed(1)}%` : "—"}</strong></div><div className="stat-tile"><span>平均耗时</span><strong>{snap?.completed ? `${(snap.avg_duration_ms / 1000).toFixed(2)} s` : "—"}</strong></div></div><div className="wb-log-summary"><span><span className="wb-status-dot" /> {snap?.succeeded ?? 0} 次成功</span><span><span className="wb-status-dot is-warning" /> {snap?.failed ?? 0} 次失败</span><span>{snap?.in_flight ?? 0} 个请求处理中</span></div></section></div>
    <section className="acct-card wb-form-card"><div className="wb-section-heading"><div className="flex items-center gap-3"><AppIcon name="requests" size={29} /><h2>请求日志</h2><span className="wb-count">{logs.length}</span></div><span className="wb-muted text-xs">点击请求查看详情</span></div>
      <form className="wb-log-filters" onSubmit={(e) => { e.preventDefault(); void load(); }}><label className="wb-field"><span>账号</span><select className="select select-bordered select-sm" value={filter.account_uid ?? ""} onChange={(e) => setFilter({ ...filter, account_uid: e.target.value })}><option value="">全部账号</option>{config.accounts.map((a) => <option key={a.uid} value={a.uid}>{a.nickname}</option>)}</select></label><label className="wb-field"><span>模型</span><input className="input input-bordered input-sm" value={filter.model} onChange={(e) => setFilter({ ...filter, model: e.target.value })} placeholder="模型名称" /></label><label className="wb-field"><span>来源 IP</span><input className="input input-bordered input-sm" value={filter.client_ip} onChange={(e) => setFilter({ ...filter, client_ip: e.target.value })} placeholder="全部来源" /></label><label className="wb-field"><span>显示条数</span><select className="select select-bordered select-sm" value={limit} onChange={(e) => setLimit(Number(e.target.value))}><option value={20}>20 条</option><option value={50}>50 条</option><option value={100}>100 条</option><option value={200}>200 条</option></select></label><button className="btn btn-primary btn-sm" type="submit" disabled={busy}>筛选</button><button className="btn btn-ghost btn-sm" type="button" disabled={busy} onClick={() => { setFilter(initialFilter); void load(initialFilter); }}>重置</button></form>
      <div className="overflow-x-auto mt-5"><table className="table table-sm wb-log-table"><thead><tr><th>时间</th><th>账号</th><th>模型</th><th>状态</th><th>Token</th><th>耗时</th><th>详情</th></tr></thead><tbody>{!logs.length && <tr><td colSpan={7}><div className="wb-empty wb-empty-small"><AppIcon name="requests" size={50} /><h3>{busy ? "正在读取请求" : "暂无请求记录"}</h3><p>通过网关发送请求后，实际运行记录会显示在这里。</p></div></td></tr>}{logs.map((log) => <LogRow key={log.request_id} log={log} account={accountName(log.account_uid || log.account)} expanded={expanded === log.request_id} onToggle={() => setExpanded(expanded === log.request_id ? null : log.request_id)} />)}</tbody></table></div>
    </section>
    {snap && <section className="acct-card wb-archive-card"><div className="flex items-center gap-3"><AppIcon name="settings" size={28} /><div><h2 className="text-sm font-semibold">日志归档</h2><p className="text-xs wb-muted mt-1">{snap.archive.enabled ? `已启用 · ${snap.archive.files} 个文件 · ${(snap.archive.bytes / 1024 / 1024).toFixed(2)} MB` : "归档未启用"} · 保留 {config.logging.retention_days} 天</p></div></div><code className="wb-archive-path" title={snap.archive.dir}>{snap.archive.dir}</code>{snap.archive.last_error && <p className="text-error text-xs w-full">{snap.archive.last_error}</p>}</section>}
  </div>;
}

function LogRow({ log, account, expanded, onToggle }: { log: LogEvent; account: string; expanded: boolean; onToggle: () => void }) {
  return <><tr className={expanded ? "wb-log-expanded" : ""}><td className="whitespace-nowrap"><span className="block">{new Date(log.time).toLocaleTimeString("zh-CN")}</span><small className="wb-muted">{new Date(log.time).toLocaleDateString("zh-CN")}</small></td><td>{account}</td><td><code>{log.model || "—"}</code></td><td><span className={`badge badge-sm ${log.ok ? "badge-success" : "badge-error"}`}>{log.status || (log.ok ? "成功" : "失败")}</span></td><td className="font-mono">{log.total_tokens.toLocaleString()}</td><td>{(log.duration_ms / 1000).toFixed(2)} s</td><td><button className="btn btn-ghost btn-xs" aria-expanded={expanded} onClick={onToggle}>{expanded ? "收起 −" : "查看 ＋"}</button></td></tr>{expanded && <tr><td colSpan={7}><dl className="wb-log-detail"><div><dt>请求 ID</dt><dd>{log.request_id}</dd></div><div><dt>请求路径</dt><dd>{log.path}</dd></div><div><dt>输入 / 输出 Token</dt><dd>{log.prompt_tokens.toLocaleString()} / {log.completion_tokens.toLocaleString()}</dd></div><div><dt>首字延迟 / 尝试次数</dt><dd>{log.ttfb_ms != null ? `${log.ttfb_ms} ms` : "—"} / {log.attempts ?? "—"}</dd></div><div><dt>来源 IP</dt><dd>{log.client_ip || "—"}</dd></div><div><dt>结果</dt><dd>{log.outcome || "—"}</dd></div><div className="sm:col-span-2"><dt>客户端</dt><dd>{log.user_agent || "—"}</dd></div></dl></td></tr>}</>;
}


