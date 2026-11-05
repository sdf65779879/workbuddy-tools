import { useEffect } from "react";
import { useApp } from "../stores/appStore";
import { checkinOne, poolStatus } from "./commands";
import { hasCheckedInToday } from "./checkin";
import { reportError } from "./errors";

/** One app-level runner: continues working when users move between panels. */
export function useAutomation(enabled: boolean) {
  useEffect(() => {
    if (!enabled) return;
    let active = true;
    let working = false;
    let lastBalance = Date.now();
    let lastCheckinAttempt = 0;
    const run = async () => {
      if (!active || working) return;
      working = true;
      try {
        const { config } = useApp.getState();
        const accounts = config.accounts.filter((account) => account.enabled && !account.paused && account.token.trim());
        const now = Date.now();
        const shanghai = new Date(now + 8 * 3600000);
        const configuredHours = config.schedule.checkin_hours.filter((hour) => hour >= 0 && hour <= 23);
        const hours = configuredHours.length ? configuredHours : [0];
        if (config.schedule.checkin_enabled && hours.some((hour) => shanghai.getUTCHours() >= hour) && now - lastCheckinAttempt >= 600000) {
          lastCheckinAttempt = now;
          const states = await poolStatus();
          const failures: string[] = [];
          let completed = 0;
          for (const account of accounts.filter((item) => item.realm === "cn")) {
            if (!active) break;
            if (hasCheckedInToday(account.uid) || states.find((state) => state.uid === account.uid)?.checkin_done) continue;
            try {
              const result = await checkinOne(account.uid);
              if (result.error) throw new Error(result.error);
              completed++;
            } catch (error) { failures.push(`${account.nickname}：${String(error)}`); }
          }
          window.dispatchEvent(new Event("wb-pool-updated"));
          if (active && failures.length) reportError(`自动签到失败：\n${failures.join("\n")}`);
          else if (active && completed) useApp.getState().setNotice(`${completed} 个账号 · 自动签到完成`);
        }
        const interval = config.schedule.balance_refresh_minutes * 60000;
        if (config.accounts.length > 0 && interval > 0 && now - lastBalance >= interval) {
          lastBalance = now;
          await useApp.getState().refreshAccountBalances();
          const failure = useApp.getState().balanceError;
          if (active && failure) reportError(`自动刷新积分失败：${failure}`);
        }
      } catch (error) {
        if (active) reportError(`自动化执行失败：${String(error)}`);
      } finally { working = false; }
    };
    void run();
    const timer = window.setInterval(() => void run(), 30000);
    const update = () => void run();
    window.addEventListener("wb-automation-change", update);
    return () => { active = false; window.clearInterval(timer); window.removeEventListener("wb-automation-change", update); };
  }, [enabled]);
}

