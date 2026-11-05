import { useEffect, useRef, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { cancelBrowserLogin, pollBrowserLogin, startBrowserLogin } from "../lib/commands";
import type { BrowserLoginPoll, BrowserLoginStart } from "../types";

type Phase = "idle" | "starting" | "waiting" | "completed" | "expired" | "error";

function errorText(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

export default function BrowserLoginPanel({ onCompleted }: {
  onCompleted: (result: BrowserLoginPoll, isCurrent: () => boolean) => Promise<void>;
}) {
  const [realm, setRealm] = useState<"cn" | "global">("cn");
  const [phase, setPhase] = useState<Phase>("idle");
  const [login, setLogin] = useState<BrowserLoginStart | null>(null);
  const [message, setMessage] = useState("");
  const [actionError, setActionError] = useState("");
  const [copied, setCopied] = useState(false);
  const [opening, setOpening] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const mounted = useRef(false);
  const generation = useRef(0);
  const sessionId = useRef<string | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const completed = useRef<BrowserLoginPoll | null>(null);
  const completionHandler = useRef(onCompleted);
  completionHandler.current = onCompleted;

  const stopLocal = () => {
    generation.current += 1;
    if (timer.current) clearTimeout(timer.current);
    timer.current = null;
    const previous = sessionId.current;
    sessionId.current = null;
    return previous;
  };

  const resetActions = () => {
    setOpening(false);
    setSyncing(false);
    setCopied(false);
    setActionError("");
  };

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      const previous = stopLocal();
      if (previous) void cancelBrowserLogin(previous).catch(() => {
        // The view is gone; the backend also expires abandoned sessions.
      });
    };
  }, []);

  const synchronize = async (result: BrowserLoginPoll, epoch: number) => {
    if (!mounted.current || generation.current !== epoch) return;
    setSyncing(true);
    try {
      await completionHandler.current(result, () => mounted.current && generation.current === epoch);
    } catch (error) {
      if (mounted.current && generation.current === epoch) {
        setActionError(`账号已登录，但列表刷新失败：${errorText(error)}`);
      }
    } finally {
      if (mounted.current && generation.current === epoch) setSyncing(false);
    }
  };

  const schedulePoll = (session: BrowserLoginStart, epoch: number) => {
    if (!mounted.current || generation.current !== epoch) return;
    timer.current = setTimeout(() => void poll(session, epoch), Math.max(3000, session.poll_interval_ms));
  };

  const poll = async (session: BrowserLoginStart, epoch: number) => {
    if (!mounted.current || generation.current !== epoch) return;
    timer.current = null;
    if (Date.now() >= session.expires_at) {
      sessionId.current = null;
      setPhase("expired");
      setMessage("授权链接已过期，请重新获取。");
      void cancelBrowserLogin(session.session_id).catch(() => {});
      return;
    }
    try {
      const result = await pollBrowserLogin(session.session_id);
      if (!mounted.current || generation.current !== epoch) return;
      if (result.status === "completed") {
        sessionId.current = null;
        completed.current = result;
        setPhase("completed");
        setMessage(result.message || "浏览器授权完成，账号已加入保险柜。");
        await synchronize(result, epoch);
      } else if (result.status === "expired") {
        sessionId.current = null;
        setPhase("expired");
        setMessage(result.message || "授权链接已过期，请重新获取。");
      } else {
        setMessage(result.message || "等待浏览器完成登录与授权…");
        schedulePoll(session, epoch);
      }
    } catch (error) {
      if (!mounted.current || generation.current !== epoch) return;
      setPhase("error");
      setMessage(errorText(error));
      sessionId.current = null;
      void cancelBrowserLogin(session.session_id).catch(() => {});
    }
  };

  const begin = async () => {
    const previous = stopLocal();
    const epoch = generation.current;
    setPhase("starting");
    setLogin(null);
    setMessage("");
    resetActions();
    completed.current = null;
    try {
      if (previous) await cancelBrowserLogin(previous);
      if (!mounted.current || generation.current !== epoch) return;
      const result = await startBrowserLogin(realm);
      if (!mounted.current || generation.current !== epoch) {
        await cancelBrowserLogin(result.session_id).catch(() => {});
        return;
      }
      sessionId.current = result.session_id;
      setLogin(result);
      setPhase("waiting");
      setMessage("在浏览器中完成 WorkBuddy 登录与授权，账号会自动加入保险柜。");
      schedulePoll(result, epoch);
    } catch (error) {
      if (!mounted.current || generation.current !== epoch) return;
      setPhase("error");
      setMessage(errorText(error));
    }
  };

  const changeRealm = (value: "cn" | "global") => {
    if (value === realm) return;
    const previous = stopLocal();
    const epoch = generation.current;
    setRealm(value);
    setPhase("idle");
    setLogin(null);
    setMessage("");
    resetActions();
    completed.current = null;
    if (previous) void cancelBrowserLogin(previous).catch((error) => {
      if (mounted.current && generation.current === epoch) setActionError(`取消上次授权失败：${errorText(error)}`);
    });
  };

  const cancel = async () => {
    const previous = stopLocal();
    const epoch = generation.current;
    setPhase("idle");
    setLogin(null);
    setMessage("已取消本次授权。");
    resetActions();
    if (previous) {
      try { await cancelBrowserLogin(previous); }
      catch (error) {
        if (mounted.current && generation.current === epoch) {
          setActionError(`取消授权失败：${errorText(error)}`);
        }
      }
    }
  };

  const open = async () => {
    if (!login || phase !== "waiting") return;
    const epoch = generation.current;
    setOpening(true);
    setActionError("");
    try { await openUrl(login.url); }
    catch (error) {
      if (mounted.current && generation.current === epoch) {
        setActionError(`无法打开浏览器，可复制授权链接手动打开：${errorText(error)}`);
      }
    } finally {
      if (mounted.current && generation.current === epoch) setOpening(false);
    }
  };

  const copy = async () => {
    if (!login || phase !== "waiting") return;
    const epoch = generation.current;
    try {
      await navigator.clipboard.writeText(login.url);
      if (mounted.current && generation.current === epoch) {
        setCopied(true);
        setActionError("");
      }
    } catch (error) {
      if (mounted.current && generation.current === epoch) setActionError(`复制失败：${errorText(error)}`);
    }
  };

  return <div className="wb-browser-login">
    <fieldset className="wb-login-realms">
      <legend>账号区域</legend>
      <label className={realm === "cn" ? "is-selected" : ""}>
        <input className="radio radio-primary radio-sm" type="radio" name="browser-login-realm" value="cn" checked={realm === "cn"} onChange={() => changeRealm("cn")} />
        <span>国内版 <small>CN</small></span>
      </label>
      <label className={realm === "global" ? "is-selected" : ""}>
        <input className="radio radio-primary radio-sm" type="radio" name="browser-login-realm" value="global" checked={realm === "global"} onChange={() => changeRealm("global")} />
        <span>国际版 <small>Global</small></span>
      </label>
    </fieldset>
    <p className="wb-login-description">在 WorkBuddy 官方页面完成登录，授权后自动保存账号与续期凭证。</p>
    <p className="wb-login-description">优先读取官方登录 IP；未返回时通过 ipify 记录本机导入出口 IP。将链接发给他人登录时，本机出口无法代表对方 IP。</p>
    {phase === "waiting" && login && <div className="wb-login-link-box">
      <label className="wb-field"><span>授权链接</span><input className="input input-bordered input-sm font-mono" readOnly value={login.url} aria-label="浏览器授权链接" /></label>
      <div className="wb-login-link-actions"><button className="btn btn-primary btn-sm" disabled={opening} onClick={() => void open()}>{opening ? "正在打开…" : "打开浏览器"}</button><button className="btn btn-outline btn-sm" onClick={() => void copy()}>{copied ? "已复制" : "复制链接"}</button></div>
      <p className="wb-muted text-xs">有效期至 {new Date(login.expires_at).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false })} · 每 3 秒检查授权结果</p>
    </div>}
    {(message || phase === "starting") && <div className={`wb-login-status${phase === "error" || phase === "expired" ? " is-error" : ""}`} role={phase === "error" ? "alert" : "status"}>
      {(phase === "waiting" || phase === "starting") && <span className="loading loading-spinner loading-xs" aria-hidden="true" />}
      <span>{phase === "starting" ? "正在获取授权链接…" : message}</span>
    </div>}
    {actionError && <p className="wb-login-action-error" role="alert">{actionError}</p>}
    <div className="wb-login-footer">
      {phase === "waiting" || phase === "starting"
        ? <button className="btn btn-outline btn-sm" onClick={() => void cancel()}>取消授权</button>
        : phase === "completed"
          ? <button className="btn btn-primary btn-sm" disabled={syncing} onClick={() => completed.current && void synchronize(completed.current, generation.current)}>{syncing ? "正在刷新账号…" : "刷新账号列表"}</button>
          : <button className="btn btn-primary btn-sm" onClick={() => void begin()}>{phase === "expired" || phase === "error" ? "重新获取授权链接" : "获取授权链接"}</button>}
    </div>
  </div>;
}
