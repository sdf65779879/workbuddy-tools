async (page) => {
  const checks = [];
  const pageErrors = [];
  page.on('pageerror', error => pageErrors.push(error.message));
  const assert = (condition, label) => { if (!condition) throw new Error(label); checks.push(label); };
  await page.getByRole('heading', { name: '用量统计', exact: true }).waitFor();
  await page.evaluate(() => {
    window.__centeredErrorsOriginal ??= window.__TAURI_INTERNALS__.invoke;
    window.__centeredErrorCase = '';
    window.__TAURI_INTERNALS__.invoke = async (command, args = {}) => {
      const mode = window.__centeredErrorCase;
      if (command === 'keepalive_status') return [];
      if (mode === 'overview' && command === 'usage_dashboard') throw new Error('QA 用量统计读取失败');
      if (mode === 'share' && command === 'reqlog_read') throw new Error('QA 日志读取失败');
      if (mode === 'gateway' && command === 'test_connection') return { ok: false, status: 401, latency_ms: 1, content: '', error: 'QA 上游凭证无效' };
      if (mode === 'vault' && command === 'switch_account') return { ok: false, uid: args.uid, nickname: '虚构测试账号', auth_file: '', message: 'QA 切换账号失败' };
      const result = await window.__centeredErrorsOriginal(command, args);
      if ((mode === 'header' || mode === 'long') && command === 'refresh_balances') return result.map((item, index) => index ? item : {
        ...item, error: mode === 'long' ? Array.from({ length: 120 }, (_, i) => `QA 长错误 ${i + 1}：虚构账号的接口暂时不可用，请稍后重试。`).join('\n') : 'QA 积分读取失败',
      });
      if (mode === 'checkin' && command === 'checkin_one') return { ...result, checkin: '', error: 'QA 批量签到请求失败' };
      if (mode === 'growth' && command === 'task_run_all') return { ...result, items: result.items.map((item, index) => index < 2 ? { ...item, error: `QA 成长任务失败 ${index + 1}` } : item) };
      return result;
    };
  });
  const mode = value => page.evaluate(value => { window.__centeredErrorCase = value; }, value);
  const verify = async (message, label) => {
    const dialog = page.getByRole('alertdialog');
    await dialog.waitFor({ state: 'visible' });
    assert((await dialog.innerText()).includes(message), `${label}: failure text visible`);
    const metrics = await dialog.evaluate(element => {
      const bounds = element.getBoundingClientRect();
      return { x: bounds.x, y: bounds.y, w: bounds.width, h: bounds.height, vw: innerWidth, vh: innerHeight,
        modal: element.matches(':modal'), focused: element.contains(document.activeElement) };
    });
    assert(Math.abs(metrics.x + metrics.w / 2 - metrics.vw / 2) < 1.5 && Math.abs(metrics.y + metrics.h / 2 - metrics.vh / 2) < 1.5, `${label}: dialog centered in viewport`);
    assert(metrics.modal && metrics.focused, `${label}: native modal layer and focus`);
    assert(await page.getByRole('alertdialog').count() === 1, `${label}: one error dialog`);
    assert(await page.locator('.wb-toast').count() === 0, `${label}: no success notification over failure`);
    return dialog;
  };
  const dismiss = async (how = 'confirm') => {
    if (how === 'esc') await page.keyboard.press('Escape');
    else if (how === 'close') await page.getByRole('button', { name: '关闭错误提示', exact: true }).click();
    else await page.getByRole('button', { name: '知道了', exact: true }).click();
    await page.getByRole('alertdialog').waitFor({ state: 'detached' });
    checks.push(`dismiss with ${how}`);
  };

  await mode('header');
  await page.evaluate(() => window.scrollTo(0, 700));
  await page.getByRole('button', { name: '刷新账号实际额度', exact: true }).click();
  await verify('QA 积分读取失败', 'header balance refresh');
  await page.screenshot({ path: 'output/playwright/errors-centered-desktop.png', fullPage: false });
  await dismiss('esc');
  await mode('');
  await page.getByRole('button', { name: '刷新账号实际额度', exact: true }).click();
  await page.waitForFunction(() => !document.querySelector('.wb-balance-error'));

  await page.getByRole('button', { name: '账号保险柜', exact: true }).click();
  await mode('vault');
  await page.getByRole('button', { name: '切换账号', exact: true }).first().click();
  await verify('QA 切换账号失败', 'vault switch ok:false');
  await dismiss('close');

  await mode('');
  await page.getByRole('button', { name: '运维·签到积分', exact: true }).click();
  await mode('checkin');
  await page.getByRole('button', { name: '执行签到', exact: true }).click();
  const checkin = await verify('QA 批量签到请求失败', 'batch checkin returned errors');
  assert((await checkin.innerText()).includes('备用账号'), 'batch checkin: failures aggregated across accounts');
  await dismiss();

  await mode('');
  await page.getByRole('button', { name: '成长任务', exact: true }).click();
  await page.getByRole('button', { name: '执行并自动领奖', exact: true }).waitFor();
  await mode('growth');
  await page.getByRole('button', { name: '执行并自动领奖', exact: true }).click();
  const growth = await verify('QA 成长任务失败 1', 'growth batch item.error');
  assert((await growth.innerText()).includes('QA 成长任务失败 2'), 'growth: multiple account failures aggregated');
  await dismiss();

  await mode('');
  await page.getByRole('button', { name: 'OpenAI 网关', exact: true }).click();
  await mode('gateway');
  await page.getByRole('button', { name: '测试上游连接', exact: true }).click();
  await verify('QA 上游凭证无效', 'gateway test ok:false');
  await dismiss();

  await mode('');
  await page.getByRole('button', { name: '分享·日志', exact: true }).click();
  await page.getByRole('button', { name: '刷新日志', exact: true }).waitFor();
  await mode('share');
  await page.getByRole('button', { name: '刷新日志', exact: true }).click();
  await verify('QA 日志读取失败', 'share manual logs refresh');
  await dismiss();

  await mode('');
  await page.getByRole('button', { name: '用量统计', exact: true }).click();
  await page.getByRole('button', { name: '刷新数据', exact: true }).waitFor();
  await mode('overview');
  await page.getByRole('button', { name: '刷新数据', exact: true }).click();
  await verify('QA 用量统计读取失败', 'overview manual refresh');
  await dismiss();

  await mode('');
  await page.getByRole('button', { name: '切换深色外观', exact: true }).click();
  await mode('header');
  await page.getByRole('button', { name: '刷新账号实际额度', exact: true }).click();
  const dark = await verify('QA 积分读取失败', 'dark theme');
  assert(await dark.evaluate(element => getComputedStyle(element).backgroundColor) === 'rgb(35, 52, 44)', 'dark theme: dedicated readable surface');
  await page.screenshot({ path: 'output/playwright/errors-centered-dark.png', fullPage: false });
  await dismiss();
  await page.getByRole('button', { name: '切换浅色外观', exact: true }).click();

  await page.setViewportSize({ width: 390, height: 844 });
  await mode('long');
  await page.getByRole('button', { name: '刷新账号实际额度', exact: true }).click();
  const long = await verify('QA 长错误 120', '390px long failure');
  const scroll = await long.locator('.wb-error-message').evaluate(element => ({ client: element.clientHeight, scroll: element.scrollHeight }));
  assert(scroll.scroll > scroll.client && scroll.client > 100, '390px long failure: message scrolls inside dialog');
  const bounds = await long.boundingBox();
  assert(bounds.x >= 15 && bounds.x + bounds.width <= 375 && bounds.y >= 23 && bounds.y + bounds.height <= 821, '390px long failure: dialog fits viewport');
  await long.locator('.wb-error-message').evaluate(element => { element.scrollTop = element.scrollHeight; });
  await page.screenshot({ path: 'output/playwright/errors-centered-mobile-long.png', fullPage: false });
  await dismiss();
  await mode('');
  await page.setViewportSize({ width: 1440, height: 1080 });
  assert(!pageErrors.length, `no uncaught page errors (${pageErrors.join('; ')})`);
  await page.evaluate(checks => { window.__centeredErrorChecks = checks; }, checks);
  console.log(JSON.stringify({ passed: checks.length, checks }));
}
