import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { isTauri } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import { writeTextFile } from "@tauri-apps/plugin-fs";
import { reqlogRead, usageDashboard } from "../lib/commands";
import { useApp } from "../stores/appStore";
import { clearExecutionError, reportError } from "../lib/errors";
import type { LogEvent, UsageBreakdown, UsageDashboard, UsagePoint, UsageQuery } from "../types";
import AppIcon from "./AppIcon";
import type { Page } from "./Sidebar";
import "./usage.css";

type Range = "today" | "7d" | "30d" | "all" | "custom";
type ChartMetric = "tokens" | "requests";
type SortKey = "total_tokens" | "requests" | "credit" | "errors" | "avg_latency_ms";
const RANGES: { id: Range; label: string }[] = [
  { id: "today", label: "今日" }, { id: "7d", label: "近 7 天" },
  { id: "30d", label: "近 30 天" }, { id: "all", label: "全部" }, { id: "custom", label: "自定义" },
];
const DAY = 86_400_000;
const OFFSET = 8 * 3_600_000;
const number = (n: number) => n.toLocaleString("zh-CN", { maximumFractionDigits: 0 });
const compact = (n: number) => n >= 1_000_000 ? `${(n / 1_000_000).toFixed(2)}M` : n >= 10_000 ? `${(n / 1_000).toFixed(1)}K` : number(n);
const duration = (ms: number | null | undefined) => ms == null || ms < 0 ? "—" : ms >= 1000 ? `${(ms / 1000).toFixed(2)} s` : `${Math.round(ms)} ms`;
const dateKey = (ms: number) => new Date(ms + OFFSET).toISOString().slice(0, 10);
const dateTime = (ms: number, options?: Intl.DateTimeFormatOptions) => new Intl.DateTimeFormat("zh-CN", {
  timeZone: "Asia/Shanghai", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", hour12: false, ...options,
}).format(ms);

function getWindow(range: Range, from: string, to: string): UsageQuery {
  const now = Date.now();
  const midnight = Math.floor((now + OFFSET) / DAY) * DAY - OFFSET;
  if (range === "all") return { granularity: "day" };
  if (range === "custom") {
    const start = Date.parse(`${from}T00:00:00+08:00`);
    const end = Math.min(Date.parse(`${to}T00:00:00+08:00`) + DAY, now);
    if (!Number.isFinite(start) || !Number.isFinite(end) || start >= end) throw new Error("请选择有效的日期范围，开始日期应早于结束日期。");
    return { from_ms: start, to_ms: end, granularity: end - start > 3 * DAY ? "day" : "hour" };
  }
  const days = range === "today" ? 0 : range === "7d" ? 6 : 29;
  return { from_ms: midnight - days * DAY, to_ms: now, granularity: range === "today" ? "hour" : "day" };
}

