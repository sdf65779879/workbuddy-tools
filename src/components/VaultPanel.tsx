import { useMemo, useRef, useState, type MouseEvent } from "react";
import { checkinOne, extractTokenFromLogs, getConfig, observeImportIp, readCurrentWorkbuddyAccount, refreshWorkbuddyToken, switchAccount } from "../lib/commands";
import { useApp } from "../stores/appStore";
import { useTasks } from "../stores/taskStore";
import { checkinMessage, hasCheckedInToday } from "../lib/checkin";
import { clearExecutionError, reportError } from "../lib/errors";
import type { AccountConfig, AccountStatusView, BrowserLoginPoll } from "../types";
import AppIcon from "./AppIcon";
import BrowserLoginPanel from "./BrowserLoginPanel";
import AccountDialog from "./AccountDialog";
import "./panels.css";

function validUid(uid: string) {
  return /^[!-~]{1,256}$/.test(uid) && !uid.startsWith("acct-") && uid !== "local-account";
}

function officialUid(account: AccountConfig) {
  const uid = (account.workbuddy_uid ?? account.uid).trim();
  return validUid(uid) ? uid : "";
}

function accessToken(value: string) {
  return value.trim().replace(/^Bearer(?:\s+|$)/i, "");
}

function lifetime(a: AccountConfig) {
  if (!a.token) return "未配置";
  let exp = a.expires_at;
  if (!exp) { try { exp = JSON.parse(atob(a.token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/"))).exp ?? null; } catch { /* Opaque token. */ } }
  if (!exp) return "有效期未知";
  const left = (exp < 1e12 ? exp * 1000 : exp) - Date.now();
  return left <= 0 ? "已过期" : `${(left / 86_400_000).toFixed(1)} 天`;
}
function AccountIp({ account }: { account: AccountConfig }) {
  const observed = account.import_ip;
  const title = account.login_ip
    ? "来自官方账号登录数据"
    : observed
      ? `本机导入时的公网出口，由 ipify 查询，采集于 ${new Date(observed.observed_at).toLocaleString("zh-CN")}。远程授权者或使用不同代理的浏览器可能具有不同 IP。`
      : "官方未返回登录 IP，且未取得导入出口 IP。重新授权或导入时会再次尝试。";
  return <div className="stat-tile wb-login-ip" title={title}>
    <span>{account.login_ip ? "登录 IP" : observed ? "导入出口 IP" : "登录 IP"}</span>
    <strong>{account.login_ip || observed?.ip || "未获取"}</strong>
    {observed && !account.login_ip && <small>本机出口 · 非远程登录者 IP</small>}
  </div>;
}

export default function VaultPanel({ pool, poolError, onOpenGrowth }: { pool: AccountStatusView[]; poolError: string | null; onOpenGrowth: () => void }) {
  const { config, save, setNotice, balances, balanceError, balanceRefreshing, refreshAccountBalances } = useApp();
  const poolByUid = useMemo(() => new Map(pool.map((account) => [account.uid, account])), [pool]);
  const [busy, setBusy] = useState<string | null>(null);
  const [showImport, setShowImport] = useState(false);
  const importTrigger = useRef<HTMLButtonElement>(null);
  const [importMode, setImportMode] = useState<"browser" | "manual">("browser");
  const [token, setToken] = useState("");
  const [refreshToken, setRefreshToken] = useState("");
  const [uidInput, setUidInput] = useState("");
  const [localAccount, setLocalAccount] = useState<AccountConfig | null>(null);
  const [nickname, setNickname] = useState("");
  const [realm, setRealm] = useState<"cn" | "global">("cn");
  const [search, setSearch] = useState("");
  const [rename, setRename] = useState<string | null>(null);
  const [newName, setNewName] = useState("");
  const [repairing, setRepairing] = useState<string | null>(null);
  const [repairUid, setRepairUid] = useState("");
  const [credentialAccount, setCredentialAccount] = useState<AccountConfig | null>(null);
  const [renewalToken, setRenewalToken] = useState("");
  const [deleting, setDeleting] = useState<string | null>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [taskAccount, setTaskAccount] = useState<string | null>(null);
  const { job, starting: taskStarting, synchronized: taskSynchronized, start: startTask } = useTasks();
  const taskBusy = taskStarting || job?.status === "running" || !taskSynchronized;
  const taskTarget = config.accounts.find((account) => account.uid === taskAccount);
  const confirmTask = async () => {
    if (!taskTarget || taskBusy || !taskTarget.enabled || taskTarget.paused) return;
    clearExecutionError();
    try {
      await startTask(taskTarget.uid);
      setTaskAccount(null);
      onOpenGrowth();
    } catch (error) {
      setTaskAccount(null);
      reportError(error);
    }
  };
  const run = async (key: string, action: () => Promise<void>, onError = reportError) => {
    setBusy(key); clearExecutionError();
    try { await action(); } catch (e) { onError(e); } finally { setBusy(null); }
  };
  const importFrom = (source: "local" | "logs") => run(source, async () => {
    if (source === "local") {
      const account = await readCurrentWorkbuddyAccount();
      if (!account) throw new Error("未找到 WorkBuddy 登录态，请先在官方客户端登录。");
      setLocalAccount(account);
      setToken(account.token);
      setRefreshToken(account.refresh_token);
      setUidInput(officialUid(account));
      setNickname(account.nickname);
      setRealm(account.realm);
      setNotice("已读取本机账号身份与凭证，请核对后导入");
    } else {
      const found = await extractTokenFromLogs();
      if (!found) throw new Error("未找到可导入的 Token，请粘贴登录信息。");
      setLocalAccount(null);
      setToken(found);
      setRefreshToken("");
      setUidInput("");
      setNotice("已读取 Token，请填写其对应的真实 WorkBuddy UID");
    }
    setImportMode("manual");
    setShowImport(true);
  });
  const changeToken = (value: string) => {
    setToken(value);
    if (localAccount && accessToken(value) !== localAccount.token) {
      setLocalAccount(null);
      setUidInput("");
      setRefreshToken("");
    }
  };
  const add = () => run("import", async () => {
    const value = accessToken(token);
    const uid = uidInput.trim();
    if (!validUid(uid) || !value) throw new Error("请填写真实 WorkBuddy UID 和 Access Token。");
    const importedLocal = localAccount && accessToken(localAccount.token) === value
      && officialUid(localAccount) === uid && localAccount.realm === realm ? localAccount : null;
    const importIp = importedLocal?.login_ip ? null : await observeImportIp().catch(() => null);
    // The IP lookup can take a few seconds; merge into the latest account list.
    const current = useApp.getState().config;
    const existing = current.accounts.find((a) => a.token.trim() === value);
    if (existing && officialUid(existing)) throw new Error("这个 Token 已存在，请勿重复导入。");
    if (current.accounts.some((a) => a.uid !== existing?.uid && (a.uid === uid || officialUid(a) === uid))) {
      throw new Error("这个 WorkBuddy UID 已存在，请勿重复导入。");
    }
    const account: AccountConfig = {
      uid: existing?.uid ?? uid,
      workbuddy_uid: uid,
      nickname: existing?.nickname ?? (nickname.trim() || `账号 ${current.accounts.length + 1}`),
      token: value,
      refresh_token: refreshToken.trim() || existing?.refresh_token || "",
      email: importedLocal?.email ?? existing?.email ?? "",
      enterprise_id: importedLocal?.enterprise_id ?? existing?.enterprise_id ?? "",
      enterprise_name: importedLocal?.enterprise_name ?? existing?.enterprise_name ?? "",
      domain: importedLocal?.domain ?? existing?.domain ?? "",
      expires_at: importedLocal?.expires_at ?? existing?.expires_at ?? null,
      login_ip: importedLocal?.login_ip || existing?.login_ip || null,
      import_ip: importIp || existing?.import_ip || null,
      realm,
      enabled: existing?.enabled ?? true,
      paused: existing?.paused ?? false,
    };
    const accounts = existing ? current.accounts.map((a) => a.uid === existing.uid ? account : a) : [...current.accounts, account];
    await save({ ...current, accounts }, existing);
    setToken(""); setRefreshToken(""); setUidInput(""); setLocalAccount(null); setNickname(""); setShowImport(false);
    if (existing) setNotice("已补全已有账号的真实 UID，历史用量保留");
    void refreshAccountBalances();
  });
  const saveUid = (account: AccountConfig) => run(`uid-${account.uid}`, async () => {
    const uid = repairUid.trim();
    if (!validUid(uid)) throw new Error("请填写真实 WorkBuddy UID。");
    const current = useApp.getState().config;
    if (current.accounts.some((a) => a.uid !== account.uid && (a.uid === uid || officialUid(a) === uid))) {
      throw new Error("这个 WorkBuddy UID 已被其他账号使用。");
    }
    await save({ ...current, accounts: current.accounts.map((a) => a.uid === account.uid ? { ...a, workbuddy_uid: uid } : a) });
    setRepairing(null);
    setNotice("已补录真实 WorkBuddy UID，历史用量保留");
    void refreshAccountBalances();
  });
  const mutate = (uid: string, update: Partial<AccountConfig>) => run(`edit-${uid}`, async () => {
    const current = useApp.getState().config;
    await save({ ...current, accounts: current.accounts.map((a) => a.uid === uid ? { ...a, ...update } : a) });
    setRename(null);
  });
  const accountToDelete = config.accounts.find((account) => account.uid === deleting);
  const deletePending = busy === "delete";
  const confirmDelete = () => {
    if (!accountToDelete || busy) return;
    const { uid } = accountToDelete;
    setDeleteError(null);
    void run("delete", async () => {
      const current = useApp.getState().config;
      await save({ ...current, accounts: current.accounts.filter((account) => account.uid !== uid) });
      setDeleting(null);
      setNotice("已从保险柜移除账号");
    }, (error) => {
      clearExecutionError();
      setDeleteError(error instanceof Error ? error.message : String(error));
    });
  };
  const saveRenewalToken = () => {
    if (!credentialAccount) return;
    const snapshot = credentialAccount;
    void run(`credentials-${snapshot.uid}`, async () => {
      const value = renewalToken.trim();
      if (!value) throw new Error("请填写这个账号对应的 Refresh Token。");
      const current = useApp.getState().config;
      if (!current.accounts.some((account) => account.uid === snapshot.uid)) throw new Error("账号已被移除。");
      await save({ ...current, accounts: current.accounts.map((account) => account.uid === snapshot.uid
        ? { ...account, refresh_token: value } : account) }, snapshot);
      setCredentialAccount(null);
      setRenewalToken("");
      setNotice("续期凭证已保存，后台会按有效期自动续期");
      await useApp.getState().syncKeepaliveStatus();
    });
  };
  const refresh = () => run("balance", async () => {
    await refreshAccountBalances();
    const { balances: results, balanceError: refreshError } = useApp.getState();
    if (refreshError && !results.length) throw new Error(refreshError);
    const failed = results.filter((r) => r.error);
    if (failed.length) throw new Error(failed.map((r) => `${r.nickname}: ${r.error}`).join("；"));
    setNotice(`已刷新 ${results.length - failed.length}/${results.length} 个账号积分`);
  });
  const act = (a: AccountConfig, action: "checkin" | "keepalive" | "switch") => run(`${action}-${a.uid}`, async () => {
    let message: string;
    if (action === "switch") {
      const r = await switchAccount(a.uid);
      if (!r.ok) throw new Error(r.message);
      message = r.message;
    } else if (action === "keepalive") {
      const r = await refreshWorkbuddyToken(a.uid);
      if (!r.ok) throw new Error(r.message);
      useApp.setState({ config: await getConfig() });
      message = r.message;
    } else {
      const r = await checkinOne(a.uid);
      if (r.error) throw new Error(r.error);
      message = `${a.nickname} · ${checkinMessage(r.checkin)}`;
    }
    if (action === "checkin") {
      await refreshAccountBalances();
      const failure = useApp.getState().balanceError;
      if (failure) throw new Error(`操作已执行，但刷新积分失败：${failure}`);
    }
    window.dispatchEvent(new Event("wb-pool-updated"));
    setNotice(message);
  });
  const shown = config.accounts.filter((a) => `${a.nickname} ${a.uid} ${officialUid(a)} ${a.email}`.toLowerCase().includes(search.toLowerCase()));
  const openImport = (event: MouseEvent<HTMLButtonElement>) => {
    importTrigger.current = event.currentTarget;
    setImportMode("browser");
    setShowImport(true);
  };
  const openLocalImport = (event: MouseEvent<HTMLButtonElement>) => {
    importTrigger.current = event.currentTarget;
    void importFrom("local");
  };
  const importPending = busy === "import" || busy === "logs";
  const closeImport = () => {
    if (!importPending) setShowImport(false);
  };
  const browserLoginCompleted = async (result: BrowserLoginPoll, isCurrent: () => boolean) => {
    const saved = await getConfig();
    if (!isCurrent()) return;
    useApp.setState({ config: saved });
    window.dispatchEvent(new Event("wb-pool-updated"));
    setNotice(`${result.nickname || "账号"} · 浏览器授权完成，已加入保险柜`);
    setShowImport(false);
    void refreshAccountBalances();
  };
  return <div className="wb-panel vault-panel">
    <div className="wb-page-heading"><div><h1 className="wb-page-title">你的账号，井井有条</h1><p className="wb-page-description">集中管理积分、登录状态与成长任务，随时一键切换。</p></div><div className="wb-page-actions"><button className="btn btn-outline btn-sm" disabled={!!busy} onClick={openLocalImport}><AppIcon name="vault" size={18} />本机导入</button><button className="btn btn-primary btn-sm" disabled={!!busy} onClick={openImport}>＋ 添加账号</button></div></div>
    {(balanceError || poolError) && <p className="wb-muted text-xs mb-4" role="status" title={balanceError || poolError || undefined}>{balanceError ? "部分账号积分暂未同步，可点击刷新积分重试。" : "账号状态暂未同步，正在保留上次结果。"}</p>}
    <div className="wb-toolbar"><div className="flex flex-wrap items-center gap-3"><span className="font-semibold">全部账号 <span className="wb-count">{config.accounts.length}</span></span><span className="wb-muted text-xs">{config.accounts.filter((a) => a.enabled && !a.paused).length} 个参与轮转</span></div><div className="flex flex-wrap gap-2"><input className="input input-bordered input-sm wb-search" aria-label="搜索账号" placeholder="搜索昵称 / UID" value={search} onChange={(e) => setSearch(e.target.value)} /><button className="btn btn-outline btn-sm" disabled={!!busy || balanceRefreshing} onClick={() => void refresh()}><AppIcon name="refresh" size={17} />刷新积分</button></div></div>
    {showImport && <AccountDialog titleId="account-import-title" onDismiss={closeImport} returnFocusTo={importTrigger.current}><section className="acct-card wb-form-card">
      <div className="wb-section-heading"><div className="flex items-center gap-3"><AppIcon name="vault" size={34} /><div><h2 id="account-import-title">添加账号</h2><p className="wb-muted text-xs">选择浏览器授权，或导入已有登录凭证。</p></div></div><button className="btn btn-ghost btn-sm" aria-label="关闭导入" disabled={importPending} onClick={closeImport}>×</button></div>
      <div className="wb-segment-control wb-import-tabs" role="tablist" aria-label="添加账号方式">
        <button type="button" role="tab" id="browser-login-tab" aria-controls="browser-login-panel" aria-selected={importMode === "browser"} className={importMode === "browser" ? "active" : ""} disabled={importPending} onClick={() => setImportMode("browser")}>浏览器登录</button>
        <button type="button" role="tab" id="manual-import-tab" aria-controls="manual-import-panel" aria-selected={importMode === "manual"} className={importMode === "manual" ? "active" : ""} onClick={() => setImportMode("manual")}>手动导入</button>
      </div>
      {importMode === "browser" ? <div role="tabpanel" id="browser-login-panel" aria-labelledby="browser-login-tab"><BrowserLoginPanel onCompleted={browserLoginCompleted} /></div> : <div role="tabpanel" id="manual-import-panel" aria-labelledby="manual-import-tab"><div className="wb-form-grid">
        <label className="wb-field"><span>账号昵称</span><input className="input input-bordered input-sm" value={nickname} placeholder="例如：日常开发账号" onChange={(e) => setNickname(e.target.value)} /></label>
        <label className="wb-field"><span>账号区域</span><select className="select select-bordered select-sm" value={realm} onChange={(e) => setRealm(e.target.value as "cn" | "global")}><option value="cn">中国大陆</option><option value="global">国际区域</option></select></label>
        <label className="wb-field sm:col-span-2"><span>WorkBuddy UID <small>本机导入会自动读取</small></span><input className="input input-bordered input-sm font-mono" autoComplete="off" value={uidInput} readOnly={!!localAccount} placeholder="填写与 Access Token 对应的真实账号 UID" onChange={(e) => setUidInput(e.target.value)} /></label>
        <label className="wb-field sm:col-span-2"><span>Access Token</span><input className="input input-bordered input-sm font-mono" type="password" autoComplete="off" value={token} placeholder="粘贴 Token 或 Bearer Token" onChange={(e) => changeToken(e.target.value)} /></label>
        <label className="wb-field sm:col-span-2"><span>Refresh Token <small>可选，用于续期</small></span><input className="input input-bordered input-sm font-mono" type="password" autoComplete="off" value={refreshToken} onChange={(e) => setRefreshToken(e.target.value)} placeholder="粘贴 Refresh Token" /></label>
      </div>
      <div className="mt-5 flex flex-wrap gap-2"><button className="btn btn-primary btn-sm" disabled={!!busy || !accessToken(token) || !validUid(uidInput.trim())} onClick={() => void add()}>{busy === "import" ? "正在导入…" : "确认导入"}</button><button className="btn btn-outline btn-sm" disabled={!!busy} onClick={() => void importFrom("logs")}>从日志提取 Token</button><button className="btn btn-ghost btn-sm" disabled={importPending} onClick={closeImport}>取消</button></div>
      </div>}
    </section></AccountDialog>}
    {taskTarget && <AccountDialog compact titleId="account-task-title" descriptionId="account-task-description"
      onDismiss={() => { if (!taskStarting) setTaskAccount(null); }}>
      <section className="acct-card wb-form-card">
        <div className="wb-section-heading"><h2 id="account-task-title">执行成长任务</h2></div>
        <p className="wb-delete-description" id="account-task-description">是否为「<strong>{taskTarget.nickname}</strong>」执行任务并领取已达标奖励？</p>
        <p className="wb-muted text-sm">自动忽略指定的 6 项任务。确认后前往成长任务查看执行进度和日志，切换页面不会中断作业。</p>
        <p className="wb-muted text-xs mt-3">可自动执行的对话会使用该账号的模型额度；其余需要手动完成的任务会在日志中说明。</p>
        <div className="wb-login-footer wb-delete-actions">
          <button type="button" className="btn btn-outline btn-sm" data-dialog-initial-focus disabled={taskStarting} onClick={() => setTaskAccount(null)}>取消</button>
          <button type="button" className="btn btn-primary btn-sm" disabled={taskBusy || !taskTarget.enabled || taskTarget.paused} onClick={() => void confirmTask()}>{taskStarting ? "正在启动…" : "确认执行"}</button>
        </div>
      </section>
    </AccountDialog>}
    {accountToDelete && <AccountDialog
      compact titleId="account-delete-title" descriptionId="account-delete-description"
      onDismiss={() => { if (!deletePending) setDeleting(null); }}
    >
      <section className="acct-card wb-form-card">
        <div className="wb-section-heading"><h2 id="account-delete-title">删除账号</h2></div>
        <p className="wb-delete-description" id="account-delete-description">
          确定从保险柜移除「<strong>{accountToDelete.nickname}</strong>」？
        </p>
        {deleteError && <p className="wb-login-action-error" role="alert">{deleteError}</p>}
        <div className="wb-login-footer wb-delete-actions">
          <button type="button" className="btn btn-outline btn-sm" data-dialog-initial-focus
            disabled={deletePending} onClick={() => setDeleting(null)}>取消</button>
          <button type="button" className="btn btn-error btn-sm" disabled={!!busy} onClick={confirmDelete}>
            {deletePending ? "正在删除…" : "确认删除"}
          </button>
        </div>
      </section>
    </AccountDialog>}
    <div className="wb-account-grid">{!shown.length && <div className="acct-card wb-empty col-span-full"><AppIcon name="vault" size={64} /><h3>{config.accounts.length ? "没有找到匹配的账号" : "从第一个账号开始"}</h3><p>{config.accounts.length ? "试试其他昵称或 UID。" : "导入 WorkBuddy 登录信息，集中管理积分、签到和网关轮转。"}</p>{!config.accounts.length && <button className="btn btn-primary btn-sm" disabled={!!busy} onClick={openImport}>＋ 添加账号</button>}</div>}
      {shown.map((a) => { const i = config.accounts.findIndex((v) => v.uid === a.uid); const st = poolByUid.get(a.uid); const checkedIn = !!st?.checkin_done || hasCheckedInToday(a.uid); const balance = balances.find((v) => v.uid === a.uid); const remaining = balance ? !balance.error ? balance.remaining_credits ?? balance.credits ?? null : null : st?.remaining_credits ?? st?.credits ?? null; const unlimited = balance ? !balance.error && !!balance.unlimited : !!st?.unlimited; const uid = officialUid(a); return <article key={a.uid} className={`acct-card wb-account-card ${!a.enabled || a.paused ? "wb-account-paused" : ""}`}>
        <div className="flex items-start gap-3"><div className="wb-account-avatar">{i + 1}</div><div className="min-w-0 flex-1"><h2 className="truncate font-bold">{a.nickname}</h2><p className="mt-1 truncate text-xs wb-muted" title={uid || undefined}>{a.email || uid || "待补录 WorkBuddy UID"}</p><p className="mt-1 text-xs wb-muted">{a.realm === "cn" ? "中国大陆" : "国际区域"} · {a.realm !== "cn" ? "暂不支持签到" : checkedIn ? "今日已签到" : st ? "今日待签到" : "签到状态同步中"}</p></div><input className="checkbox checkbox-primary checkbox-sm" type="checkbox" title="参与网关轮转" aria-label={`${a.nickname}参与网关轮转`} checked={a.enabled && !a.paused} disabled={!!busy} onChange={(e) => void mutate(a.uid, { enabled: e.target.checked, paused: false })} /></div>
        {(!a.enabled || a.paused || st?.cooling || !a.token) && <div className="wb-account-health" role="status"><span className="wb-status-dot is-warning" />{!a.enabled || a.paused ? "账号已暂停" : st?.cooling ? `冷却中 · ${Math.ceil(st.cool_remaining_secs / 60)} 分钟` : "缺少 Token"}</div>}
        <div className="wb-account-stats"><div className="stat-tile"><span>剩余积分</span><strong className={!unlimited && remaining !== null && remaining < config.pool.credit_floor ? "text-warning" : "text-primary"}>{unlimited ? "不限量" : remaining !== null ? remaining.toLocaleString("zh-CN", { maximumFractionDigits: 8 }) : balance?.error ? "获取失败" : "待刷新"}</strong></div><div className="stat-tile"><span>Token 有效期</span><strong>{lifetime(a)}</strong></div><div className="stat-tile"><span>累计成功 / 失败</span><strong>{st ? `${st.success_count} / ${st.err_total}` : "—"}</strong></div><AccountIp account={a} /></div>
        {(balance?.error || st?.reason) && <p className="wb-account-reason">{balance?.error || st?.reason}</p>}
        <div className="wb-account-actions"><button className="btn btn-sm btn-outline" disabled={!!busy || balanceRefreshing} onClick={() => void refresh()}>积分</button><button className="btn btn-sm btn-outline" disabled={!!busy || a.realm !== "cn" || checkedIn} onClick={() => void act(a, "checkin")}>{checkedIn ? "已签到" : "签到"}</button><button className="btn btn-sm btn-outline" disabled={!!busy || !a.refresh_token} title={a.refresh_token ? "续期 Token" : "导入 Refresh Token 后可续期"} onClick={() => void act(a, "keepalive")}>保活</button><button className="btn btn-sm wb-btn-tint" disabled={!!busy || taskBusy || a.realm !== "cn" || !a.enabled || a.paused || !a.token.trim()} onClick={() => setTaskAccount(a.uid)}><AppIcon name="growth" size={16} />任务</button></div>
        <div className="mt-2 flex flex-wrap items-center gap-2">
          <button className="btn btn-sm btn-ghost" onClick={() => { setRename(a.uid); setNewName(a.nickname); }}>改名</button>
          <button className="btn btn-sm btn-ghost" disabled={!!busy} onClick={() => { setCredentialAccount(a); setRenewalToken(""); }}>{a.refresh_token ? "更新续期凭证" : "补充续期凭证"}</button>
          <button className="btn btn-sm btn-ghost" disabled={!!busy} onClick={() => void run(`share-${a.uid}`, async () => { await navigator.clipboard.writeText(`WorkBuddy 账号：${a.nickname}\nUID：${uid || "未补录"}\n区域：${a.realm}`); setNotice("已复制账号摘要"); })}>分享</button>
          <button className="btn btn-sm btn-ghost text-error" disabled={!!busy} onClick={() => { setDeleteError(null); setDeleting(a.uid); }}>删除</button>
          {uid ? <button className="btn btn-primary btn-sm ml-auto" disabled={!!busy} onClick={() => void act(a, "switch")}>{busy === `switch-${a.uid}` ? "切换中…" : "切换账号"}</button>
            : <button className="btn btn-primary btn-sm ml-auto" disabled={!!busy} onClick={() => { setRepairing(a.uid); setRepairUid(""); }}>补录 UID</button>}
        </div>
        {credentialAccount?.uid === a.uid && <form className="wb-inline-edit" onSubmit={(event) => { event.preventDefault(); saveRenewalToken(); }}>
          <label className="wb-field min-w-0 flex-1"><span>Refresh Token</span><input type="password" autoComplete="off" className="input input-bordered input-sm" value={renewalToken} onChange={(event) => setRenewalToken(event.target.value)} placeholder="填写该账号对应的续期凭证" /></label>
          <button className="btn btn-primary btn-sm" type="submit" disabled={!!busy || !renewalToken.trim()}>保存续期凭证</button>
          <button className="btn btn-ghost btn-sm" type="button" onClick={() => { setCredentialAccount(null); setRenewalToken(""); }}>取消</button>
        </form>}
        {repairing === a.uid && <form className="wb-inline-edit" onSubmit={(e) => { e.preventDefault(); void saveUid(a); }}>
          <label className="wb-field min-w-0 flex-1"><span>真实 WorkBuddy UID</span><input className="input input-bordered input-sm" autoComplete="off" value={repairUid} onChange={(e) => setRepairUid(e.target.value)} /></label>
          <button type="submit" className="btn btn-primary btn-sm" disabled={!validUid(repairUid.trim()) || !!busy}>保存 UID</button>
          <button type="button" className="btn btn-ghost btn-sm" onClick={() => setRepairing(null)}>取消</button>
          <p className="w-full text-xs wb-muted">填写后可切换账号，原有用量记录会保留。</p>
        </form>}
        {rename === a.uid && <div className="wb-inline-edit"><input aria-label="新昵称" className="input input-bordered input-sm min-w-0 flex-1" value={newName} onChange={(e) => setNewName(e.target.value)} /><button className="btn btn-primary btn-sm" disabled={!newName.trim() || !!busy} onClick={() => void mutate(a.uid, { nickname: newName.trim() })}>保存</button><button className="btn btn-ghost btn-sm" onClick={() => setRename(null)}>取消</button></div>}

      </article>; })}
    </div>
  </div>;
}

