mod account_switch;
mod apikey;
mod balances;
mod browser_auth;
mod commands;
mod config;
mod gateway;
mod growth_auto;
mod import_ip;
mod pool;
mod renewal;
mod reqlog;
mod task_jobs;
mod tasks;
mod usage;
mod workbuddy_auth_crypto;

use commands::AppState;
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;

pub fn run() {
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "workbuddy_codex_gateway_lib=info,axum=warn".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    let cfg = config::load_config().unwrap_or_default();
    let shared_cfg: gateway::SharedConfig = Arc::new(RwLock::new(cfg.clone()));

    let keys = Arc::new(apikey::Store::new_or_empty(
        config::apikeys_path().display().to_string(),
    ));
    let pool = pool::Pool::new(shared_cfg.clone());
    let reqlog = reqlog::Recorder::new(reqlog::ArchiveConfig {
        enabled: cfg.logging.archive_enabled,
        dir: config::request_log_dir(),
        retention_days: cfg.logging.retention_days,
        max_bytes: cfg.logging.archive_max_mb.saturating_mul(1024 * 1024),
    });
    let usage = Arc::new(usage::UsageRecorder::new(
        config::usage_path().display().to_string(),
    ));
    usage.start_persistence();
    let usage_on_exit = usage.clone();
    let account_refreshes = Arc::new(Mutex::default());
    let renewal = renewal::Service::new(shared_cfg.clone(), account_refreshes.clone());
    renewal.start();
    let browser_auth = browser_auth::Service::new(shared_cfg.clone(), account_refreshes.clone());

    let task_jobs = task_jobs::Service::new(shared_cfg.clone(), pool.clone());

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            shared_cfg,
            account_refreshes,
            renewal,
            browser_auth,
            task_jobs,
            gateway: Mutex::new(None),
            keys,
            pool,
            reqlog,
            usage,
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::observe_import_ip,
            commands::save_config,
            commands::test_connection,
            commands::start_gateway,
            commands::stop_gateway,
            commands::gateway_status,
            commands::extract_token_from_logs,
            commands::list_api_keys,
            commands::create_api_key,
            commands::update_api_key,
            commands::delete_api_key,
            commands::pool_status,
            commands::refresh_balances,
            commands::usage_snapshot,
            commands::usage_dashboard,
            commands::reqlog_snapshot,
            commands::reqlog_read,
            commands::task_scan_all,
            commands::start_task_job,
            commands::task_job_status,
            commands::task_run_all,
            commands::task_run_one,
            commands::checkin_one,
            commands::keepalive_one,
            commands::get_data_dir,
            commands::switch_account,
            commands::read_current_workbuddy_token,
            commands::read_current_workbuddy_account,
            commands::refresh_workbuddy_token,
            commands::keepalive_status,
            commands::start_browser_login,
            commands::poll_browser_login,
            commands::cancel_browser_login,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(move |_, event| {
            if let tauri::RunEvent::Exit = event {
                usage_on_exit.flush();
            }
        });
}