function UsageChart({ points, metric, granularity, loading }: {
  points: UsagePoint[]; metric: ChartMetric; granularity: "hour" | "day"; loading: boolean;
}) {
  const id = useId().replace(/:/g, "");
  const [hover, setHover] = useState<number | null>(null);
  const width = 940, height = 254, left = 48, right = 18, top = 20, bottom = 34;
  const chartWidth = width - left - right, chartHeight = height - top - bottom;
  const first = points.map((p) => metric === "tokens" ? p.prompt_tokens : Math.max(0, p.requests - p.errors));
  const second = points.map((p) => metric === "tokens" ? p.completion_tokens : p.errors);
  const maximum = Math.max(4, ...first, ...second);
  const magnitude = 10 ** Math.floor(Math.log10(maximum));
  const ceiling = Math.ceil(maximum / magnitude) * magnitude;
  const x = (i: number) => left + (points.length === 1 ? chartWidth / 2 : i * chartWidth / Math.max(1, points.length - 1));
  const y = (v: number) => top + chartHeight * (1 - v / ceiling);
  const line = (values: number[]) => values.map((v, i) => `${i === 0 ? "M" : "L"}${x(i).toFixed(2)},${y(v).toFixed(2)}`).join(" ");
  const area = (values: number[]) => values.length ? `${line(values)} L${x(values.length - 1)},${top + chartHeight} L${x(0)},${top + chartHeight} Z` : "";
  const tickIndices = Array.from(new Set(Array.from({ length: Math.min(6, points.length) }, (_, i) => Math.round(i * (points.length - 1) / Math.max(1, Math.min(6, points.length) - 1)))));
  const hasData = points.some((p) => p.requests > 0 || p.total_tokens > 0);
  const active = hover != null ? points[hover] : null;
  return (
    <div className="us-chart-wrap" onMouseLeave={() => setHover(null)}>
      <svg className="us-chart" viewBox={`0 0 ${width} ${height}`} role="img" aria-label={`${metric === "tokens" ? "输入与输出 Token" : "成功与失败请求"}按${granularity === "hour" ? "小时" : "日"}变化的趋势图`}>
        <defs>
          <linearGradient id={`${id}-fill`} x1="0" x2="0" y1="0" y2="1"><stop offset="0%" stopColor="#09bc75" stopOpacity=".20" /><stop offset="100%" stopColor="#09bc75" stopOpacity="0" /></linearGradient>
          <linearGradient id={`${id}-blue`} x1="0" x2="0" y1="0" y2="1"><stop offset="0%" stopColor="#54a9d7" stopOpacity=".09" /><stop offset="100%" stopColor="#54a9d7" stopOpacity="0" /></linearGradient>
        </defs>
        {[0, 1, 2, 3, 4].map((i) => <g key={i}><line x1={left} x2={width - right} y1={y(ceiling * i / 4)} y2={y(ceiling * i / 4)} className="us-chart-grid" /><text x={left - 12} y={y(ceiling * i / 4) + 4} textAnchor="end" className="us-chart-label">{compact(ceiling * i / 4)}</text></g>)}
        {tickIndices.map((i) => <text key={i} x={x(i)} y={height - 8} textAnchor="middle" className="us-chart-label">{granularity === "hour" ? dateTime(points[i].time_ms, { month: undefined, day: undefined }) : dateTime(points[i].time_ms, { hour: undefined, minute: undefined })}</text>)}
        {hasData && <>
          <path d={area(first)} fill={`url(#${id}-fill)`} /><path d={area(second)} fill={`url(#${id}-blue)`} />
          <path d={line(first)} fill="none" stroke="#0bb878" strokeWidth="2.5" strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
          <path d={line(second)} fill="none" stroke={metric === "tokens" ? "#64a9d4" : "#e77e75"} strokeWidth="2" strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
          {points.length === 1 && <><circle cx={x(0)} cy={y(first[0])} r="4" fill="#0bb878" /><circle cx={x(0)} cy={y(second[0])} r="3" fill="#64a9d4" /></>}
          {hover != null && points[hover] && <g><line x1={x(hover)} x2={x(hover)} y1={top} y2={height - bottom} stroke="#a0bbaf" strokeDasharray="4 4" /><circle cx={x(hover)} cy={y(first[hover])} r="4" fill="#0bb878" stroke="white" strokeWidth="2" /><circle cx={x(hover)} cy={y(second[hover])} r="4" fill={metric === "tokens" ? "#64a9d4" : "#e77e75"} stroke="white" strokeWidth="2" /></g>}
          <rect x={left} y={top} width={chartWidth} height={chartHeight} fill="transparent" onMouseMove={(e) => { const bounds = e.currentTarget.ownerSVGElement!.getBoundingClientRect(); const position = (e.clientX - bounds.left) / bounds.width * width; setHover(Math.max(0, Math.min(points.length - 1, Math.round((position - left) / chartWidth * (points.length - 1))))); }} />
        </>}
      </svg>
      {!hasData && <div className="us-chart-empty"><AppIcon name="usage" size={40} /><strong>{loading ? "正在读取用量…" : "这段时间，尚无用量记录"}</strong><span>通过 OpenAI 网关发送请求后，趋势会在这里呈现</span></div>}
      {active && hover != null && hasData && <div className="us-chart-tooltip" style={{ left: `${Math.min(72, Math.max(8, x(hover) / width * 100))}%` }}><strong>{dateTime(active.time_ms)}</strong><span><i className="us-dot" />{metric === "tokens" ? "输入 Token" : "成功请求"}<b>{number(first[hover])}</b></span><span><i className={`us-dot ${metric === "tokens" ? "blue" : "red"}`} />{metric === "tokens" ? "输出 Token" : "失败请求"}<b>{number(second[hover])}</b></span></div>}
    </div>
  );
}

