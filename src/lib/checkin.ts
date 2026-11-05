import type { TaskRunItem } from "../types";

export function checkinDate(now = Date.now()) {
  return new Date(now + 8 * 3600000).toISOString().slice(0, 10);
}

export function hasCheckedInToday(uid: string) {
  try {
    return localStorage.getItem(`wb-checkin-date:${uid}`) === checkinDate();
  } catch {
    return false;
  }
}

export function rememberCheckin(uid: string) {
  try {
    localStorage.setItem(`wb-checkin-date:${uid}`, checkinDate());
  } catch {
    // A storage restriction must not turn a confirmed check-in into a failure.
  }
}

export function recordCheckinResult(result: TaskRunItem) {
  if (result.checkin === "ok" || result.checkin === "already") rememberCheckin(result.uid);
}

export function checkinMessage(result: string) {
  return result === "already" ? "今日已签到" : result === "ok" ? "签到成功" : result || "签到完成";
}
