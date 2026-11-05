//! One bounded, observable task job shared by every UI entry point.
use crate::{
    config::{AccountConfig, AppConfig},
    gateway::SharedConfig,
    pool::Pool,
    reqlog,
    tasks,
};
use futures_util::FutureExt;
use serde::Serialize;
use std::{
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, Serialize)]
pub struct JobLog {
    seq: u64,
    time: i64,
    uid: Option<String>,
    nickname: Option<String>,
    task_code: Option<String>,
    status: String,
    message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub id: String,
    pub status: String,
    started_at: i64,
    finished_at: Option<i64>,
    total: usize,
    completed: usize,
    current_uid: Option<String>,
    logs: Vec<JobLog>,
    pub result: tasks::TaskRunResult,
}

pub struct Service {
    cfg: SharedConfig,
    pool: Arc<Pool>,
    snapshot: Mutex<Option<Snapshot>>,
    account_timeout: Duration,
}

impl Service {
    pub fn new(cfg: SharedConfig, pool: Arc<Pool>) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            pool,
            snapshot: Mutex::new(None),
            account_timeout: Duration::from_secs(300),
        })
    }

    pub fn status(&self) -> Option<Snapshot> {
        self.snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub async fn start(self: &Arc<Self>, uid: Option<String>) -> Result<Snapshot, String> {
        let cfg = self.cfg.read().await;
        let accounts: Vec<_> = cfg
            .accounts
            .iter()
            .filter(|a| {
                uid.as_ref().map_or(true, |uid| &a.uid == uid)
                    && a.enabled
                    && !a.paused
                    && a.realm == "cn"
                    && !a.token.trim().is_empty()
            })
            .cloned()
            .collect();
        if accounts.is_empty() {
            return Err("没有可执行任务的国内账号，请检查账号是否启用、暂停或缺少凭证".into());
        }
        // 快照整份配置（base_url + 模型映射）：成长任务的真实对话需要用
        // models 映射把别名翻译成上游模型 id，避免执行期间持有读锁。
        let app_cfg = cfg.clone();
        drop(cfg);
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "无法创建任务 HTTP 客户端".to_string())?;
        let mut current = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        if current.as_ref().is_some_and(|job| job.status == "running") {
            return Err("已有任务作业正在执行，请在成长任务中查看进度".into());
        }
        let snapshot = Snapshot {
            id: uuid::Uuid::new_v4().to_string(),
            status: "running".into(),
            started_at: reqlog::now_ms(),
            finished_at: None,
            total: accounts.len(),
            completed: 0,
            current_uid: None,
            logs: vec![],
            result: tasks::TaskRunResult {
                items: vec![],
                total_credit: 0,
                total_energy: 0,
            },
        };
        *current = Some(snapshot.clone());
        drop(current);
        let service = self.clone();
        tokio::spawn(async move {
            // A panic must release the job just like a transport error or timeout.
            let result = AssertUnwindSafe(service.execute(client, app_cfg, accounts))
                .catch_unwind()
                .await;
            if result.is_err() {
                service.log(
                    None,
                    None,
                    "error",
                    "任务执行发生内部错误，作业已停止，可重试".into(),
                );
                service.finish(Some("failed"));
            }
        });
        Ok(snapshot)
    }

    fn log(
        &self,
        account: Option<&AccountConfig>,
        task_code: Option<String>,
        status: &str,
        message: String,
    ) {
        let mut current = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        let Some(job) = current.as_mut() else { return };
        let seq = job.logs.last().map_or(1, |entry| entry.seq + 1);
        // Guard against accidental credential echoes from upstream error messages.
        let mut message = message;
        if let Some(account) = account {
            for secret in [&account.token, &account.refresh_token] {
                if !secret.is_empty() {
                    message = message.replace(secret, "[已隐藏凭证]");
                }
            }
        }
        job.logs.push(JobLog {
            seq,
            time: reqlog::now_ms(),
            uid: account.map(|a| a.uid.clone()),
            nickname: account.map(|a| a.nickname.clone()),
            task_code,
            status: status.into(),
            message: message.chars().take(1000).collect(),
        });
        if job.logs.len() > 500 {
            job.logs.remove(0);
        }
    }

    async fn execute(&self, client: reqwest::Client, cfg: AppConfig, accounts: Vec<AccountConfig>) {
        let base = cfg.base_url.clone();
        self.log(
            None,
            None,
            "info",
            format!(
                "作业已启动，共 {} 个账号。已配置的 6 项任务将自动忽略。",
                accounts.len()
            ),
        );
        for queued in accounts {
            // Re-read credentials after earlier accounts finish, and respect a
            // user removing/pausing the next account while this job is running.
            let latest = self
                .cfg
                .read()
                .await
                .accounts
                .iter()
                .find(|a| a.uid == queued.uid)
                .cloned();
            let account = latest.filter(|a| {
                a.enabled && !a.paused && a.realm == "cn" && !a.token.trim().is_empty()
            });
            let started = reqlog::now_ms();
            self.snapshot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
                .unwrap()
                .current_uid = Some(queued.uid.clone());
            let mut item = if let Some(account) = account.as_ref() {
                self.log(Some(account), None, "running", "开始执行账号任务".into());
                let progress = |event: tasks::TaskProgress| {
                    self.log(Some(account), event.task_code, &event.status, event.message);
                };
                match tokio::time::timeout(
                    self.account_timeout,
                    tasks::run_account_with_progress(&client, &base, &cfg, account, &progress),
                )
                .await
                {
                    Ok(item) => item,
                    Err(_) => failed_item(
                        account,
                        "账号执行超时，已停止后续请求。已发生的操作请刷新任务与积分确认。",
                    ),
                }
            } else {
                failed_item(&queued, "账号已移除、暂停或凭证不可用，已跳过")
            };
            if matches!(item.checkin.as_str(), "ok" | "already") {
                self.pool.set_checkin_done(&item.uid, started);
            }
            let account = account.as_ref().unwrap_or(&queued);
            if let Some(error) = item.error.as_mut() {
                for secret in [&account.token, &account.refresh_token] {
                    if !secret.is_empty() {
                        *error = error.replace(secret, "[已隐藏凭证]");
                    }
                }
            }
            self.log(
                Some(account),
                None,
                if item.error.is_some() {
                    "error"
                } else {
                    "completed"
                },
                item.error.clone().unwrap_or_else(|| {
                    format!(
                        "账号作业结束，领取 {} 项，仍待完成 {} 项，忽略 {} 项",
                        item.claimed, item.waiting, item.skipped
                    )
                }),
            );
            let mut current = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            let job = current.as_mut().unwrap();
            job.result.total_credit += item.credit;
            job.result.total_energy += item.energy;
            job.result.items.push(item);
            job.completed += 1;
        }
        self.finish(None);
    }

    fn finish(&self, override_status: Option<&str>) {
        let mut current = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = current.as_mut() {
            let failures = job
                .result
                .items
                .iter()
                .filter(|item| item.error.is_some())
                .count();
            let partial_work = job.result.items.iter().any(|item| {
                item.error.is_none()
                    || item.claimed > 0
                    || item.completed > 0
                    || !item.checkin.is_empty() && item.checkin != "failed"
            });
            job.status = override_status
                .unwrap_or(if failures == 0 {
                    "completed"
                } else if partial_work {
                    "partial"
                } else {
                    "failed"
                })
                .into();
            job.finished_at = Some(reqlog::now_ms());
            job.current_uid = None;
        }
    }
}

