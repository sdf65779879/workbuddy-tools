async (page) => {
  await page.evaluate(async () => {
    const { useTasks } = await import('/src/stores/taskStore.ts');
    const original = window.__TAURI_INTERNALS__.invoke;
    window.__growthJob = null;
    window.__growthStarts = [];
    window.__growthScanCount = 0;
    const excluded = ['无畏契约拼豆专家', '中国电商运营专家', '商家宣传页与海报设计师', '直播电商教练', '关注腾讯 WorkBuddy 官方公众号', '抖音策略师'];
    const task = (name, index) => ({ task_code: `task-${index}`, title: name, description: '模拟任务说明', task_desc: '', credit: 100, energy: 0, has_reward: true, task_type: 'daily', locked: false, target: 1, current: 0, accept_status: 'accepted', status: 'in_progress', claimed: false, claimable: false });
    window.__growthTasks = [...excluded.map((name, index) => ({ ...task(name, index), current: 1, claimable: true, skipped: true, skip_reason: '按用户设置自动忽略' })), { ...task('正常待领奖励', 6), current: 1, claimable: true }, task('等待真实进度', 7)];
    window.__TAURI_INTERNALS__.invoke = async (command, args = {}) => {
      if (command === 'keepalive_status') return [];
      if (command === 'task_job_status') return structuredClone(window.__growthJob);
      if (command === 'task_scan_all') {
        window.__growthScanCount++;
        const snapshot = [{ uid: 'preview-1', nickname: '日常开发账号', tasks: structuredClone(window.__growthTasks), error: null }];
        if (window.__holdGrowthScan) {
          window.__holdGrowthScan = false;
          return new Promise(resolve => { window.__releaseGrowthScan = () => resolve(snapshot); });
        }
        return snapshot;
      }
      if (command === 'start_task_job') {
        if (window.__growthJob?.status === 'running') throw new Error('duplicate job');
        window.__growthStarts.push(args.uid);
        const now = Date.now();
        window.__growthJob = { id: `growth-job-${window.__growthStarts.length}`, status: 'running', started_at: now, finished_at: null, total: args.uid ? 1 : 4, completed: 0, current_uid: 'preview-1', result: { items: [], total_credit: 0, total_energy: 0 }, logs: [{ seq: 1, time: now, uid: 'preview-1', nickname: '日常开发账号', task_code: null, status: 'running', message: '模拟作业已启动' }] };
        return structuredClone(window.__growthJob);
      }
      return original(command, args);
    };
    await useTasks.getState().sync();
  });
  await page.getByRole('button', { name: '账号保险柜', exact: true }).click();
  const taskButton = page.locator('article').first().getByRole('button', { name: '任务', exact: true });
  await taskButton.click();
  const dialog = page.getByRole('dialog');
  await dialog.getByRole('heading', { name: '执行成长任务', exact: true }).waitFor();
  if (!await dialog.evaluate(element => element.matches(':modal'))) throw new Error('task confirmation is not modal');
  if (await page.evaluate(() => window.__growthStarts.length)) throw new Error('opening confirmation executed a job');
  await dialog.getByRole('button', { name: '取消', exact: true }).click();
  await dialog.waitFor({ state: 'detached' });
  if (await page.evaluate(() => window.__growthStarts.length)) throw new Error('cancel executed a job');
  await taskButton.click();
  await page.screenshot({ path: 'output/playwright/growth-task-confirm.png' });
  await dialog.getByRole('button', { name: '确认执行', exact: true }).click();
  await page.getByRole('heading', { name: '任务作业', exact: true }).waitFor();
  if (await page.evaluate(() => window.__growthStarts[0]) !== 'preview-1') throw new Error('single account UID was lost');
  await page.getByRole('log').getByText('日常开发账号 · 模拟作业已启动', { exact: true }).waitFor();
  if (!await page.getByRole('button', { name: '任务正在执行', exact: true }).isDisabled()) throw new Error('batch can start during single job');
  await page.getByRole('button', { name: /^待领奖励/ }).click();
  await page.getByText('正常待领奖励', { exact: true }).waitFor();
  if (await page.locator('.wb-task-item').count() !== 1) throw new Error('excluded rewards leaked into claimable view');
  await page.getByRole('button', { name: '全部任务', exact: true }).click();
  if (await page.locator('.wb-task-item').filter({ hasText: '已忽略' }).count() !== 6) throw new Error('six tasks not marked ignored');
  if (!await page.locator('.wb-task-item').filter({ hasText: '等待真实进度' }).getByText('待完成', { exact: true }).isVisible()) throw new Error('idle task mislabeled in progress');

  await page.getByRole('button', { name: '账号保险柜', exact: true }).click();
  if (!await page.locator('article').first().getByRole('button', { name: '任务', exact: true }).isDisabled()) throw new Error('card can duplicate a job');
  if (await page.locator('article .wb-inline-confirm').count()) throw new Error('inline task details still present');
  await page.evaluate(() => {
    window.__growthJob.logs.push({ seq: 2, time: Date.now(), uid: 'preview-1', nickname: '日常开发账号', task_code: 'task-6', status: 'completed', message: '切页期间继续执行并完成领奖' });
  });
  await page.waitForFunction(async () => {
    const { useTasks } = await import('/src/stores/taskStore.ts');
    return useTasks.getState().job?.logs.length === 2;
  });
  await page.getByRole('button', { name: '成长任务', exact: true }).click();
  await page.getByRole('log').getByText('日常开发账号 · 切页期间继续执行并完成领奖', { exact: true }).waitFor();

  await page.evaluate(() => { window.__holdGrowthScan = true; });
  await page.getByRole('button', { name: '刷新任务', exact: true }).click();
  await page.waitForFunction(() => !!window.__releaseGrowthScan);
  const scanCount = await page.evaluate(() => window.__growthScanCount);
  await page.evaluate(() => {
    window.__growthTasks[7].claimable = true;
    window.__growthTasks[7].current = 1;
    window.__growthJob.status = 'partial';
    window.__growthJob.completed = 1;
    window.__growthJob.finished_at = Date.now();
    window.__growthJob.current_uid = null;
    window.__growthJob.result = { items: [{ uid: 'preview-1', nickname: '日常开发账号', checkin: 'already', accepted: 1, claimed: 1, completed: 1, skipped: 6, waiting: 1, credit: 100, energy: 0, error: '模拟单项失败' }], total_credit: 100, total_energy: 0 };
    window.__growthJob.logs.push({ seq: 3, time: Date.now(), uid: 'preview-1', nickname: '日常开发账号', task_code: 'task-7', status: 'error', message: '模拟单项失败，作业已结束' });
  });
  await page.getByText('作业结束 · 部分失败', { exact: true }).waitFor();
  await page.evaluate(() => window.__releaseGrowthScan());
  await page.waitForFunction(count => window.__growthScanCount > count, scanCount);
  await page.locator('.wb-task-item').filter({ hasText: '等待真实进度' }).getByText('可领取', { exact: true }).waitFor();
  await page.getByRole('log').getByText('日常开发账号 · 模拟单项失败，作业已结束', { exact: true }).waitFor();
  await page.screenshot({ path: 'output/playwright/growth-job-desktop.png' });
  await page.getByRole('button', { name: '执行并自动领奖', exact: true }).click();
  await page.getByText('执行中', { exact: true }).waitFor();
  if (await page.evaluate(() => window.__growthStarts.length) !== 2 || await page.evaluate(() => window.__growthStarts[1]) !== null) throw new Error('batch retry did not start after terminal result');
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: 'output/playwright/growth-job-mobile.png' });
  if (await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)) throw new Error('mobile overflow');
  console.log('PASS modal cancellation/confirmation, single-account navigation, shared live logs, duplicate prevention, six exclusions, completion refresh race, partial result and retry, mobile layout');
}
