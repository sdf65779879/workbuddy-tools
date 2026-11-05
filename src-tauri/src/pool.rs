//! 账号池：最久未使用优先 + 冷却/熔断 + 可选会话粘性。

use crate::config::{AccountConfig, AppConfig, PoolConfig};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub type SharedConfig = std::sync::Arc<tokio::sync::RwLock<AppConfig>>;

#[derive(Debug, Clone)]
pub struct AcctStatus {
    pub credits: Option<i64>,
    pub remaining_credits: Option<f64>,
    pub total_credits: Option<f64>,
    pub used_credits: Option<f64>,
    pub unlimited: bool,
    pub cooling_until: Option<Instant>,
    pub cool_kind: String,
    pub reason: String,
    pub soft_streak: u32,
    pub consecutive_fails: u32,
    pub success_count: i64,
    pub err_total: i64,
    pub in_flight: usize,
    pub last_used: Option<Instant>,
    pub checkin_done: bool,
    checkin_day: Option<i64>,
}

impl Default for AcctStatus {
    fn default() -> Self {
        Self {
            credits: None,
            remaining_credits: None,
            total_credits: None,
            used_credits: None,
            unlimited: false,
            cooling_until: None,
            cool_kind: String::new(),
            reason: String::new(),
            soft_streak: 0,
            consecutive_fails: 0,
            success_count: 0,
            err_total: 0,
            in_flight: 0,
            last_used: None,
            checkin_done: false,
            checkin_day: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 请求通道校验拒绝：不是余额或账号健康故障，不罚账号。
    ChannelRefused,
    /// 上游内容策略拒绝：请求级失败，不改变账号健康状态。
    ContentBlocked,
    /// 429 限流 → 软冷却，指数退避
    SoftRate,
    /// 402/403 余额不足或风控 → 硬冷却
    HardCredit,
    /// 404 模型/端点不存在 → 短冷却
    NotFound,
    /// 其他错误 → 连续失败熔断
    Other,
}

struct Sticky {
    uid: String,
    until: Instant,
}

struct Inner {
    statuses: HashMap<String, AcctStatus>,
    sticky: HashMap<String, Sticky>,
    rng_state: u64,
}

pub struct Pool {
    cfg: SharedConfig,
    inner: Mutex<Inner>,
}

impl Pool {
    pub fn new(cfg: SharedConfig) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            cfg,
            inner: Mutex::new(Inner {
                statuses: HashMap::new(),
                sticky: HashMap::new(),
                rng_state: super::reqlog::now_ms() as u64 | 1,
            }),
        })
    }

    fn next_rand(state: &mut u64) -> f64 {
        // xorshift64
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    async fn accounts(&self) -> (Vec<AccountConfig>, PoolConfig) {
        let cfg = self.cfg.read().await;
        let accounts: Vec<AccountConfig> = cfg
            .accounts
            .iter()
            .filter(|a| a.enabled && !a.paused && !a.token.trim().is_empty())
            .filter(|a| {
                crate::renewal::token_expiry_ms(a)
                    .map_or(true, |expiry| expiry > super::reqlog::now_ms())
            })
            .cloned()
            .collect();
        (accounts, cfg.pool.clone())
    }

    /// 选号。tried 为本次请求已尝试失败的 uid 集合；conv_id 用于会话粘性。
    pub async fn pick(
        &self,
        tried: &HashSet<String>,
        conv_id: Option<&str>,
    ) -> Option<AccountConfig> {
        let (accounts, pool_cfg) = self.accounts().await;
        if accounts.is_empty() {
            return None;
        }
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        inner.sticky.retain(|_, s| s.until > now);

        // Legacy weighted_random settings adopt deterministic idle-first rotation too.
        let idle_first = !matches!(pool_cfg.strategy.as_str(), "order" | "random");
        let use_sticky = pool_cfg.sticky_enabled && !idle_first;
        if use_sticky {
            if let Some(conv) = conv_id {
                if let Some(sticky_uid) = inner.sticky.get(conv).map(|s| s.uid.clone()) {
                    if let Some(acc) = accounts.iter().find(|a| a.uid == sticky_uid) {
                        if !tried.contains(&sticky_uid)
                            && Self::healthy(&inner, &sticky_uid, &pool_cfg, now)
                        {
                            let st = inner.statuses.entry(sticky_uid.clone()).or_default();
                            st.in_flight += 1;
                            st.last_used = Some(now);
                            inner.sticky.insert(
                                conv.to_string(),
                                Sticky {
                                    uid: sticky_uid,
                                    until: now + Duration::from_secs(pool_cfg.sticky_ttl_secs),
                                },
                            );
                            return Some(acc.clone());
                        }
                    }
                }
            }
        }

        let healthy: Vec<&AccountConfig> = accounts
            .iter()
            .filter(|a| !tried.contains(&a.uid))
            .filter(|a| Self::healthy(&inner, &a.uid, &pool_cfg, now))
            .collect();

        let picked: Option<&AccountConfig> = if pool_cfg.strategy == "order" {
            healthy.first().copied()
        } else if pool_cfg.strategy == "random" {
            if healthy.is_empty() {
                return None;
            }
            let r = Self::next_rand(&mut inner.rng_state);
            let idx = (r * healthy.len() as f64) as usize;
            healthy.get(idx.min(healthy.len() - 1)).copied()
        } else {
            healthy.iter().copied().min_by_key(|account| {
                let status = inner.statuses.get(&account.uid);
                (
                    status.and_then(|status| status.last_used),
                    status.map_or(0, |status| status.in_flight),
                )
            })
        };

        let Some(acc) = picked else {
            return None;
        };

        let st = inner.statuses.entry(acc.uid.clone()).or_default();
        st.in_flight += 1;
        st.last_used = Some(now);
        if use_sticky {
            if let Some(conv) = conv_id {
                inner.sticky.insert(
                    conv.to_string(),
                    Sticky {
                        uid: acc.uid.clone(),
                        until: now + Duration::from_secs(pool_cfg.sticky_ttl_secs),
                    },
                );
            }
        }
        Some(acc.clone())
    }

    fn healthy(inner: &Inner, uid: &str, pool_cfg: &PoolConfig, now: Instant) -> bool {
        let Some(st) = inner.statuses.get(uid) else {
            return true;
        };
        if let Some(until) = st.cooling_until {
            if now < until {
                return false;
            }
        }
        if pool_cfg.max_in_flight > 0 && st.in_flight >= pool_cfg.max_in_flight {
            return false;
        }
        let remaining = st
            .remaining_credits
            .or_else(|| st.credits.map(|credits| credits as f64));
        if !st.unlimited
            && remaining.is_some_and(|credits| {
                credits <= 0.0 || credits < pool_cfg.credit_floor.max(0) as f64
            })
        {
            return false;
        }
        true
    }

    pub fn release(&self, uid: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(st) = inner.statuses.get_mut(uid) {
            st.in_flight = st.in_flight.saturating_sub(1);
        }
    }

    pub async fn note_success(&self, uid: &str, credits: Option<i64>) {
        let mut inner = self.inner.lock().unwrap();
        let st = inner.statuses.entry(uid.to_string()).or_default();
        // A concurrent request may have reported exhausted credits after this one started.
        // One late success cannot make that account eligible again.
        if st.cool_kind != "hard_credit" {
            st.cooling_until = None;
            st.cool_kind.clear();
            st.reason.clear();
        }
        st.soft_streak = 0;
        st.consecutive_fails = 0;
        st.success_count += 1;
        if let Some(c) = credits {
            st.credits = Some(c);
        }
    }

    pub async fn note_error(&self, uid: &str, kind: ErrorKind, reason: &str) {
        // Request/channel policy failures are still recorded by gateway accounting,
        // but must not cool, trip, unstick or otherwise mutate healthy accounts.
        if matches!(kind, ErrorKind::ChannelRefused | ErrorKind::ContentBlocked) {
            return;
        }
        let (_accounts, pool_cfg) = self.accounts().await;
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        let st = inner.statuses.entry(uid.to_string()).or_default();
        st.err_total += 1;
        st.reason = reason.to_string();
        inner.sticky.retain(|_, sticky| sticky.uid != uid);
        let st = inner.statuses.get_mut(uid).unwrap();
        match kind {
            ErrorKind::ChannelRefused | ErrorKind::ContentBlocked => {}
            ErrorKind::SoftRate => {
                let base = pool_cfg.soft_cooldown_secs.max(60);
                let max = pool_cfg.soft_cooldown_max_secs.max(base);
                let secs = base.saturating_mul(1u64 << st.soft_streak.min(6)).min(max);
                st.soft_streak += 1;
                st.cool_kind = "soft_rate".to_string();
                st.cooling_until = Some(now + Duration::from_secs(secs));
            }
            ErrorKind::HardCredit => {
                st.cool_kind = "hard_credit".to_string();
                st.cooling_until = Some(now + Duration::from_secs(6 * 3600));
            }
            ErrorKind::NotFound => {
                st.cool_kind = "not_found".to_string();
                st.cooling_until = Some(now + Duration::from_secs(60));
            }
            ErrorKind::Other => {
                st.consecutive_fails += 1;
                if st.consecutive_fails >= pool_cfg.breaker_threshold.max(1) {
                    st.cool_kind = "breaker".to_string();
                    st.cooling_until = Some(now + Duration::from_secs(30 * 60));
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn set_credits(&self, uid: &str, credits: i64) {
        let mut inner = self.inner.lock().unwrap();
        let status = inner.statuses.entry(uid.to_string()).or_default();
        status.credits = Some(credits);
        status.remaining_credits = Some(credits as f64);
        status.unlimited = false;
    }

    pub fn set_balance(&self, uid: &str, balance: &crate::balances::BalanceObservation) {
        let mut inner = self.inner.lock().unwrap();
        let status = inner.statuses.entry(uid.to_string()).or_default();
        // Routing needs a finite weight; this sentinel is never exposed as an observed balance.
        status.credits = if balance.unlimited {
            Some(1 << 40)
        } else {
            balance.remaining.map(|remaining| remaining.round() as i64)
        };
        status.remaining_credits = balance.remaining;
        status.total_credits = balance.total;
        status.used_credits = balance.used;
        status.unlimited = balance.unlimited;
        if status.cool_kind == "hard_credit"
            && (balance.unlimited || balance.remaining.is_some_and(|remaining| remaining > 0.0))
        {
            status.cooling_until = None;
            status.cool_kind.clear();
            status.reason.clear();
        }
    }

    pub fn set_checkin_done(&self, uid: &str, started_at_ms: i64) {
        self.set_checkin_done_for_request_at(uid, started_at_ms, super::reqlog::now_ms());
    }

    fn set_checkin_done_for_request_at(&self, uid: &str, started_at_ms: i64, finished_at_ms: i64) {
        // A long task/batch may finish after the day its check-in was confirmed.
        // Keep that response unconfirmed for the new day rather than marking it done.
        if beijing_day(started_at_ms) != beijing_day(finished_at_ms) {
            return;
        }
        // Anchor the write to the request's day even if acquiring the lock crosses midnight.
        self.set_checkin_done_at(uid, started_at_ms);
    }

    fn set_checkin_done_at(&self, uid: &str, now_ms: i64) {
        let mut inner = self.inner.lock().unwrap();
        let status = inner.statuses.entry(uid.to_string()).or_default();
        status.checkin_done = true;
        status.checkin_day = Some(beijing_day(now_ms));
    }

    pub fn statuses(&self, accounts: &[AccountConfig]) -> Vec<(AccountConfig, AcctStatus)> {
        self.statuses_at(accounts, super::reqlog::now_ms())
    }

    fn statuses_at(
        &self,
        accounts: &[AccountConfig],
        now_ms: i64,
    ) -> Vec<(AccountConfig, AcctStatus)> {
        let mut inner = self.inner.lock().unwrap();
        let day = beijing_day(now_ms);
        for status in inner.statuses.values_mut() {
            status.checkin_done = status.checkin_day == Some(day);
        }
        accounts
            .iter()
            .map(|a| {
                let st = inner.statuses.get(&a.uid).cloned().unwrap_or_default();
                (a.clone(), st)
            })
            .collect()
    }
}

fn beijing_day(now_ms: i64) -> i64 {
    (now_ms + 8 * 3_600_000).div_euclid(86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(uid: &str) -> AccountConfig {
        serde_json::from_value(serde_json::json!({
            "uid":uid, "nickname":uid, "token":"test-token", "realm":"cn", "enabled":true, "paused":false
        })).unwrap()
    }

    fn test_pool(strategy: &str) -> (std::sync::Arc<Pool>, SharedConfig) {
        let mut config = AppConfig::default();
        config.accounts = vec![account("a"), account("b"), account("c")];
        config.pool.strategy = strategy.into();
        config.pool.credit_floor = 0;
        let config = std::sync::Arc::new(tokio::sync::RwLock::new(config));
        (Pool::new(config.clone()), config)
    }

    #[tokio::test]
    async fn idle_first_rotation_is_deterministic_even_for_the_same_conversation() {
        for strategy in ["least_recent", "weighted_random"] {
            let (pool, _) = test_pool(strategy);
            for expected in ["a", "b", "c", "a"] {
                let chosen = pool
                    .pick(&HashSet::new(), Some("same-conversation"))
                    .await
                    .unwrap();
                assert_eq!(chosen.uid, expected);
                pool.release(&chosen.uid);
            }
        }
    }

    #[tokio::test]
    async fn retried_sticky_accounts_and_known_zero_credit_accounts_are_excluded() {
        let (pool, _) = test_pool("order");
        let chosen = pool
            .pick(&HashSet::new(), Some("conversation"))
            .await
            .unwrap();
        pool.release(&chosen.uid);
        let tried = HashSet::from(["a".to_string()]);
        assert_eq!(
            pool.pick(&tried, Some("conversation")).await.unwrap().uid,
            "b"
        );
        pool.set_credits("a", 0);
        pool.set_balance(
            "b",
            &crate::balances::BalanceObservation {
                remaining: Some(0.25),
                total: Some(1.0),
                used: Some(0.75),
                unlimited: false,
            },
        );
        assert_eq!(pool.pick(&HashSet::new(), None).await.unwrap().uid, "b");
    }

    #[tokio::test]
    async fn empty_expired_cooling_and_exhausted_accounts_cannot_bypass_health_checks() {
        let (pool, config) = test_pool("weighted_random");
        {
            let mut config = config.write().await;
            config.accounts[0].token.clear();
            config.accounts[1].expires_at = Some(1);
        }
        pool.note_error("c", ErrorKind::HardCredit, "balance exhausted")
            .await;
        assert!(pool.pick(&HashSet::new(), None).await.is_none());
        pool.set_credits("c", 0);
        pool.inner
            .lock()
            .unwrap()
            .statuses
            .get_mut("c")
            .unwrap()
            .cooling_until = None;
        assert!(pool.pick(&HashSet::new(), None).await.is_none());
    }

    #[tokio::test]
    async fn exhaustion_survives_late_success_until_a_real_balance_refresh() {
        let (pool, _) = test_pool("least_recent");
        pool.note_error("a", ErrorKind::HardCredit, "no credits")
            .await;
        pool.note_success("a", None).await;
        assert_eq!(
            pool.inner.lock().unwrap().statuses["a"].cool_kind,
            "hard_credit"
        );
        pool.set_balance(
            "a",
            &crate::balances::BalanceObservation {
                remaining: Some(10.0),
                total: Some(10.0),
                used: Some(0.0),
                unlimited: false,
            },
        );
        assert!(pool.inner.lock().unwrap().statuses["a"]
            .cooling_until
            .is_none());
        assert_eq!(pool.pick(&HashSet::new(), None).await.unwrap().uid, "a");
    }

    #[tokio::test]
    async fn request_policy_rejections_do_not_penalize_or_unstick_accounts() {
        for kind in [ErrorKind::ChannelRefused, ErrorKind::ContentBlocked] {
            let (pool, config) = test_pool("order");
            config.write().await.pool.breaker_threshold = 1;
            let chosen = pool
                .pick(&HashSet::new(), Some("policy-conversation"))
                .await
                .unwrap();
            assert_eq!(chosen.uid, "a");
            pool.release(&chosen.uid);
            for _ in 0..3 {
                pool.note_error("a", kind, "request rejected by upstream policy")
                    .await;
            }
            let inner = pool.inner.lock().unwrap();
            let status = &inner.statuses["a"];
            assert_eq!(status.err_total, 0);
            assert_eq!(status.consecutive_fails, 0);
            assert_eq!(status.soft_streak, 0);
            assert_eq!(status.in_flight, 0);
            assert!(status.cooling_until.is_none());
            assert!(status.cool_kind.is_empty());
            assert!(status.reason.is_empty());
            assert_eq!(inner.sticky["policy-conversation"].uid, "a");
            drop(inner);
            assert_eq!(
                pool.pick(&HashSet::new(), Some("policy-conversation"))
                    .await
                    .unwrap()
                    .uid,
                "a"
            );
        }
    }

    #[test]
    fn checkin_status_resets_at_beijing_midnight() {
        let pool = Pool::new(std::sync::Arc::new(tokio::sync::RwLock::new(
            AppConfig::default(),
        )));
        let account: AccountConfig = serde_json::from_value(serde_json::json!({
            "uid":"a", "nickname":"test", "token":"", "realm":"cn", "enabled":true, "paused":false
        }))
        .unwrap();
        let accounts = vec![account];
        let midnight = 16 * 3_600_000;
        pool.set_checkin_done_at("a", midnight - 1);
        assert!(pool.statuses_at(&accounts, midnight - 1)[0].1.checkin_done);
        assert!(!pool.statuses_at(&accounts, midnight)[0].1.checkin_done);
        pool.set_checkin_done_at("a", midnight);
        assert!(pool.statuses_at(&accounts, midnight + 1)[0].1.checkin_done);
        assert!(
            !pool.statuses_at(&accounts, midnight + 86_400_000)[0]
                .1
                .checkin_done
        );
    }

    #[test]
    fn delayed_checkin_results_cannot_mark_the_next_beijing_day_complete() {
        let (pool, _) = test_pool("order");
        let accounts = vec![account("a")];
        let midnight = 16 * 3_600_000;

        pool.set_checkin_done_for_request_at("a", midnight - 60_000, midnight - 1);
        assert!(pool.statuses_at(&accounts, midnight - 1)[0].1.checkin_done);
        // The write is anchored to the original day, including when the next read
        // happens after midnight immediately after the completion check.
        assert!(!pool.statuses_at(&accounts, midnight)[0].1.checkin_done);

        pool.set_checkin_done_for_request_at("a", midnight - 60_000, midnight + 60_000);
        assert!(
            !pool.statuses_at(&accounts, midnight + 60_000)[0]
                .1
                .checkin_done
        );
        assert_eq!(
            pool.inner.lock().unwrap().statuses["a"].checkin_day,
            Some(beijing_day(midnight - 1))
        );

        pool.set_checkin_done_for_request_at("a", midnight + 1, midnight + 2);
        assert!(pool.statuses_at(&accounts, midnight + 2)[0].1.checkin_done);
        // An old batch finishing later cannot overwrite a separate confirmation
        // already received for the new day.
        pool.set_checkin_done_for_request_at("a", midnight - 60_000, midnight + 120_000);
        assert!(
            pool.statuses_at(&accounts, midnight + 120_000)[0]
                .1
                .checkin_done
        );
        assert_eq!(
            pool.inner.lock().unwrap().statuses["a"].checkin_day,
            Some(beijing_day(midnight))
        );
    }

    #[test]
    fn a_batch_spanning_beijing_midnight_leaves_all_accounts_unconfirmed() {
        let (pool, _) = test_pool("order");
        let accounts = vec![account("a"), account("b"), account("c")];
        let midnight = 16 * 3_600_000;

        for account in &accounts {
            pool.set_checkin_done_for_request_at(
                &account.uid,
                midnight - 60_000,
                midnight + 60_000,
            );
        }
        assert!(pool
            .statuses_at(&accounts, midnight + 60_000)
            .iter()
            .all(|(_, status)| !status.checkin_done));
        assert!(pool.inner.lock().unwrap().statuses.is_empty());
    }
}
