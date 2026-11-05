async (page) => {
  await page.getByRole('heading', { name: '用量统计', exact: true }).waitFor();
  await page.evaluate(async () => {
    const original = window.__TAURI_INTERNALS__.invoke;
    window.__browserCalls = [];
    window.__browserSessions = {};
    window.__browserSequence = 0;
    window.__browserMode = 'pending';
    window.__TAURI_INTERNALS__.invoke = async (command, args = {}) => {
      window.__browserCalls.push({ command, args });
      if (command === 'keepalive_status') return [];
      if (command === 'start_browser_login') {
        const id = `mock-browser-${++window.__browserSequence}`;
        const result = { session_id: id, url: 'https://www.codebuddy.cn/login?state=mock-only', realm: args.realm, expires_at: Date.now() + 900000, poll_interval_ms: 3000 };
        window.__browserSessions[id] = { realm: args.realm, result };
        if (window.__browserMode === 'late-start') return new Promise(resolve => { window.__resolveStart = () => resolve(result); });
        if (window.__browserMode === 'short-expiry') result.expires_at = Date.now() + 1000;
        return result;
      }
      if (command === 'cancel_browser_login') {
        window.__browserSessions[args.sessionId].cancelled = true;
        return;
      }
      if (command === 'poll_browser_login') {
        if (window.__browserMode === 'error') throw new Error('模拟授权失败（code=10001）');
        if (window.__browserMode === 'complete') {
          const account = { ...window.__previewConfig.accounts[0], uid: 'mock-authorized-uid', workbuddy_uid: 'mock-authorized-uid', nickname: '浏览器授权测试账号', token: 'mock-only-access', refresh_token: 'mock-only-refresh', login_ip: '203.0.113.48', realm: window.__browserSessions[args.sessionId].realm };
          if (!window.__previewConfig.accounts.some(item => item.uid === account.uid)) window.__previewConfig.accounts.push(account);
          return { status: 'completed', uid: account.uid, nickname: account.nickname, message: '账号已授权并加入账号池' };
        }
        return { status: 'pending', message: '等待在浏览器完成授权' };
      }
      if (command === 'plugin:opener|open_url') {
        if (window.__holdOpen) return new Promise(resolve => { window.__resolveOpen = resolve; });
        return;
      }
      return original(command, args);
    };
    const { useApp } = await import('/src/stores/appStore.ts');
    window.__previewConfig.accounts[0].login_ip = '2001:db8:1234:5678:abcd:ef01:2345:6789';
    useApp.setState({ config: structuredClone(window.__previewConfig) });
  });
  await page.getByRole('button', { name: '账号保险柜', exact: true }).click();
  await page.getByRole('button', { name: '＋ 添加账号', exact: true }).click();
  if (await page.getByRole('tab', { name: '浏览器登录' }).getAttribute('aria-selected') !== 'true') throw new Error('browser login is not the default');
  await page.screenshot({ path: 'output/playwright/browser-login-desktop.png' });
  await page.getByRole('button', { name: '获取授权链接', exact: true }).click();
  await page.getByRole('textbox', { name: '浏览器授权链接' }).waitFor();
  await page.waitForTimeout(3300);
  const polls = await page.evaluate(() => window.__browserCalls.filter(call => call.command === 'poll_browser_login').length);
  if (polls !== 1) throw new Error(`expected one nonoverlapping 3-second poll, got ${polls}`);
  await page.evaluate(() => { window.__browserMode = 'complete'; });
  await page.getByRole('heading', { name: '浏览器授权测试账号', exact: true }).waitFor({ timeout: 10000 });
  const card = page.locator('article').filter({ has: page.getByRole('heading', { name: '浏览器授权测试账号', exact: true }) });
  if (!await card.getByText('203.0.113.48', { exact: true }).isVisible()) throw new Error('observed login IP was not displayed');
  if (await page.getByRole('tabpanel', { name: '浏览器登录' }).count()) throw new Error('completed login panel did not close');
  await page.screenshot({ path: 'output/playwright/browser-login-imported.png' });
  await page.evaluate(() => { window.__browserMode = 'pending'; });
  await page.getByRole('button', { name: '＋ 添加账号', exact: true }).click();
  await page.getByRole('radio', { name: '国际版 Global' }).check();
  await page.getByRole('button', { name: '获取授权链接', exact: true }).click();
  await page.getByRole('textbox', { name: '浏览器授权链接' }).waitFor();
  const active = await page.evaluate(() => `mock-browser-${window.__browserSequence}`);
  await page.evaluate(() => { window.__holdOpen = true; });
  await page.getByRole('button', { name: '打开浏览器', exact: true }).click();
  await page.getByRole('button', { name: '正在打开…', exact: true }).waitFor();
  await page.getByRole('radio', { name: '国内版 CN' }).check();
  await page.getByRole('button', { name: '获取授权链接', exact: true }).click();
  await page.getByRole('textbox', { name: '浏览器授权链接' }).waitFor();
  if (!await page.getByRole('button', { name: '打开浏览器', exact: true }).isEnabled()) throw new Error('old opener left the next session permanently busy');
  await page.evaluate(() => { window.__holdOpen = false; window.__resolveOpen?.(); });
  if (!await page.evaluate(id => window.__browserSessions[id].cancelled, active)) throw new Error('realm switch did not cancel old authorization');
  await page.getByRole('tab', { name: '手动导入' }).click();
  const cancelled = await page.evaluate(() => window.__browserSessions[`mock-browser-${window.__browserSequence}`].cancelled);
  if (!cancelled) throw new Error('switching to manual import did not cancel authorization');
  await page.getByRole('tab', { name: '浏览器登录' }).click();
  await page.evaluate(() => { window.__browserMode = 'late-start'; });
  await page.getByRole('button', { name: '获取授权链接', exact: true }).click();
  await page.waitForFunction(() => !!window.__resolveStart);
  await page.getByRole('button', { name: '关闭导入', exact: true }).click();
  await page.evaluate(() => window.__resolveStart());
  await page.waitForFunction(() => window.__browserSessions[`mock-browser-${window.__browserSequence}`].cancelled);
  await page.getByRole('button', { name: '＋ 添加账号', exact: true }).click();
  await page.evaluate(() => { window.__browserMode = 'short-expiry'; });
  await page.getByRole('button', { name: '获取授权链接', exact: true }).click();
  await page.getByRole('button', { name: '重新获取授权链接', exact: true }).waitFor({ timeout: 7000 });
  await page.getByRole('button', { name: '关闭导入', exact: true }).click();
  await page.getByRole('button', { name: '＋ 添加账号', exact: true }).click();
  await page.evaluate(() => { window.__browserMode = 'error'; });
  await page.getByRole('button', { name: '获取授权链接', exact: true }).click();
  await page.getByText('模拟授权失败（code=10001）', { exact: true }).waitFor({ timeout: 7000 });
  await page.getByRole('button', { name: '关闭导入', exact: true }).click();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole('button', { name: '＋ 添加账号', exact: true }).click();
  await page.screenshot({ path: 'output/playwright/browser-login-mobile.png', fullPage: true });
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth);
  if (overflow) throw new Error('authorization panel or long IPv6 overflows the narrow viewport');
  const calls = await page.evaluate(() => window.__browserCalls);
  if (calls.some(call => ['switch_account', 'refresh_workbuddy_token', 'checkin_one', 'save_config'].includes(call.command))) throw new Error('browser login UI invoked unrelated account mutations');
  console.log(JSON.stringify({ ok: true, polls, starts: calls.filter(call => call.command === 'start_browser_login').length, cancellations: calls.filter(call => call.command === 'cancel_browser_login').length, narrowViewportOverflow: overflow }));
}
