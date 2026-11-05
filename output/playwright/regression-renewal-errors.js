async (page) => {
  const checks = [];
  const assert = (condition, label) => { if (!condition) throw new Error(label); checks.push(label); };
  await page.getByRole('heading', { name: '用量统计', exact: true }).waitFor();
  await page.evaluate(async () => {
    const { useApp } = await import('/src/stores/appStore.ts');
    const original = window.__TAURI_INTERNALS__.invoke;
    window.__renewalFixtureStatuses = [];
    window.__renewalFixtureReadFailure = false;
    window.__TAURI_INTERNALS__.invoke = async (command, args) => {
      if (command === 'keepalive_status') {
        if (window.__renewalFixtureReadFailure) throw new Error('fixture status read failed');
        return window.__renewalFixtureStatuses;
      }
      return original(command, args);
    };
    useApp.setState({ keepaliveStatuses: [], error: null, notice: null });
  });
  const status = (overrides = {}) => ({ uid: 'checkin-fixture-1', in_progress: false, last_attempt_at: null,
    last_success_at: null, next_attempt_at: null, consecutive_failures: 0, error: null, ...overrides });
  const sync = async (statuses) => page.evaluate(async (statuses) => {
    const { useApp } = await import('/src/stores/appStore.ts');
    window.__renewalFixtureStatuses = statuses;
    await useApp.getState().syncKeepaliveStatus();
    return { error: useApp.getState().error, notice: useApp.getState().notice, readError: useApp.getState().keepaliveError };
  }, statuses);
  const dismiss = async () => {
    await page.getByRole('alertdialog').waitFor();
    await page.keyboard.press('Escape');
    await page.getByRole('alertdialog').waitFor({ state: 'detached' });
  };

  let result = await sync([status({ error: '缺少 Refresh Token，尚未执行续期' })]);
  assert(!result.error, 'unattempted missing credentials do not open execution error');
  result = await sync([status({ in_progress: true, last_attempt_at: 1000, error: 'previous failure' })]);
  assert(!result.error, 'in-progress attempt with retained old error does not open error');
  await page.evaluate(async () => {
    const { useApp } = await import('/src/stores/appStore.ts');
    useApp.getState().setNotice('fixture success notice');
  });
  result = await sync([status({ last_attempt_at: 1000, error: 'previous failure', consecutive_failures: 1 })]);
  assert(result.error?.includes('测试已签到账号：previous failure'), 'completion after observed in-progress state reports failure');
  assert(result.notice === null, 'renewal failure clears success notice');
  const dialog = page.getByRole('alertdialog');
  await dialog.waitFor();
  const centered = await dialog.evaluate(element => {
    const bounds = element.getBoundingClientRect();
    return Math.abs(bounds.x + bounds.width / 2 - innerWidth / 2) < 1.5 && Math.abs(bounds.y + bounds.height / 2 - innerHeight / 2) < 1.5;
  });
  assert(centered, 'automatic renewal failure uses viewport-centered dialog');
  await dismiss();
  for (let i = 0; i < 3; i++) {
    result = await sync([status({ last_attempt_at: 1000, error: 'previous failure', consecutive_failures: 1 })]);
    assert(!result.error, `unchanged failed attempt does not reopen on poll ${i + 1}`);
  }
  result = await sync([status({ last_attempt_at: 2000, error: 'previous failure', consecutive_failures: 2 })]);
  assert(result.error?.includes('previous failure'), 'next failed attempt reopens even with identical message');
  await dismiss();
  result = await sync([status({ in_progress: true, last_attempt_at: 3000, error: 'previous failure', consecutive_failures: 2 })]);
  assert(!result.error, 'next in-progress retry remains silent');
  result = await sync([status({ last_attempt_at: 3000, error: 'previous failure', consecutive_failures: 3 })]);
  assert(result.error?.includes('previous failure'), 'retry completion with same timestamp as observed progress reports failure');
  await dismiss();
  result = await sync([status({ last_attempt_at: 4000, last_success_at: 4001, error: '' })]);
  assert(!result.error, 'successful completion with empty backend error remains silent');
  result = await sync([
    status({ last_attempt_at: 5000, error: 'first account failed', consecutive_failures: 1 }),
    status({ uid: 'checkin-fixture-2', last_attempt_at: 5000, error: 'second account failed', consecutive_failures: 1 }),
  ]);
  assert(result.error?.includes('测试已签到账号：first account failed') && result.error?.includes('测试重试账号：second account failed'), 'new failures from multiple accounts aggregate in one dialog');
  await dismiss();
  await page.evaluate(() => { window.__renewalFixtureReadFailure = true; });
  result = await sync([]);
  assert(!result.error && result.readError?.includes('fixture status read failed'), 'status read failure remains status information rather than execution error');
  await page.evaluate((checks) => { window.__renewalErrorResults = checks; }, checks);
  console.log(JSON.stringify({ passed: checks.length, checks }));
}
