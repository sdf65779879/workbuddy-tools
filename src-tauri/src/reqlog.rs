//! 请求运行日志：内存最近 100 条 + 进程级计数 + JSONL 归档（按天、轮转、保留/体积清理）。
//! 只记录元数据，不写提示词 / 响应正文 / Authorization。

use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const OUTCOME_SUCCESS: &str = "success";
pub const OUTCOME_HTTP_ERROR: &str = "http_error";
pub const OUTCOME_STREAM_ERROR: &str = "stream_error";
#[allow(dead_code)]
pub const OUTCOME_INTERRUPTED: &str = "interrupted";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub time: i64,
    pub request_id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_uid: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    pub status: u16,
    pub ok: bool,
    pub outcome: String,
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttfb_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<i64>,
    #[serde(default)]
    pub prompt_tokens: i64,
    #[serde(default)]
    pub completion_tokens: i64,
    #[serde(default)]
    pub total_tokens: i64,
    #[serde(default)]
    pub credit: f64,
    #[serde(default)]
    pub credit_known: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_ip: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user_agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Filter {
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub account_uid: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub model_exact: String,
    #[serde(default)]
    pub client_ip: String,
    #[serde(default)]
    pub from_ms: i64,
    #[serde(default)]
    pub to_ms: i64,
}

impl Filter {
    fn matches(&self, e: &Event) -> bool {
        if !self.outcome.is_empty() && e.outcome != self.outcome {
            return false;
        }
        if !contains_fold(&e.account, &self.account) {
            return false;
        }
        if !self.account_uid.is_empty() && e.account_uid != self.account_uid {
            return false;
        }
        if !self.model_exact.is_empty() && e.model != self.model_exact {
            return false;
        }
        if !contains_fold(&e.model, &self.model) {
            return false;
        }
        if !contains_fold(&e.client_ip, &self.client_ip) {
            return false;
        }
        if self.from_ms > 0 && e.time < self.from_ms {
            return false;
        }
        if self.to_ms > 0 && e.time >= self.to_ms {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveStats {
    pub enabled: bool,
    pub dir: String,
    pub files: usize,
    pub bytes: u64,
    pub dropped_writes: u64,
    pub last_error: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub started_at_ms: i64,
    pub completed: i64,
    pub in_flight: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub success_rate: f64,
    pub http_success_rate: f64,
    pub avg_duration_ms: f64,
    pub recent: Vec<Event>,
    pub archive: ArchiveStats,
}

pub struct ArchiveConfig {
    pub enabled: bool,
    pub dir: PathBuf,
    pub retention_days: u32,
    pub max_bytes: u64,
}

struct Inner {
    started_at_ms: i64,
    in_flight: i64,
    completed: i64,
    succeeded: i64,
    http_success: i64,
    duration_sum: i64,
    recent: VecDeque<Event>,
}

pub struct Recorder {
    inner: Mutex<Inner>,
    archive: Arc<ArchiveState>,
}

struct ArchiveState {
    cfg: ArchiveConfig,
    tx: mpsc::Sender<Event>,
    dropped: AtomicU64,
    last_error: Mutex<String>,
    alive: AtomicBool,
}

impl Recorder {
    pub fn new(cfg: ArchiveConfig) -> Arc<Self> {
        let archive = spawn_archive(cfg);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                started_at_ms: now_ms(),
                in_flight: 0,
                completed: 0,
                succeeded: 0,
                http_success: 0,
                duration_sum: 0,
                recent: VecDeque::new(),
            }),
            archive,
        })
    }

    #[allow(dead_code)]
    pub fn begin(&self) {
        self.inner.lock().unwrap().in_flight += 1;
    }

    pub fn record(&self, mut e: Event) {
        if e.time == 0 {
            e.time = now_ms();
        }
        if e.outcome.is_empty() {
            e.outcome = if e.status == 200 && e.ok {
                OUTCOME_SUCCESS.to_string()
            } else {
                OUTCOME_HTTP_ERROR.to_string()
            };
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.in_flight > 0 {
                inner.in_flight -= 1;
            }
            inner.completed += 1;
            if e.ok {
                inner.succeeded += 1;
            }
            if (200..300).contains(&e.status) {
                inner.http_success += 1;
            }
            inner.duration_sum += e.duration_ms;
            inner.recent.push_front(e.clone());
            if inner.recent.len() > 100 {
                inner.recent.pop_back();
            }
        }
        self.archive.send(e);
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.inner.lock().unwrap();
        let mut s = Snapshot {
            started_at_ms: inner.started_at_ms,
            completed: inner.completed,
            in_flight: inner.in_flight,
            succeeded: inner.succeeded,
            failed: inner.completed - inner.succeeded,
            success_rate: 0.0,
            http_success_rate: 0.0,
            avg_duration_ms: 0.0,
            recent: inner.recent.iter().cloned().collect(),
            archive: self.archive.stats(),
        };
        if inner.completed > 0 {
            s.success_rate = round1(inner.succeeded as f64 / inner.completed as f64 * 100.0);
            s.http_success_rate =
                round1(inner.http_success as f64 / inner.completed as f64 * 100.0);
            s.avg_duration_ms = inner.duration_sum as f64 / inner.completed as f64;
        }
        s
    }

    pub fn read_archive(&self, limit: usize, filter: Filter) -> Vec<Event> {
        // Include live rows even when archiving is disabled or the writer has not flushed yet.
        let limit = if limit == 0 { 200 } else { limit.min(1000) };
        let mut events: Vec<Event> = self
            .inner
            .lock()
            .unwrap()
            .recent
            .iter()
            .filter(|event| filter.matches(event))
            .cloned()
            .collect();
        events.extend(self.archive.read(limit, filter));
        events.sort_by(|a, b| b.time.cmp(&a.time));
        let mut seen = HashSet::new();
        events.retain(|event| seen.insert(event.request_id.clone()));
        events.truncate(limit);
        events
    }
}