export default function OverviewPanel({ onNavigate }: { onNavigate: (p: Page) => void }) {
  const { config, status, setNotice } = useApp();
  const [range, setRange] = useState<Range>("today");
  const [customFrom, setCustomFrom] = useState(() => dateKey(Date.now() - 6 * DAY));
  const [customTo, setCustomTo] = useState(() => dateKey(Date.now()));
  const [account, setAccount] = useState("");
  const [model, setModel] = useState("");
  const [metric, setMetric] = useState<ChartMetric>("tokens");
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [data, setData] = useState<UsageDashboard | null>(null);
  const [logs, setLogs] = useState<LogEvent[]>([]);
  const [error, setError] = useState("");
  const [logError, setLogError] = useState("");
  const [busy, setBusy] = useState(true);
  const [dimension, setDimension] = useState<"model" | "account">("model");
  const [sort, setSort] = useState<SortKey>("total_tokens");
  const [showSeries, setShowSeries] = useState(false);
  const [knownModels, setKnownModels] = useState<string[]>([]);
  const [knownAccounts, setKnownAccounts] = useState<string[]>([]);
  const requestId = useRef(0);
  const activeFilter = useRef("");
  const accountName = useCallback((uid: string) => config.accounts.find((a) => a.uid === uid)?.nickname || uid || "未识别账号", [config.accounts]);

  const load = useCallback(async (showFailure = false) => {
    const id = ++requestId.current;
    if (showFailure) clearExecutionError();
    setBusy(true);
    let query: UsageQuery;
    try { query = { ...getWindow(range, customFrom, customTo), uid: account, model, timezone_offset_minutes: 480 }; }
    catch (e) {
      setError(String(e instanceof Error ? e.message : e)); setData(null); setLogs([]); setLogError(""); setBusy(false);
      if (showFailure) reportError(e);
      return;
    }
    const filter = JSON.stringify([range, customFrom, customTo, account, model]);
    if (activeFilter.current !== filter) { activeFilter.current = filter; setData(null); setLogs([]); setError(""); setLogError(""); }
    const results = await Promise.allSettled([
      usageDashboard(query),
      reqlogRead(8, {
        from_ms: query.from_ms ?? 0, to_ms: query.to_ms ?? 0, account: "", account_uid: account,
        model: "", model_exact: query.model, outcome: "", client_ip: "",
      }),
    ]);
    if (id !== requestId.current) return;
    if (results[0].status === "fulfilled") {
      const snapshot = results[0].value;
      setData(snapshot); setError("");
      setKnownModels((prev) => [...new Set([...prev, ...snapshot.by_model.map((m) => m.model)])]);
      setKnownAccounts((prev) => [...new Set([...prev, ...snapshot.by_account.map((a) => a.uid)])]);
    } else { setError(String(results[0].reason)); setData(null); }
    if (results[1].status === "fulfilled") { setLogs(results[1].value); setLogError(""); }
    else { setLogs([]); setLogError(String(results[1].reason)); }
    if (showFailure) {
      const failures = results.flatMap((result, index) => result.status === "rejected"
        ? [`${index === 0 ? "读取用量统计" : "读取请求日志"}失败：${String(result.reason)}`] : []);
      if (failures.length) reportError(failures.join("\n"));
    }
    setBusy(false);
  }, [range, customFrom, customTo, account, model, accountName]);

  useEffect(() => { void load(); return () => { requestId.current += 1; }; }, [load]);
  useEffect(() => {
    if (!autoRefresh) return;
    const timer = window.setInterval(() => { if (!document.hidden) void load(); }, 15_000);
    return () => window.clearInterval(timer);
  }, [autoRefresh, load]);

  const models = useMemo(() => {
    // The gateway records configured aliases with a realm prefix. Older observed names
    // remain selectable exactly as recorded.
    const configured = config.models.map(({ codex_name }) =>
      knownModels.includes(codex_name) ? codex_name : `cn:${codex_name}`);
    return [...new Set([...configured, ...knownModels])].sort();
  }, [config.models, knownModels]);
  const accounts = useMemo(() => [...new Set([...config.accounts.map((a) => a.uid), ...knownAccounts])], [config.accounts, knownAccounts]);
  const rows = useMemo(() => {
    const items: (UsageBreakdown & { id: string; label: string })[] = dimension === "model"
      ? (data?.by_model ?? []).map((item) => ({ ...item, id: item.model, label: item.model || "未知模型" }))
      : (data?.by_account ?? []).map((item) => ({ ...item, id: item.uid, label: accountName(item.uid) }));
    return items.sort((a, b) => b[sort] - a[sort]);
  }, [data, dimension, sort, accountName]);
  const [rowPage, setRowPage] = useState(0);
  useEffect(() => setRowPage(0), [dimension, sort, range, account, model]);
  const pageCount = Math.ceil(rows.length / 8);
  const visibleRows = rows.slice(Math.min(rowPage, Math.max(0, pageCount - 1)) * 8, (Math.min(rowPage, Math.max(0, pageCount - 1)) + 1) * 8);
  const total = data?.total_tokens ?? 0;
  const promptPercent = total ? Math.max(0, Math.min(100, (data?.prompt_tokens ?? 0) / total * 100)) : 0;
  const completionPercent = total ? Math.max(0, Math.min(100 - promptPercent, (data?.completion_tokens ?? 0) / total * 100)) : 0;
  const hasRequests = !!data?.total_requests;
  const stats = [
    { icon: "requests" as const, label: "请求次数", value: data ? number(data.total_requests) : "—", note: data ? `${number(data.total_errors)} 次失败尝试` : "等待统计数据", accent: true },
    { icon: "tokens" as const, label: "总 Token 用量", value: data ? compact(total) : "—", note: "输入 + 输出 Token", accent: false },
    { icon: "tokens" as const, label: "输入 Token", value: data ? compact(data.prompt_tokens) : "—", note: "Prompt tokens", accent: false },
    { icon: "tokens" as const, label: "输出 Token", value: data ? compact(data.completion_tokens) : "—", note: "Completion tokens", accent: false },
    { icon: "success" as const, label: "请求成功率", value: hasRequests ? `${data!.success_rate.toFixed(1)}%` : "—", note: hasRequests ? `${number(data!.total_requests - data!.total_errors)} 次成功尝试` : "暂无请求样本", accent: false },
    { icon: "latency" as const, label: "平均请求耗时", value: duration(hasRequests ? data?.avg_latency_ms : null), note: "完整请求的平均耗时", accent: false },
  ];

  const exportCsv = async () => {
    const escape = (v: unknown) => {
      const value = typeof v === "string" && /^[=+@\-\t\r]/.test(v) ? `'${v}` : String(v);
      return `"${value.replace(/"/g, '""')}"`;
    };
    const output = "\ufeff" + [
      [dimension === "model" ? "模型" : "账号", "请求数", "失败数", "输入Token", "输出Token", "总Token", "记录积分", "平均耗时ms"],
      ...rows.map((r) => [r.label, r.requests, r.errors, r.prompt_tokens, r.completion_tokens, r.total_tokens, r.credit, r.avg_latency_ms]),
    ].map((row) => row.map(escape).join(",")).join("\r\n");
    try {
      const fileName = `workbuddy-usage-${dateKey(Date.now())}.csv`;
      if (isTauri()) {
        const path = await save({ defaultPath: fileName, filters: [{ name: "CSV 表格", extensions: ["csv"] }] });
        if (!path) return;
        await writeTextFile(path, output);
      } else {
        const url = URL.createObjectURL(new Blob([output], { type: "text/csv;charset=utf-8;" }));
        const a = document.createElement("a"); a.href = url; a.download = fileName; a.click();
        window.setTimeout(() => URL.revokeObjectURL(url), 1000);
      }
      setNotice("用量明细已导出");
    } catch (e) { reportError(`导出失败：${String(e)}`); }
  };

  return (
    <div className="wb-panel usage-panel">
      <div className="wb-page-heading us-heading">
        <div><h1 className="wb-page-title">用量统计<span className="us-title-dot" /></h1><p className="wb-page-description">每一次调用，都清晰可见。查看网关用量、运行趋势与积分消耗。</p></div>
        <div className={`us-gateway-state ${status?.running ? "running" : ""}`}><span className="us-status-dot" />{status?.running ? "网关运行中" : "网关未启动"}<button onClick={() => onNavigate("gateway")} aria-label="前往网关设置">网关设置 ↗</button></div>
      </div>

      <section className="card bg-base-100 us-filterbar" aria-label="用量筛选条件">
        <div className="us-filter-primary">
          <div className="us-segments" aria-label="统计时间范围">{RANGES.map((item) => <button key={item.id} className={range === item.id ? "active" : ""} aria-pressed={range === item.id} onClick={() => setRange(item.id)}>{item.label}</button>)}</div>
          <div className="us-filter-selects"><label><span className="us-sr-only">筛选账号</span><select className="select select-bordered select-sm" value={account} onChange={(e) => setAccount(e.target.value)}><option value="">全部账号</option>{accounts.map((uid) => <option key={uid} value={uid}>{accountName(uid)}</option>)}</select></label><label><span className="us-sr-only">筛选模型</span><select className="select select-bordered select-sm" value={model} onChange={(e) => setModel(e.target.value)}><option value="">全部模型</option>{models.map((name) => <option key={name} value={name}>{name}</option>)}</select></label></div>
          <button className="btn btn-sm us-refresh-btn" disabled={busy} onClick={() => void load(true)}><AppIcon name="refresh" size={18} />{busy ? "刷新中" : "刷新数据"}</button>
        </div>
        {range === "custom" && <div className="us-custom-range"><label>开始日期<input className="input input-bordered input-sm" type="date" value={customFrom} max={customTo} onChange={(e) => setCustomFrom(e.target.value)} /></label><span>—</span><label>结束日期<input className="input input-bordered input-sm" type="date" value={customTo} min={customFrom} max={dateKey(Date.now())} onChange={(e) => setCustomTo(e.target.value)} /></label><span className="us-muted">包含结束日，今日统计至当前时刻</span></div>}
        <div className="us-filter-foot"><span>{data?.window_from_ms != null ? `${dateTime(data.window_from_ms)} — ${data.window_to_ms != null ? dateTime(data.window_to_ms) : "现在"}` : "全部历史记录"}<span className="us-foot-sep">·</span>UTC+8<span className="us-foot-sep">·</span>小时桶聚合</span><label className="us-auto-refresh"><input className="checkbox checkbox-primary checkbox-xs" type="checkbox" checked={autoRefresh} onChange={(e) => setAutoRefresh(e.target.checked)} />每 15 秒自动刷新{data && <span className="us-updated">更新于 {dateTime(data.saved_at, { month: undefined, day: undefined, second: "2-digit" })}</span>}</label></div>
      </section>

      {error && <div className="us-error"><strong>暂时无法读取用量统计</strong><span>{error}</span><button className="btn btn-sm" onClick={() => void load(true)}>重新加载</button></div>}
      <div className="us-kpi-grid" aria-busy={busy}>
        {stats.map((stat, i) => <section key={stat.label} className={`card bg-base-100 us-kpi ${stat.accent ? "featured" : ""}`}><div className="us-kpi-label"><span>{stat.label}</span><span className={`us-kpi-icon tone-${i}`}><AppIcon name={stat.icon} size={21} /></span></div><div className="us-kpi-value" title={data && i === 1 ? number(total) : undefined}>{stat.value}</div><div className="us-kpi-note">{stat.note}</div></section>)}
      </div>

      <div className="us-charts-grid">
        <section className="card bg-base-100 us-trend-card">
          <div className="us-card-heading"><div><h2><AppIcon name="usage" size={23} />用量趋势</h2><p>{data?.granularity === "hour" ? "按小时" : "按日"}查看调用变化</p></div><div className="us-segments us-small-segments"><button className={metric === "tokens" ? "active" : ""} aria-pressed={metric === "tokens"} onClick={() => setMetric("tokens")}>Token</button><button className={metric === "requests" ? "active" : ""} aria-pressed={metric === "requests"} onClick={() => setMetric("requests")}>请求数</button></div></div>
          <div className="us-chart-legend"><span><i className="us-dot" />{metric === "tokens" ? "输入 Token" : "成功请求"}</span><span><i className={`us-dot ${metric === "tokens" ? "blue" : "red"}`} />{metric === "tokens" ? "输出 Token" : "失败请求"}</span><button onClick={() => setShowSeries((v) => !v)} aria-expanded={showSeries}>{showSeries ? "收起数据表" : "查看数据表"}</button></div>
          <UsageChart points={data?.series ?? []} metric={metric} granularity={data?.granularity ?? "hour"} loading={busy} />
          {showSeries && <div className="us-series-table us-table-scroll"><table className="table table-sm"><caption className="us-sr-only">趋势图的统计数据</caption><thead><tr><th>时间 (UTC+8)</th><th>请求</th><th>失败</th><th>输入 Token</th><th>输出 Token</th></tr></thead><tbody>{(data?.series ?? []).map((p) => <tr key={p.time_ms}><td>{dateTime(p.time_ms)}</td><td>{number(p.requests)}</td><td>{number(p.errors)}</td><td>{number(p.prompt_tokens)}</td><td>{number(p.completion_tokens)}</td></tr>)}{!data?.series.length && <tr><td colSpan={5} className="us-empty-cell">暂无趋势数据</td></tr>}</tbody></table></div>}
          <div className="us-card-bottom"><span><span className="us-status-dot" />数据来自本地网关请求记录</span><span>不记录提示词或响应正文</span></div>
        </section>

        <section className="card bg-base-100 us-token-card">
          <div className="us-card-heading"><div><h2><AppIcon name="tokens" size={23} />Token 构成</h2><p>输入与输出的用量占比</p></div></div>
          <div className="us-donut-wrap"><div className="us-donut" style={{ background: total ? `conic-gradient(#0cbe7d 0% ${promptPercent}%, #83bfe2 ${promptPercent}% ${promptPercent + completionPercent}%, #dfebe6 ${promptPercent + completionPercent}% 100%)` : undefined }}><div className="us-donut-center"><span>总 Token</span><strong title={number(total)}>{data ? compact(total) : "—"}</strong></div></div></div>
          <div className="us-token-legend"><div><span><i className="us-dot" />输入 Token</span><b>{data ? `${promptPercent.toFixed(1)}%` : "—"}</b><small>{data ? number(data.prompt_tokens) : "—"}</small></div><div><span><i className="us-dot blue" />输出 Token</span><b>{data ? `${completionPercent.toFixed(1)}%` : "—"}</b><small>{data ? number(data.completion_tokens) : "—"}</small></div></div>
        </section>
      </div>

      <section className="card bg-base-100 us-live-card">
        <div className="us-live-title"><span className="us-live-icon"><AppIcon name="gateway" size={27} /></span><div><h2>网关运行指标</h2><p><span className="us-status-dot" />实时速率 · 最近 60 秒</p></div></div>
        <div className="us-live-metric"><span>RPM <small>请求 / 分钟</small></span><strong>{data ? number(data.live_rpm) : "—"}</strong></div>
        <div className="us-live-metric"><span>TPM <small>Token / 分钟</small></span><strong>{data ? compact(data.live_tpm) : "—"}</strong></div>
        <div className="us-live-metric"><span>平均首字延迟 <small>所选时间范围</small></span><strong>{duration(data?.avg_ttfb_ms)}</strong></div>
        <div className="us-live-metric"><span>生成速度 <small>所选时间范围</small></span><strong>{data?.avg_tokens_per_second != null ? `${data.avg_tokens_per_second.toFixed(1)}` : "—"}{data?.avg_tokens_per_second != null && <em> tok/s</em>}</strong></div>
        <div className="us-live-metric"><span>缓存命中率 <small>仅含观测到的缓存样本</small></span><strong>{data?.cache_hit_rate != null ? `${data.cache_hit_rate.toFixed(1)}%` : "—"}</strong></div>
        <div className="us-live-metric us-credit-metric"><span>记录积分 <small>所选时间范围累计</small></span><strong>{data ? data.total_credit.toLocaleString("zh-CN", { maximumFractionDigits: 2 }) : "—"}<em> 分</em></strong></div>
      </section>

      <section className="card bg-base-100 us-breakdown-card">
        <div className="us-card-heading us-breakdown-heading"><div><h2><AppIcon name="vault" size={23} />用量明细</h2><p>按模型或账号，了解每一份用量</p></div><div className="us-breakdown-controls"><div className="us-segments us-small-segments"><button className={dimension === "model" ? "active" : ""} aria-pressed={dimension === "model"} onClick={() => setDimension("model")}>按模型</button><button className={dimension === "account" ? "active" : ""} aria-pressed={dimension === "account"} onClick={() => setDimension("account")}>按账号</button></div><label><span className="us-sr-only">明细排序方式</span><select className="select select-bordered select-sm" value={sort} onChange={(e) => setSort(e.target.value as SortKey)}><option value="total_tokens">按 Token 排序</option><option value="requests">按请求数排序</option><option value="credit">按积分排序</option><option value="errors">按失败数排序</option><option value="avg_latency_ms">按耗时排序</option></select></label><button className="btn btn-sm us-export-btn" disabled={!rows.length} onClick={() => void exportCsv()}>导出 CSV ↗</button></div></div>
        <div className="us-table-scroll"><table className="table us-breakdown-table"><caption className="us-sr-only">当前筛选范围内按{dimension === "model" ? "模型" : "账号"}聚合的用量明细</caption><thead><tr><th>{dimension === "model" ? "模型" : "账号"}</th><th>请求次数</th><th>输入 / 输出 Token</th><th>总 Token / 占比</th><th>成功率</th><th>平均耗时</th><th>记录积分</th></tr></thead><tbody>{visibleRows.map((row, i) => <tr key={row.id}><td><div className="us-dimension-name"><span className={`us-rank rank-${i % 3}`}>{rowPage * 8 + i + 1}</span><strong title={row.label}>{row.label}</strong></div></td><td>{number(row.requests)}<span className="us-cell-secondary">{number(row.errors)} 次失败</span></td><td>{compact(row.prompt_tokens)}<span className="us-token-slash"> / </span><span className="us-text-blue">{compact(row.completion_tokens)}</span></td><td><strong>{compact(row.total_tokens)}</strong><div className="us-share-bar"><i style={{ width: `${total ? row.total_tokens / total * 100 : 0}%` }} /><span>{total ? (row.total_tokens / total * 100).toFixed(1) : "0"}%</span></div></td><td><span className={`us-success-label ${row.errors > 0 ? "with-errors" : ""}`}>{row.requests ? `${((row.requests - row.errors) / row.requests * 100).toFixed(1)}%` : "—"}</span></td><td>{duration(row.avg_latency_ms)}</td><td>{row.credit.toLocaleString("zh-CN", { maximumFractionDigits: 2 })}</td></tr>)}{!rows.length && <tr><td colSpan={7}><div className="us-table-empty"><AppIcon name={dimension === "model" ? "tokens" : "vault"} size={37} /><strong>{error ? "统计暂不可用" : "暂无用量明细"}</strong><span>试试调整时间范围，或通过网关发起第一次调用</span></div></td></tr>}</tbody></table></div>
        <div className="us-card-bottom"><span>共 {rows.length} 个{dimension === "model" ? "模型" : "账号"}<span className="us-foot-sep">·</span>积分为网关记录值</span>{pageCount > 1 ? <div className="us-pagination"><button disabled={rowPage === 0} onClick={() => setRowPage((v) => v - 1)} aria-label="上一页">‹</button><span>{Math.min(rowPage + 1, pageCount)} / {pageCount}</span><button disabled={rowPage >= pageCount - 1} onClick={() => setRowPage((v) => v + 1)} aria-label="下一页">›</button></div> : <span>统计范围内所有调用尝试</span>}</div>
      </section>

      <section className="card bg-base-100 us-recent-card">
        <div className="us-card-heading"><div><h2><AppIcon name="share" size={23} />最近请求</h2><p>当前筛选条件下的最近 8 次调用</p></div><button className="us-text-link" onClick={() => onNavigate("share")}>查看全部日志 ↗</button></div>
        {logError ? <div className="us-log-error" role="alert">请求日志读取失败：{logError}</div> : <div className="us-table-scroll"><table className="table table-sm"><thead><tr><th>请求时间 (UTC+8)</th><th>账号</th><th>模型</th><th>状态</th><th>Token</th><th>耗时</th></tr></thead><tbody>{logs.map((log, index) => <tr key={`${log.request_id}-${index}`}><td>{dateTime(log.time, { second: "2-digit" })}</td><td>{log.account || "—"}</td><td><code className="us-model-name">{log.model || "—"}</code></td><td><span className={`us-http-status ${log.ok ? "ok" : "failed"}`}>{log.ok ? "成功" : "失败"} · {log.status || "—"}</span></td><td>{log.total_tokens < 0 ? "—" : number(log.total_tokens)}</td><td>{duration(log.duration_ms)}</td></tr>)}{!logs.length && <tr><td colSpan={6} className="us-empty-cell">尚无请求记录，新的调用会自动出现在这里</td></tr>}</tbody></table></div>}
      </section>
      <div className="us-page-foot"><span>WORKBUDDY · 本地用量中心</span><span>时序按小时桶汇总，日期按 UTC+8 展示；实时速率独立于历史时间范围。</span></div>
    </div>
  );
}
