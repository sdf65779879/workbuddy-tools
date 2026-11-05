async (page) => {
  await page.getByRole('heading', { name: '用量统计', exact: true }).waitFor();
  await page.evaluate(() => {
    const original = window.__TAURI_INTERNALS__.invoke;
    window.__loginDialogCalls = [];
    window.__loginDialogSession = 0;
    window.__loginDialogPolls = 0;
    window.__loginDialogReady = false;
    window.__TAURI_INTERNALS__.invoke = async (command, args = {}) => {
      window.__loginDialogCalls.push({ command, args });
      if (command === 'keepalive_status') return [];
      if (command === 'start_browser_login') return {
        session_id: `dialog-test-${++window.__loginDialogSession}`,
        url: 'https://www.codebuddy.cn/login?state=mock-waiting-only', realm: args.realm,
        expires_at: Date.now() + 900000, poll_interval_ms: 3000,
      };
      if (command === 'poll_browser_login') {
        window.__loginDialogPolls++;
        if (!window.__loginDialogReady) return { status: 'pending', message: '等待在浏览器完成授权' };
        const account = { ...window.__previewConfig.accounts[0], uid: 'modal-login-user', workbuddy_uid: 'modal-login-user', nickname: '弹窗授权测试账号', token: 'fake-access-only', refresh_token: 'fake-refresh-only', login_ip: '203.0.113.42' };
        if (!window.__previewConfig.accounts.some(item => item.uid === account.uid)) window.__previewConfig.accounts.push(account);
        return { status: 'completed', uid: account.uid, nickname: account.nickname };
      }
      if (command === 'cancel_browser_login' || command === 'plugin:opener|open_url') return;
      if (command === 'save_config' && window.__holdDeleteSave) {
        return new Promise((_, reject) => { window.__rejectDeleteSave = reject; });
      }
      return original(command, args);
    };
  });
  await page.getByRole('button', { name: '账号保险柜', exact: true }).click();
  const add = page.getByRole('button', { name: '＋ 添加账号', exact: true });
  const before = await page.locator('article').first().boundingBox();
  await add.click();
  const dialog = page.getByRole('dialog');
  await dialog.waitFor();
  if (!await dialog.evaluate(element => element.matches(':modal'))) throw new Error('add-account dialog is not modal');
  const after = await page.locator('article').first().boundingBox();
  if (Math.abs(before.y - after.y) > 1) throw new Error('opening the dialog shifted the account cards');
  const desktop = await dialog.boundingBox();
  const viewport = page.viewportSize();
  if (Math.abs(desktop.x + desktop.width / 2 - viewport.width / 2) > 2 || Math.abs(desktop.y + desktop.height / 2 - viewport.height / 2) > 2) throw new Error('dialog is not centered');
  await dialog.getByRole('button', { name: '获取授权链接', exact: true }).click();
  const urlInput = dialog.getByRole('textbox', { name: '浏览器授权链接' });
  await urlInput.waitFor();
  const originalUrl = await urlInput.inputValue();
  await page.waitForTimeout(6500);
  if (!await urlInput.isVisible() || await urlInput.inputValue() !== originalUrl) throw new Error('waiting removed or replaced the authorization link');
  if (await dialog.getByRole('alert').count()) throw new Error('pending authorization became a failure alert');
  const waiting = await page.evaluate(() => ({
    starts: window.__loginDialogCalls.filter(call => call.command === 'start_browser_login').length,
    polls: window.__loginDialogPolls,
    cancelled: window.__loginDialogCalls.filter(call => call.command === 'cancel_browser_login').length,
  }));
  if (waiting.starts !== 1 || waiting.polls !== 2 || waiting.cancelled !== 0) throw new Error(`waiting did not preserve the session: ${JSON.stringify(waiting)}`);
  await page.screenshot({ path: 'output/playwright/add-account-modal-waiting.png' });
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached' });
  await page.waitForFunction(() => window.__loginDialogCalls.some(call => call.command === 'cancel_browser_login'));
  if (!await add.evaluate(element => document.activeElement === element)) throw new Error('Escape did not return focus to the opener');

  await add.click();
  await dialog.waitFor();
  await dialog.getByRole('tab', { name: '手动导入', exact: true }).click();
  await dialog.getByRole('button', { name: '确认导入', exact: true }).waitFor();
  await page.mouse.click(8, 8);
  await dialog.waitFor({ state: 'detached' });

  const localImport = page.getByRole('button', { name: '本机导入', exact: true });
  await localImport.click();
  await dialog.waitFor();
  if (await dialog.getByRole('tab', { name: '手动导入', exact: true }).getAttribute('aria-selected') !== 'true') throw new Error('local import did not open the manual tab in the modal');
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached' });
  if (!await localImport.evaluate(element => document.activeElement === element)) throw new Error('closing local import did not restore its trigger focus');

  await add.click();
  await dialog.waitFor();
  await dialog.getByRole('button', { name: '获取授权链接', exact: true }).click();
  await urlInput.waitFor();
  await page.evaluate(() => { window.__loginDialogReady = true; });
  await page.getByRole('heading', { name: '弹窗授权测试账号', exact: true }).waitFor({ timeout: 7000 });
  await dialog.waitFor({ state: 'detached' });
  await page.setViewportSize({ width: 390, height: 844 });
  await add.click();
  await dialog.waitFor();
  await dialog.getByRole('tab', { name: '手动导入', exact: true }).click();
  const mobile = await dialog.boundingBox();
  if (mobile.x < 0 || mobile.y < 0 || mobile.x + mobile.width > 391 || mobile.y + mobile.height > 845) throw new Error('modal exceeds the narrow viewport');
  if (await dialog.evaluate(element => element.scrollWidth > element.clientWidth)) throw new Error('modal overflows horizontally');
  await page.screenshot({ path: 'output/playwright/add-account-modal-mobile.png' });
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached' });

  await page.setViewportSize({ width: 1440, height: 1080 });
  const accountCard = page.locator('article').filter({ hasText: '弹窗授权测试账号' });
  const deleteButton = accountCard.getByRole('button', { name: '删除', exact: true });
  const saveCount = () => page.evaluate(() => window.__loginDialogCalls.filter(call => call.command === 'save_config').length);
  const previousSaves = await saveCount();
  await deleteButton.click();
  await dialog.waitFor();
  if (!(await dialog.textContent()).includes('弹窗授权测试账号')) throw new Error('deletion dialog did not identify the target account');
  const cancelDelete = dialog.getByRole('button', { name: '取消', exact: true });
  if (!await cancelDelete.evaluate(element => document.activeElement === element)) throw new Error('safe action is not focused by default');
  await cancelDelete.click();
  await dialog.waitFor({ state: 'detached' });
  if (await saveCount() !== previousSaves || !await accountCard.count()) throw new Error('cancel submitted a deletion');
  await deleteButton.click();
  await dialog.waitFor();
  await page.screenshot({ path: 'output/playwright/delete-account-modal.png' });
  await page.evaluate(() => { window.__holdDeleteSave = true; });
  await dialog.getByRole('button', { name: '确认删除', exact: true }).click();
  await page.waitForFunction(() => !!window.__rejectDeleteSave);
  await page.keyboard.press('Escape');
  if (!await dialog.isVisible()) throw new Error('dialog closed during the save');
  await page.evaluate(() => {
    window.__holdDeleteSave = false;
    window.__rejectDeleteSave(new Error('模拟删除保存失败'));
  });
  await dialog.getByRole('alert').filter({ hasText: '模拟删除保存失败' }).waitFor();
  if (!await accountCard.count()) throw new Error('a failed save removed the account card');
  await dialog.getByRole('button', { name: '确认删除', exact: true }).click();
  await dialog.waitFor({ state: 'detached' });
  if (await accountCard.count() || !await page.getByRole('heading', { name: '日常开发账号', exact: true }).isVisible()) throw new Error('confirmed deletion did not remove only the target');
  if (await saveCount() !== previousSaves + 2) throw new Error('unexpected deletion submissions');
  console.log(JSON.stringify({ ok: true, waiting, desktop, mobile, modalCloseAndImport: true, deleteCancelFailureAndSuccess: true }));
}