impl ArchiveState {
    fn send(&self, e: Event) {
        if !self.cfg.enabled || !self.alive.load(Ordering::SeqCst) {
            return;
        }
        match self.tx.send(e) {
            Ok(()) => {}
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    fn stats(&self) -> ArchiveStats {
        let mut s = ArchiveStats {
            enabled: self.cfg.enabled && self.alive.load(Ordering::SeqCst),
            dir: self.cfg.dir.display().to_string(),
            files: 0,
            bytes: 0,
            dropped_writes: self.dropped.load(Ordering::SeqCst),
            last_error: self.last_error.lock().unwrap().clone(),
        };
        if !s.enabled {
            return s;
        }
        if let Ok(entries) = fs::read_dir(&self.cfg.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("requests-") && name.ends_with(".jsonl") {
                    if let Ok(md) = entry.metadata() {
                        s.files += 1;
                        s.bytes += md.len();
                    }
                }
            }
        }
        s
    }

    fn read(&self, limit: usize, filter: Filter) -> Vec<Event> {
        if !self.cfg.enabled || !self.alive.load(Ordering::SeqCst) {
            return Vec::new();
        }
        let mut limit = if limit == 0 { 200 } else { limit };
        if limit > 1000 {
            limit = 1000;
        }
        let mut out: Vec<Event> = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.cfg.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !(name.starts_with("requests-") && name.ends_with(".jsonl")) {
                    continue;
                }
                let path = entry.path();
                if let Ok(text) = fs::read_to_string(path) {
                    for line in text.lines() {
                        if let Ok(e) = serde_json::from_str::<Event>(line) {
                            if filter.matches(&e) {
                                out.push(e);
                            }
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| b.time.cmp(&a.time));
        out.truncate(limit);
        out
    }
}

fn spawn_archive(cfg: ArchiveConfig) -> Arc<ArchiveState> {
    if !cfg.enabled {
        let st = Arc::new(ArchiveState {
            cfg,
            tx: mpsc::channel().0,
            dropped: AtomicU64::new(0),
            last_error: Mutex::new(String::new()),
            alive: AtomicBool::new(false),
        });
        return st;
    }
    let (tx, rx) = mpsc::channel::<Event>();
    let alive = AtomicBool::new(false);
    let st = Arc::new(ArchiveState {
        cfg,
        tx,
        dropped: AtomicU64::new(0),
        last_error: Mutex::new(String::new()),
        alive,
    });
    let st2 = st.clone();
    std::thread::spawn(move || {
        if let Err(e) = fs::create_dir_all(&st2.cfg.dir) {
            *st2.last_error.lock().unwrap() = e.to_string();
            return;
        }
        st2.alive.store(true, Ordering::SeqCst);
        let mut file: Option<std::fs::File> = None;
        let mut file_day = String::new();
        let mut file_size = 0u64;
        let max_file: u64 = 16 << 20;

        loop {
            let events: Vec<Event> = match rx.recv() {
                Ok(first) => {
                    let mut v = vec![first];
                    while let Ok(e) = rx.try_recv() {
                        v.push(e);
                    }
                    v
                }
                Err(_) => break,
            };
            for e in events {
                let day = day_of(e.time);
                if file_day != day || file.is_none() {
                    if let Some(mut f) = file.take() {
                        let _ = f.flush();
                    }
                    let path = st2.cfg.dir.join(format!("requests-{day}.jsonl"));
                    match OpenOptions::new().create(true).append(true).open(&path) {
                        Ok(f) => {
                            file_size = f.metadata().map(|m| m.len()).unwrap_or(0);
                            file = Some(f);
                            file_day = day.clone();
                        }
                        Err(e) => {
                            *st2.last_error.lock().unwrap() = e.to_string();
                            continue;
                        }
                    }
                }
                if file_size > 0 && file_size + 4096 > max_file {
                    let mut idx = 1;
                    let path = loop {
                        let p = st2.cfg.dir.join(format!("requests-{day}.{idx}.jsonl"));
                        if !p.exists() {
                            break p;
                        }
                        idx += 1;
                    };
                    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&path) {
                        file_size = 0;
                        file = Some(f);
                    }
                }
                if let Some(f) = file.as_mut() {
                    if let Ok(line) = serde_json::to_string(&e) {
                        if let Err(e) = writeln!(f, "{line}") {
                            *st2.last_error.lock().unwrap() = e.to_string();
                        } else {
                            file_size += line.len() as u64 + 1;
                        }
                    }
                }
            }
            if let Some(f) = file.as_mut() {
                let _ = f.flush();
            }
            prune(&st2.cfg);
        }
        if let Some(f) = file.as_mut() {
            let _ = f.flush();
        }
        st2.alive.store(false, Ordering::SeqCst);
    });
    st
}

fn prune(cfg: &ArchiveConfig) {
    let Ok(entries) = fs::read_dir(&cfg.dir) else {
        return;
    };
    let cutoff = now_ms() - cfg.retention_days as i64 * 86_400_000;
    let mut total = 0u64;
    let mut files: Vec<(PathBuf, i64, u64)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !(name.starts_with("requests-") && name.ends_with(".jsonl")) {
            continue;
        }
        let path = entry.path();
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        total += md.len();
        files.push((path, mtime, md.len()));
    }
    files.sort_by_key(|f| f.1);
    for (path, mtime, size) in files {
        if mtime < cutoff || total > cfg.max_bytes {
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }
}

fn day_of(ms: i64) -> String {
    // 将 ms epoch 转为 UTC 日（归档按天滚动，本地时区偏差可忽略）
    let secs = ms / 1000;
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

// Howard Hinnant 的 civil_from_days 算法
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn contains_fold(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_model_filter_matches_only_the_selected_model() {
        let usage = crate::usage::UsageRecorder::new(String::new());
        let recorder = Recorder::new(ArchiveConfig {
            enabled: false,
            dir: PathBuf::new(),
            retention_days: 0,
            max_bytes: 0,
        });
        for (index, model) in ["cn:workbuddy-fast", "cn:workbuddy-fast-plus"]
            .iter()
            .enumerate()
        {
            let event = serde_json::from_value(serde_json::json!({
                "time": index + 1,
                "request_id": index.to_string(),
                "path": "/v1/chat/completions",
                "model": model,
                "status": 200,
                "ok": true,
                "outcome": OUTCOME_SUCCESS,
                "duration_ms": 10
            }))
            .unwrap();
            recorder.record(event);
            usage.record(crate::usage::UsageDelta {
                model: model.to_string(),
                ok: true,
                ..Default::default()
            });
        }

        let exact = serde_json::from_value(serde_json::json!({"model_exact": "cn:workbuddy-fast"}))
            .unwrap();
        let rows = recorder.read_archive(8, exact);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "cn:workbuddy-fast");
        let snapshot = usage.dashboard(&crate::usage::UsageQuery {
            model: "cn:workbuddy-fast".into(),
            ..Default::default()
        });
        assert_eq!(snapshot.snapshot.total_requests as usize, rows.len());

        let free_text =
            serde_json::from_value(serde_json::json!({"model": "WORKBUDDY-FAST"})).unwrap();
        assert_eq!(recorder.read_archive(8, free_text).len(), 2);
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_request_id() -> String {
    format!("req-{}", uuid::Uuid::new_v4().simple())
}
