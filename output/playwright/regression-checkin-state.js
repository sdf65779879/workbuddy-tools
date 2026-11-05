async (page) => {
  await page.clock.setFixedTime(new Date('2026-11-05T15:59:00Z'));
  const checks = [];
  const assert = (condition, message) => { if (!condition) throw new Error(message); };
  const navigate = (name) => page.getByRole('button', { name, exact: true }).click();
  const card = (name) => page.locator('.wb-account-card').filter({ has: page.getByRole('heading', { name, exact: true }) });
  const first = card('测试已签到账号');
  await page.evaluate(() => {
    for (const key of Object.keys(localStorage)) if (key.startsWith('wb-checkin-date:checkin-fixture-')) localStorage.removeItem(key);
    window.dispatchEvent(new Event('wb-pool-updated'));
  });
  for (const name of ['账号保险柜', '运维·签到积分', '成长任务', 'OpenAI 网关', '分享·日志', '用量统计']) {
    await navigate(name);
    assert(await page.locator('.wb-page-kicker').count() === 0, `${name}: English heading remains`);
  }
  checks.push('all six decorative English headings removed');
  await navigate('账号保险柜');
  await first.getByText('中国大陆 · 今日待签到', { exact: true }).waitFor();
  await page.evaluate(() => { window.__qaMode = 'failed'; });
  await first.getByRole('button', { name: '签到', exact: true }).click();
  await page.getByRole('alertdialog').waitFor();
  assert(!await page.evaluate(() => localStorage.getItem('wb-checkin-date:checkin-fixture-1')), 'failed check-in persisted completion');
  await page.getByRole('button', { name: '知道了', exact: true }).click();
  assert(await first.getByRole('button', { name: '签到', exact: true }).isEnabled(), 'failed check-in cannot retry');
  checks.push('failed check-in stays pending and retryable');

  await page.evaluate(() => { window.__qaMode = 'already'; });
  await first.getByRole('button', { name: '签到', exact: true }).click();
  await first.getByText('中国大陆 · 今日已签到', { exact: true }).waitFor();
  assert(await first.getByRole('button', { name: '已签到', exact: true }).isDisabled(), 'already checked-in button still active');
  assert(await page.evaluate(() => localStorage.getItem('wb-checkin-date:checkin-fixture-1')) === '2026-11-05', 'already check-in not cached in Beijing day');
  checks.push('already response immediately updates status and disables duplicate button');
  await page.screenshot({ path: 'output/playwright/checkin-fixed-desktop.png' });

  await navigate('运维·签到积分');
  await page.getByRole('button', { name: '执行签到', exact: true }).click();
  await page.getByText('操作完成 · 3 个成功，0 个失败或跳过', { exact: true }).waitFor();
  assert(await page.locator('.wb-result-row').count() === 3, 'batch check-in includes unsupported international account');
  assert(await page.locator('.wb-result-row').filter({ hasText: '今日已签到' }).count() === 3, 'batch already message is untranslated');
  checks.push('batch check-in updates all domestic accounts and renders Chinese already message');

  await page.reload();
  await navigate('账号保险柜');
  await first.getByText('中国大陆 · 今日已签到', { exact: true }).waitFor();
  assert(await page.getByText('中国大陆 · 今日已签到', { exact: true }).count() === 3, 'reload lost remembered completion with empty backend state');
  await card('测试国际账号').getByText('国际区域 · 暂不支持签到', { exact: true }).waitFor();
  assert(await card('测试国际账号').getByRole('button', { name: '签到', exact: true }).isDisabled(), 'international check-in enabled');
  checks.push('reload preserves confirmed day after backend state reset; international account has accurate status');

  await page.evaluate(() => {
    window.__qaMode = 'task-partial';
    localStorage.removeItem('wb-checkin-date:checkin-fixture-3');
    window.dispatchEvent(new Event('wb-pool-updated'));
  });
  const taskCard = card('测试成长任务账号');
  await taskCard.getByText('中国大陆 · 今日待签到', { exact: true }).waitFor();
  await taskCard.getByRole('button', { name: '任务', exact: true }).click();
  await taskCard.getByRole('button', { name: '执行并领奖', exact: true }).click();
  await page.getByRole('alertdialog').waitFor();
  assert(await page.evaluate(() => localStorage.getItem('wb-checkin-date:checkin-fixture-3')) === '2026-11-05', 'partial growth error lost successful check-in');
  await page.keyboard.press('Escape');
  await taskCard.getByText('中国大陆 · 今日已签到', { exact: true }).waitFor();
  checks.push('growth failure still preserves successful check-in and opens central error');

  await page.evaluate(() => { window.__qaMode = 'already'; });
  await page.clock.setFixedTime(new Date('2026-11-05T16:01:00Z'));
  await page.clock.fastForward(120000);
  await page.evaluate(() => window.dispatchEvent(new Event('wb-pool-updated')));
  await first.getByText('中国大陆 · 今日待签到', { exact: true }).waitFor();
  assert(await first.getByRole('button', { name: '签到', exact: true }).isEnabled(), 'next Beijing day did not reset check-in');
  checks.push('Beijing midnight expires completion and re-enables check-in');

  const midnightRace = await page.evaluate(async () => {
    const { poolStatus } = await import('/src/lib/commands.ts');
    const original = window.__TAURI_INTERNALS__.invoke;
    const realNow = Date.now;
    let now = Date.parse('2026-11-06T15:59:59.999Z');
    Date.now = () => now;
    window.__TAURI_INTERNALS__.invoke = async (command, args) => {
      if (command !== 'pool_status') return original(command, args);
      now = Date.parse('2026-11-06T16:00:00.001Z');
      return [{ uid: 'checkin-midnight-race', checkin_done: true }];
    };
    try {
      const states = await poolStatus();
      return !states[0].checkin_done && !localStorage.getItem('wb-checkin-date:checkin-midnight-race');
    } finally { Date.now = realNow; window.__TAURI_INTERNALS__.invoke = original; }
  });
  assert(midnightRace, 'stale pre-midnight pool response marked next day complete');
  checks.push('response crossing Beijing midnight cannot persist stale completion');
  await page.evaluate((results) => { window.__checkinStateResults = results; }, checks);
  console.log(JSON.stringify({ passed: checks.length, checks }));
}