fn failed_item(account: &AccountConfig, message: &str) -> tasks::TaskRunItem {
    tasks::TaskRunItem {
        uid: account.uid.clone(),
        nickname: account.nickname.clone(),
        checkin: String::new(),
        accepted: 0,
        claimed: 0,
        credit: 0,
        energy: 0,
        completed: 0,
        skipped: 0,
        waiting: 0,
        error: Some(message.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use axum::{extract::Request, routing::any, Router};
    use serde_json::json;

    fn account(uid: &str) -> AccountConfig {
        serde_json::from_value(json!({"uid":uid,"nickname":uid,"token":uid,
            "realm":"cn","enabled":true,"paused":false}))
        .unwrap()
    }

    async fn fixture(timeout: Duration) -> (Arc<Service>, tokio::task::JoinHandle<()>) {
        let app = Router::new().fallback(any(|request: Request| async move {
            if request
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                == Some("Bearer slow")
            {
                tokio::time::sleep(Duration::from_millis(350)).await;
            }
            axum::Json(json!({"code":0,"data":{"tasks":[]}}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cfg = Arc::new(tokio::sync::RwLock::new(AppConfig {
            base_url: base,
            accounts: vec![account("slow"), account("fast")],
            ..Default::default()
        }));
        let pool = Pool::new(cfg.clone());
        let mut service = Service::new(cfg, pool);
        Arc::get_mut(&mut service).unwrap().account_timeout = timeout;
        (service, server)
    }

    async fn settled(service: &Service) -> Snapshot {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = service.status().unwrap();
                if snapshot.status != "running" {
                    return snapshot;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("job failed to settle")
    }

    #[tokio::test]
    async fn timeout_releases_job_preserves_logs_and_continues_next_account() {
        let (service, server) = fixture(Duration::from_millis(100)).await;
        let started = service.start(None).await.unwrap();
        assert_eq!(started.status, "running");
        assert!(service.start(Some("fast".into())).await.is_err());
        let done = settled(&service).await;
        assert_eq!(done.id, started.id);
        assert_eq!(done.status, "partial");
        assert_eq!(done.completed, 2);
        assert!(done.finished_at.is_some());
        assert!(done.current_uid.is_none());
        assert!(done.result.items[0]
            .error
            .as_ref()
            .unwrap()
            .contains("超时"));
        assert_eq!(done.result.items[1].checkin, "ok");
        assert!(!done.logs.is_empty());
        assert!(done.logs.windows(2).all(|logs| logs[0].seq < logs[1].seq));
        assert!(service.start(Some("fast".into())).await.is_ok());
        assert_eq!(settled(&service).await.status, "completed");
        server.abort();
    }

    #[tokio::test]
    async fn paused_or_removed_accounts_do_not_execute_and_empty_selection_is_rejected() {
        let (service, server) = fixture(Duration::from_millis(100)).await;
        assert!(service.start(Some("missing".into())).await.is_err());
        service.start(None).await.unwrap();
        service
            .cfg
            .write()
            .await
            .accounts
            .retain(|a| a.uid != "fast");
        let done = settled(&service).await;
        assert_eq!(done.status, "failed");
        assert!(done.result.items[1]
            .error
            .as_ref()
            .unwrap()
            .contains("移除"));
        service.cfg.write().await.accounts[0].paused = true;
        assert!(service.start(None).await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn job_log_is_bounded_and_redacts_credentials() {
        let (service, server) = fixture(Duration::from_millis(100)).await;
        service.start(Some("fast".into())).await.unwrap();
        settled(&service).await;
        let mut account = account("safe-local-id");
        account.token = "fake-access-secret".into();
        account.refresh_token = "fake-refresh-secret".into();
        for _ in 0..550 {
            service.log(
                Some(&account),
                None,
                "error",
                "fake-access-secret fake-refresh-secret".into(),
            );
        }
        let snapshot = service.status().unwrap();
        assert_eq!(snapshot.logs.len(), 500);
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("fake-access-secret"));
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("fake-refresh-secret"));
        server.abort();
    }
}
