//! Persistent hourly usage. Every dashboard dimension uses the same selected window.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Bucket {
    pub slot: String,
    pub realm: String,
    pub uid: String,
    pub model: String,
    pub requests: i64,
    pub errors: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub latency_ms_sum: i64,
    pub latency_count: i64,
    pub credit_sum: f64,
    pub credit_count: i64,
    pub ttfb_ms_sum: i64,
    pub ttfb_count: i64,
    pub tps_sum: f64,
    pub tps_count: i64,
    pub cache_hit_tokens: i64,
    pub cache_miss_tokens: i64,
    pub cache_count: i64,
}
#[derive(Debug, Clone, Default)]
pub struct UsageDelta {
    pub realm: String,
    pub uid: String,
    pub model: String,
    pub ok: bool,
    pub latency_ms: i64,
    pub ttfb_ms: Option<i64>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub credit: f64,
    pub credit_known: bool,
    pub cache_hit_tokens: i64,
    pub cache_miss_tokens: i64,
    pub cache_known: bool,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UsageQuery {
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub uid: String,
    pub model: String,
    pub granularity: String,
    pub timezone_offset_minutes: Option<i32>,
}
impl UsageQuery {
    pub fn validate(&self) -> Result<(), String> {
        if let (Some(from), Some(to)) = (self.from_ms, self.to_ms) {
            if from >= to {
                return Err("统计结束时间必须晚于开始时间".into());
            }
        }
        if !self.granularity.is_empty() && self.granularity != "hour" && self.granularity != "day" {
            return Err("统计粒度必须为 hour 或 day".into());
        }
        Ok(())
    }
    fn matches_dimension(&self, uid: &str, model: &str) -> bool {
        (self.uid.is_empty() || self.uid == uid) && (self.model.is_empty() || self.model == model)
    }
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct Metrics {
    pub requests: i64,
    pub errors: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub credit: f64,
    pub avg_latency_ms: f64,
    pub cache_hit_tokens: i64,
    pub cache_miss_tokens: i64,
}
#[derive(Debug, Clone, Serialize)]
pub struct ModelStat {
    pub model: String,
    #[serde(flatten)]
    pub metrics: Metrics,
}
#[derive(Debug, Clone, Serialize)]
pub struct AccountStat {
    pub uid: String,
    pub realm: String,
    #[serde(flatten)]
    pub metrics: Metrics,
}
#[derive(Debug, Clone, Serialize)]
pub struct UsagePoint {
    pub time_ms: i64,
    #[serde(flatten)]
    pub metrics: Metrics,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageSnapshot {
    pub saved_at: i64,
    pub total_requests: i64,
    pub total_errors: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub avg_latency_ms: f64,
    pub total_credit: f64,
    pub by_model: Vec<ModelStat>,
    pub by_account: Vec<AccountStat>,
}
#[derive(Debug, Clone, Serialize)]
pub struct UsageDashboard {
    #[serde(flatten)]
    pub snapshot: UsageSnapshot,
    pub window_from_ms: Option<i64>,
    pub window_to_ms: Option<i64>,
    pub granularity: String,
    pub success_rate: f64,
    pub avg_ttfb_ms: Option<f64>,
    pub avg_tokens_per_second: Option<f64>,
    pub cache_hit_tokens: i64,
    pub cache_miss_tokens: i64,
    pub cache_hit_rate: Option<f64>,
    pub live_rpm: i64,
    pub live_tpm: i64,
    pub series: Vec<UsagePoint>,
}
#[derive(Serialize, Deserialize)]
struct DiskFile {
    version: u32,
    saved: String,
    buckets: Vec<Bucket>,
}
struct LiveSample {
    time: i64,
    uid: String,
    model: String,
    total_tokens: i64,
}
pub struct UsageRecorder {
    path: String,
    buckets: Mutex<HashMap<String, Bucket>>,
    live: Mutex<VecDeque<LiveSample>>,
    last_persist: Mutex<Instant>,
    dirty: AtomicBool,
}
#[derive(Default)]
struct Accumulator {
    metrics: Metrics,
    latency_sum: i64,
    latency_count: i64,
    ttfb_sum: i64,
    ttfb_count: i64,
    tps_sum: f64,
    tps_count: i64,
    cache_count: i64,
}
impl Accumulator {
    fn add(&mut self, b: &Bucket) {
        self.metrics.requests += b.requests;
        self.metrics.errors += b.errors;
        self.metrics.prompt_tokens += b.prompt_tokens;
        self.metrics.completion_tokens += b.completion_tokens;
        self.metrics.total_tokens += b.total_tokens;
        self.metrics.credit += b.credit_sum;
        self.metrics.cache_hit_tokens += b.cache_hit_tokens;
        self.metrics.cache_miss_tokens += b.cache_miss_tokens;
        self.latency_sum += b.latency_ms_sum;
        self.latency_count += b.latency_count;
        self.ttfb_sum += b.ttfb_ms_sum;
        self.ttfb_count += b.ttfb_count;
        self.tps_sum += b.tps_sum;
        self.tps_count += b.tps_count;
        self.cache_count += b.cache_count;
    }
    fn finish(&self) -> Metrics {
        let mut metrics = self.metrics.clone();
        if self.latency_count > 0 {
            metrics.avg_latency_ms = self.latency_sum as f64 / self.latency_count as f64;
        }
        metrics
    }
}
impl UsageRecorder {
    pub fn new(path: String) -> Self {
        let mut recorder = Self {
            path,
            buckets: Mutex::new(HashMap::new()),
            live: Mutex::new(VecDeque::new()),
            last_persist: Mutex::new(Instant::now() - std::time::Duration::from_secs(10)),
            dirty: AtomicBool::new(false),
        };
        recorder.load();
        recorder
    }
    fn load(&mut self) {
        if let Ok(raw) = std::fs::read_to_string(&self.path) {
            if let Ok(file) = serde_json::from_str::<DiskFile>(&raw) {
                let mut map = self.buckets.lock().unwrap();
                for bucket in file.buckets {
                    map.insert(
                        bucket_key(&bucket.slot, &bucket.realm, &bucket.uid, &bucket.model),
                        bucket,
                    );
                }
            }
        }
    }
    pub fn record(&self, delta: UsageDelta) {
        self.record_at(super::reqlog::now_ms(), delta);
    }
    fn record_at(&self, now: i64, delta: UsageDelta) {
        let slot = hour_slot(now);
        let key = bucket_key(&slot, &delta.realm, &delta.uid, &delta.model);
        let total_tokens = if delta.total_tokens > 0 {
            delta.total_tokens
        } else {
            delta.prompt_tokens + delta.completion_tokens
        }
        .max(0);
        {
            let mut map = self.buckets.lock().unwrap();
            let bucket = map.entry(key).or_insert_with(|| Bucket {
                slot,
                realm: delta.realm.clone(),
                uid: delta.uid.clone(),
                model: delta.model.clone(),
                ..Default::default()
            });
            bucket.requests += 1;
            bucket.errors += i64::from(!delta.ok);
            bucket.prompt_tokens += delta.prompt_tokens.max(0);
            bucket.completion_tokens += delta.completion_tokens.max(0);
            bucket.total_tokens += total_tokens;
            if delta.latency_ms >= 0 {
                bucket.latency_ms_sum += delta.latency_ms;
                bucket.latency_count += 1;
            }
            if delta.credit_known && delta.credit.is_finite() && delta.credit >= 0.0 {
                bucket.credit_sum += delta.credit;
                bucket.credit_count += 1;
            }
            if let Some(ttfb) = delta.ttfb_ms.filter(|value| *value >= 0) {
                bucket.ttfb_ms_sum += ttfb;
                bucket.ttfb_count += 1;
                let generation_ms = delta.latency_ms - ttfb;
                if delta.ok && delta.completion_tokens > 0 && generation_ms > 0 {
                    bucket.tps_sum +=
                        delta.completion_tokens as f64 * 1000.0 / generation_ms as f64;
                    bucket.tps_count += 1;
                }
            }
            if delta.cache_known {
                bucket.cache_hit_tokens += delta.cache_hit_tokens.max(0);
                bucket.cache_miss_tokens += delta.cache_miss_tokens.max(0);
                bucket.cache_count += 1;
            }
            self.dirty.store(true, Ordering::Relaxed);
        }
        {
            let mut live = self.live.lock().unwrap();
            live.retain(|sample| sample.time > now - 60_000);
            live.push_back(LiveSample {
                time: now,
                uid: delta.uid,
                model: delta.model,
                total_tokens,
            });
        }
        self.maybe_persist();
    }
    fn maybe_persist(&self) {
        if self.path.is_empty() || !self.dirty.load(Ordering::Relaxed) {
            return;
        }
        let mut last = self.last_persist.lock().unwrap();
        if last.elapsed().as_secs() < 3 {
            return;
        }
        // Serialize replacements: concurrent writers must not share the temporary file.
        let buckets = {
            let buckets = self.buckets.lock().unwrap();
            self.dirty.store(false, Ordering::Relaxed);
            buckets.values().cloned().collect()
        };
        let disk = DiskFile {
            version: 2,
            saved: chrono_like_now(),
            buckets,
        };
        if let Ok(out) = serde_json::to_string_pretty(&disk) {
            if let Some(dir) = std::path::Path::new(&self.path).parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = format!("{}.tmp", self.path);
            if std::fs::write(&tmp, out).is_ok() && std::fs::rename(&tmp, &self.path).is_ok() {
                *last = Instant::now();
            } else {
                self.dirty.store(true, Ordering::Relaxed);
            }
        } else {
            self.dirty.store(true, Ordering::Relaxed);
        }
    }
    pub fn start_persistence(self: &std::sync::Arc<Self>) {
        let weak = std::sync::Arc::downgrade(self);
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(3));
            let Some(recorder) = weak.upgrade() else {
                break;
            };
            recorder.maybe_persist();
        });
    }
    pub fn flush(&self) {
        *self.last_persist.lock().unwrap() = Instant::now() - std::time::Duration::from_secs(10);
        self.maybe_persist();
    }
    pub fn snapshot(&self) -> UsageSnapshot {
        self.dashboard(&UsageQuery::default()).snapshot
    }
    pub fn dashboard(&self, query: &UsageQuery) -> UsageDashboard {
        self.dashboard_at(query, super::reqlog::now_ms())
    }
    fn dashboard_at(&self, query: &UsageQuery, now: i64) -> UsageDashboard {
        let offset = query
            .timezone_offset_minutes
            .unwrap_or(480)
            .clamp(-840, 840) as i64
            * 60_000;
        let daily = query.granularity == "day";
        let step = if daily { DAY_MS } else { HOUR_MS };
        let point_time = |time: i64| (time + offset).div_euclid(step) * step - offset;
        let mut total = Accumulator::default();
        let mut models: HashMap<String, Accumulator> = HashMap::new();
        let mut accounts: HashMap<(String, String), Accumulator> = HashMap::new();
        let mut series: BTreeMap<i64, Accumulator> = BTreeMap::new();
        for bucket in self.buckets.lock().unwrap().values() {
            let Some(time) = slot_time(&bucket.slot) else {
                continue;
            };
            if !query.matches_dimension(&bucket.uid, &bucket.model)
                || query.from_ms.is_some_and(|from| time < from)
                || query.to_ms.is_some_and(|to| time >= to)
            {
                continue;
            }
            total.add(bucket);
            models.entry(bucket.model.clone()).or_default().add(bucket);
            accounts
                .entry((bucket.realm.clone(), bucket.uid.clone()))
                .or_default()
                .add(bucket);
            series.entry(point_time(time)).or_default().add(bucket);
        }
        // Empty chart slots are meaningful. Bound expansion for unusually large custom ranges.
        if let (Some(from), Some(to)) = (query.from_ms, query.to_ms) {
            let mut time = point_time(from);
            if to > from && (to - from) / step <= 2_200 {
                while time < to {
                    series.entry(time).or_default();
                    time += step;
                }
            }
        }
        let mut live = self.live.lock().unwrap();
        live.retain(|sample| sample.time > now - 60_000);
        let samples: Vec<_> = live
            .iter()
            .filter(|sample| query.matches_dimension(&sample.uid, &sample.model))
            .collect();
        let metrics = total.finish();
        let mut by_model: Vec<_> = models
            .into_iter()
            .map(|(model, acc)| ModelStat {
                model,
                metrics: acc.finish(),
            })
            .collect();
        by_model.sort_by(|a, b| {
            b.metrics
                .total_tokens
                .cmp(&a.metrics.total_tokens)
                .then_with(|| a.model.cmp(&b.model))
        });
        let mut by_account: Vec<_> = accounts
            .into_iter()
            .map(|((realm, uid), acc)| AccountStat {
                realm,
                uid,
                metrics: acc.finish(),
            })
            .collect();
        by_account.sort_by(|a, b| {
            b.metrics
                .total_tokens
                .cmp(&a.metrics.total_tokens)
                .then_with(|| a.uid.cmp(&b.uid))
        });
        let cache_total = metrics.cache_hit_tokens + metrics.cache_miss_tokens;
        UsageDashboard {
            snapshot: UsageSnapshot {
                saved_at: now,
                total_requests: metrics.requests,
                total_errors: metrics.errors,
                prompt_tokens: metrics.prompt_tokens,
                completion_tokens: metrics.completion_tokens,
                total_tokens: metrics.total_tokens,
                avg_latency_ms: metrics.avg_latency_ms,
                total_credit: metrics.credit,
                by_model,
                by_account,
            },
            window_from_ms: query.from_ms,
            window_to_ms: query.to_ms,
            granularity: if daily { "day" } else { "hour" }.into(),
            success_rate: if metrics.requests > 0 {
                (metrics.requests - metrics.errors) as f64 / metrics.requests as f64 * 100.0
            } else {
                0.0
            },
            avg_ttfb_ms: (total.ttfb_count > 0)
                .then(|| total.ttfb_sum as f64 / total.ttfb_count as f64),
            avg_tokens_per_second: (total.tps_count > 0)
                .then(|| total.tps_sum / total.tps_count as f64),
            cache_hit_tokens: metrics.cache_hit_tokens,
            cache_miss_tokens: metrics.cache_miss_tokens,
            cache_hit_rate: (total.cache_count > 0 && cache_total > 0)
                .then(|| metrics.cache_hit_tokens as f64 / cache_total as f64 * 100.0),
            live_rpm: samples.len() as i64,
            live_tpm: samples.iter().map(|sample| sample.total_tokens).sum(),
            series: series
                .into_iter()
                .map(|(time_ms, acc)| UsagePoint {
                    time_ms,
                    metrics: acc.finish(),
                })
                .collect(),
        }
    }
}
fn bucket_key(slot: &str, realm: &str, uid: &str, model: &str) -> String {
    format!("{slot}|{realm}|{uid}|{model}")
}
fn hour_slot(ms: i64) -> String {
    let (year, month, day) = civil_from_days(ms.div_euclid(DAY_MS));
    let hour = ms.rem_euclid(DAY_MS) / HOUR_MS;
    format!("h:{year:04}-{month:02}-{day:02}T{hour:02}")
}
fn slot_time(slot: &str) -> Option<i64> {
    let text = slot.strip_prefix("h:")?;
    if text.len() != 13 || !text.is_ascii() {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || !(0..24).contains(&hour) {
        return None;
    }
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let timestamp = (era * 146_097 + doe - 719_468) * DAY_MS + hour * HOUR_MS;
    (hour_slot(timestamp) == slot).then_some(timestamp)
}
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}
fn chrono_like_now() -> String {
    let millis = super::reqlog::now_ms();
    let slot = hour_slot(millis);
    let seconds = millis.div_euclid(1000).rem_euclid(3600);
    format!("{}:{:02}:{:02}Z", &slot[2..], seconds / 60, seconds % 60)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flush_preserves_the_last_debounced_record_and_old_files() {
        let directory =
            std::env::temp_dir().join(format!("wb-usage-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("usage.json");
        std::fs::write(&path, r#"{"version":1,"saved":"","buckets":[{"slot":"h:1970-01-01T00","realm":"cn","uid":"a","model":"m1","requests":2,"total_tokens":30}]}"#).unwrap();
        let recorder = UsageRecorder::new(path.display().to_string());
        assert_eq!(recorder.snapshot().total_requests, 2);
        recorder.record_at(HOUR_MS, delta("a", "m1", true, 10, 50));
        recorder.record_at(HOUR_MS, delta("a", "m1", false, 0, 60));
        recorder.flush();
        let reloaded = UsageRecorder::new(path.display().to_string());
        assert_eq!(reloaded.snapshot().total_requests, 4);
        assert_eq!(reloaded.snapshot().total_errors, 1);
        assert_eq!(reloaded.snapshot().total_tokens, 40);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    fn delta(uid: &str, model: &str, ok: bool, tokens: i64, latency: i64) -> UsageDelta {
        UsageDelta {
            uid: uid.into(),
            model: model.into(),
            realm: "cn".into(),
            ok,
            total_tokens: tokens,
            latency_ms: latency,
            ..Default::default()
        }
    }
    #[test]
    fn window_filters_every_dimension_and_weights_latency() {
        let r = UsageRecorder::new(String::new());
        r.record_at(0, delta("a", "m1", true, 10, 100));
        r.record_at(HOUR_MS, delta("a", "m1", true, 20, 200));
        r.record_at(HOUR_MS, delta("a", "m1", false, 0, 600));
        r.record_at(2 * HOUR_MS, delta("b", "m2", true, 50, 900));
        let q = UsageQuery {
            from_ms: Some(HOUR_MS),
            to_ms: Some(2 * HOUR_MS),
            ..Default::default()
        };
        let d = r.dashboard_at(&q, 3 * HOUR_MS);
        assert_eq!(d.snapshot.total_requests, 2);
        assert_eq!(d.snapshot.total_errors, 1);
        assert_eq!(d.snapshot.total_tokens, 20);
        assert_eq!(d.snapshot.avg_latency_ms, 400.0);
        assert_eq!(d.success_rate, 50.0);
        assert_eq!(d.snapshot.by_model.len(), 1);
        assert_eq!(d.snapshot.by_account.len(), 1);
        assert_eq!(d.series.len(), 1);
        assert_eq!(d.series[0].metrics.requests, 2);
    }
    #[test]
    fn live_window_and_missing_metrics_are_honest() {
        let r = UsageRecorder::new(String::new());
        r.record_at(1_000, delta("a", "m1", false, 0, 50));
        r.record_at(50_000, delta("a", "m1", true, 100, 500));
        r.record_at(55_000, delta("b", "m1", true, 200, 500));
        let d = r.dashboard_at(
            &UsageQuery {
                uid: "a".into(),
                ..Default::default()
            },
            62_000,
        );
        assert_eq!(d.live_rpm, 1);
        assert_eq!(d.live_tpm, 100);
        assert_eq!(d.snapshot.total_requests, 2);
        assert_eq!(d.avg_ttfb_ms, None);
        assert_eq!(d.avg_tokens_per_second, None);
        assert_eq!(d.cache_hit_rate, None);
    }
    #[test]
    fn cache_tps_daily_grouping_and_empty_hours() {
        let r = UsageRecorder::new(String::new());
        r.record_at(
            16 * HOUR_MS,
            UsageDelta {
                prompt_tokens: 100,
                completion_tokens: 20,
                ttfb_ms: Some(100),
                cache_known: true,
                cache_hit_tokens: 80,
                cache_miss_tokens: 20,
                ..delta("a", "m", true, 0, 1100)
            },
        );
        let q = UsageQuery {
            from_ms: Some(15 * HOUR_MS),
            to_ms: Some(18 * HOUR_MS),
            granularity: "day".into(),
            timezone_offset_minutes: Some(480),
            ..Default::default()
        };
        let d = r.dashboard_at(&q, 18 * HOUR_MS);
        assert_eq!(d.snapshot.total_tokens, 120);
        assert_eq!(d.cache_hit_rate, Some(80.0));
        assert_eq!(d.avg_tokens_per_second, Some(20.0));
        assert_eq!(d.avg_ttfb_ms, Some(100.0));
        assert_eq!(d.series.len(), 2);
        assert_eq!(d.series[1].time_ms, 16 * HOUR_MS);
    }
    #[test]
    fn old_buckets_deserialize_and_slots_roundtrip() {
        let b: Bucket =
            serde_json::from_str(r#"{"slot":"h:2026-11-05T08","requests":2,"total_tokens":30}"#)
                .unwrap();
        assert_eq!(b.cache_count, 0);
        for t in [-HOUR_MS, 0, 1_793_865_600_000, 2_000_000_000_000] {
            assert_eq!(
                slot_time(&hour_slot(t)),
                Some(t.div_euclid(HOUR_MS) * HOUR_MS)
            );
        }
        assert!(slot_time("h:2026-02-31T08").is_none());
    }
}
