async (page) => {
  await page.clock.install({ time: new Date('2026-11-05T15:59:00Z') });
  await page.addInitScript(() => {
    window.__qaMode = 'already';
    window.__qaCalls = [];
    window.__TAURI_INTERNALS__ = { invoke: async (command, args = {}) => {
      window.__qaCalls.push({ command, uid: args.uid });
      const { DEFAULT_CONFIG } = await import('/src/types.ts');
      const accounts = ['测试已签到账号', '测试重试账号', '测试成长任务账号', '测试国际账号'].map((nickname, index) => ({
        uid: `checkin-fixture-${index + 1}`, nickname, workbuddy_uid: `checkin-fixture-${index + 1}`,
        token: 'fixture-access-only', refresh_token: 'fixture-refresh-only', email: '', enterprise_id: '', enterprise_name: '', domain: '',
        expires_at: null, realm: index === 3 ? 'global' : 'cn', enabled: true, paused: false,
      }));
      window.__qaConfig ??= { ...DEFAULT_CONFIG, accounts, auto_start: false, schedule: { ...DEFAULT_CONFIG.schedule, checkin_enabled: false, balance_refresh_minutes: 0 } };
      const config = window.__qaConfig;
      const status = { running: false, port: 15722, base_url: 'http://127.0.0.1:15722/v1', models: [] };
      const item = (uid) => ({ uid, nickname: accounts.find((account) => account.uid === uid)?.nickname || uid,
        checkin: window.__qaMode === 'failed' ? 'failed' : window.__qaMode === 'ok' ? 'ok' : 'already',
        accepted: 0, claimed: 0, credit: 0, energy: 0,
        error: window.__qaMode === 'failed' ? 'HTTP 400 (code=10001)：token expired' : window.__qaMode === 'task-partial' ? '领取奖励失败：测试网络故障' : null });
      if (command === 'get_config') return config;
      if (command === 'save_config') return window.__qaConfig = args.cfg;
      if (command === 'gateway_status' || command === 'stop_gateway') return status;
      if (command === 'keepalive_status') return [];
      if (command === 'pool_status') return accounts.map((account) => ({ ...account, token_len: 19, credits: 1000, remaining_credits: 1000, unlimited: false,
        cooling: false, cool_kind: '', cool_remaining_secs: 0, reason: '', soft_streak: 0, consecutive_fails: 0, success_count: 0, err_total: 0, in_flight: 0, checkin_done: false }));
      if (command === 'refresh_balances') return accounts.map((account) => ({ uid: account.uid, nickname: account.nickname, credits: 1000,
        remaining_credits: 1000, total_credits: 1200, used_credits: 200, unlimited: false, fetched_at: Date.now(), error: null }));
      if (command === 'checkin_one' || command === 'task_run_one') return item(args.uid);
      if (command === 'task_run_all') return { items: accounts.filter((account) => account.realm === 'cn').map((account) => item(account.uid)), total_credit: 0, total_energy: 0 };
      if (command === 'task_scan_all') return accounts.filter((account) => account.realm === 'cn').map((account) => ({ uid: account.uid, nickname: account.nickname, tasks: [], error: null }));
      if (command === 'usage_dashboard' || command === 'usage_snapshot') return { saved_at: Date.now(), total_requests: 0, total_errors: 0,
        prompt_tokens: 0, completion_tokens: 0, total_tokens: 0, avg_latency_ms: 0, total_credit: 0, by_model: [], by_account: [], series: [],
        window_from_ms: Date.now(), window_to_ms: Date.now(), granularity: 'hour', success_rate: 0, avg_ttfb_ms: null, avg_tokens_per_second: null,
        cache_hit_tokens: 0, cache_miss_tokens: 0, cache_hit_rate: null, live_rpm: 0, live_tpm: 0 };
      if (command === 'reqlog_snapshot') return { started_at_ms: Date.now(), completed: 0, received: 0, in_flight: 0, succeeded: 0, failed: 0,
        success_rate: 0, http_success_rate: 0, avg_duration_ms: 0, recent: [], archive: { enabled: false, dir: '', files: 0, bytes: 0, dropped_writes: 0, last_error: '' } };
      if (command === 'reqlog_read' || command === 'list_api_keys') return [];
      if (command === 'get_data_dir') return 'C:/FixtureOnly';
      throw new Error(`Unmocked fixture command: ${command}`);
    } };
    for (const key of Object.keys(localStorage)) {
      if (key.startsWith('wb-checkin-date:') && !key.startsWith('wb-checkin-date:checkin-fixture-')) localStorage.removeItem(key);
    }
  });
  await page.setViewportSize({ width: 1440, height: 1080 });
  await page.reload();
}
