import { create } from "zustand";
import { startTaskJob, taskJobStatus } from "../lib/commands";
import { checkinDate, recordCheckinResult } from "../lib/checkin";
import type { TaskJobSnapshot } from "../types";
import { useApp } from "./appStore";

interface TaskState {
  job: TaskJobSnapshot | null;
  starting: boolean;
  synchronized: boolean;
  syncError: string | null;
  sync: () => Promise<void>;
  start: (uid?: string) => Promise<void>;
}

let polling = false;
let revision = 0;
let finishedId: string | null = null;

function completeObservation(job: TaskJobSnapshot | null) {
  if (!job || job.status === "running" || finishedId === job.id) return;
  finishedId = job.id;
  // A job crossing midnight cannot prove which date its check-in belongs to.
  if (checkinDate(job.started_at) === checkinDate()) job.result.items.forEach(recordCheckinResult);
  window.dispatchEvent(new Event("wb-pool-updated"));
  window.dispatchEvent(new Event("wb-task-finished"));
  void useApp.getState().refreshAccountBalances();
}

export const useTasks = create<TaskState>((set, get) => ({
  job: null,
  starting: false,
  synchronized: false,
  syncError: null,
  sync: async () => {
    if (polling || get().starting) return;
    polling = true;
    const epoch = revision;
    try {
      const job = await taskJobStatus();
      if (epoch !== revision) return;
      set({ job, synchronized: true, syncError: null });
      completeObservation(job);
    } catch (error) {
      if (epoch === revision) set({ syncError: String(error), synchronized: false });
    } finally { polling = false; }
  },
  start: async (uid) => {
    if (get().starting || get().job?.status === "running") throw new Error("已有任务作业执行中，请在成长任务查看进度。");
    if (!get().synchronized) throw new Error("正在确认后台任务状态，请稍后重试。");
    revision++;
    set({ starting: true, syncError: null });
    try {
      const job = await startTaskJob(uid);
      set({ job, synchronized: true });
      completeObservation(job);
    } catch (error) {
      // The request may have reached the backend. Reconcile before another start.
      set({ synchronized: false });
      throw error;
    } finally { set({ starting: false }); }
  },
}));
