async (page) => {
  await page.evaluate(async () => {
    const { useApp } = await import('/src/stores/appStore.ts');
    const original = window.__TAURI_INTERNALS__.invoke;
    window.__ipResult = { ip: '203.0.113.90', observed_at: 1793872800000 };
    window.__TAURI_INTERNALS__.invoke = async (command, args = {}) => {
      if (command === 'keepalive_status') return [];
      if (command === 'observe_import_ip') return window.__ipResult;
      return original(command, args);
    };
    window.__previewConfig.accounts[0].login_ip = '203.0.113.42';
    window.__previewConfig.accounts[0].import_ip = window.__ipResult;
    window.__previewConfig.accounts[1].import_ip = window.__ipResult;
    useApp.setState({ config: structuredClone(window.__previewConfig), error: null });
  });
  await page.getByRole('button', { name: '账号保险柜', exact: true }).click();
  const cards = page.locator('article');
  if (!await cards.nth(0).locator('.wb-login-ip').getByText('203.0.113.42', { exact: true }).isVisible()) throw new Error('official IP did not take priority');
  if (!await cards.nth(1).locator('.wb-login-ip').getByText('导入出口 IP', { exact: true }).isVisible()) throw new Error('egress mislabeled as login IP');
  if (!await cards.nth(1).getByText('本机出口 · 非远程登录者 IP', { exact: true }).isVisible()) throw new Error('remote user caveat missing');
  if (!await cards.nth(2).locator('.wb-login-ip').getByText('未获取', { exact: true }).isVisible()) throw new Error('unknown IP not preserved');

  await page.getByRole('button', { name: '本机导入', exact: true }).click();
  let dialog = page.getByRole('dialog');
  await dialog.getByRole('button', { name: '确认导入', exact: true }).click();
  await dialog.waitFor({ state: 'detached' });
  const saved = await page.evaluate(() => window.__previewConfig.accounts.find(a => a.uid === 'preview-local'));
  if (saved.login_ip || saved.import_ip?.ip !== '203.0.113.90') throw new Error('local import did not persist separate egress');

  await page.getByRole('button', { name: '＋ 添加账号', exact: true }).click();
  dialog = page.getByRole('dialog');
  await dialog.getByText('优先读取官方登录 IP；未返回时通过 ipify 记录本机导入出口 IP。将链接发给他人登录时，本机出口无法代表对方 IP。').waitFor();
  await dialog.getByRole('tab', { name: '手动导入', exact: true }).click();
  await page.evaluate(() => { window.__ipResult = null; });
  await dialog.getByLabel('账号昵称', { exact: true }).fill('查询失败仍可导入');
  await dialog.getByPlaceholder('填写与 Access Token 对应的真实账号 UID').fill('mock-ip-failure');
  await dialog.getByPlaceholder('粘贴 Token 或 Bearer Token').fill('mock-ip-failure-access');
  await dialog.getByRole('button', { name: '确认导入', exact: true }).click();
  await dialog.waitFor({ state: 'detached' });
  const failed = await page.evaluate(() => window.__previewConfig.accounts.find(a => a.uid === 'mock-ip-failure'));
  if (!failed || failed.login_ip || failed.import_ip) throw new Error('failed IP query prevented import or invented an IP');
  await page.screenshot({ path: 'output/playwright/import-ip-desktop.png' });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: 'output/playwright/import-ip-mobile.png' });
  if (await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)) throw new Error('mobile overflow');
  console.log('PASS official-IP priority, explicit egress label, unknown state, local import persistence, failure-tolerant manual import, mobile layout');
}
