async (page) => {
  const errors = [];
  page.on('pageerror', e => errors.push(e.message));
  const checks = [];
  const assert = (value, message) => { if (!value) throw new Error(message); checks.push(message); };
  await page.getByRole('heading',{name:'用量统计',exact:true}).waitFor();
  assert(await page.getByText('鑫杭加速',{exact:true}).count()===1,'brand renamed');
  assert(await page.getByRole('button',{name:'切换深色外观'}).innerText()==='伪神模式','theme renamed');
  await page.waitForFunction(() => document.querySelector('.wb-balance-summary strong')?.textContent?.includes('15,732'));
  assert(await page.evaluate(() => window.__previewCalls.some(c=>c.command==='refresh_balances')),'real balance command called on startup');
  assert(await page.locator('.wb-balance-summary').innerText().then(t=>t.includes('18,932')&&t.includes('3,200')),'actual total and used quota rendered');
  assert(await page.locator('.us-live-card').evaluate(e=>getComputedStyle(e).display)==='grid','live metrics grid preserved');
  await page.getByRole('button',{name:'近 7 天',exact:true}).click();
  await page.waitForFunction(() => window.__previewCalls.filter(c=>c.command==='usage_dashboard').at(-1)?.args.query.granularity==='day');
  checks.push('7-day filter sends day aggregation');
  await page.getByRole('combobox',{name:'筛选账号',exact:true}).selectOption('preview-2');
  await page.waitForFunction(() => window.__previewCalls.filter(c=>c.command==='reqlog_read').at(-1)?.args.filter.account_uid==='preview-2');
  checks.push('account filter uses exact UID');
  await page.getByRole('combobox',{name:'筛选模型',exact:true}).selectOption('deepseek-v4-pro');
  await page.waitForFunction(() => window.__previewCalls.filter(c=>c.command==='usage_dashboard').at(-1)?.args.query.model==='deepseek-v4-pro');
  checks.push('model filter reaches backend');
  await page.getByRole('button',{name:'请求数',exact:true}).click();
  assert(await page.getByRole('img',{name:'成功与失败请求按日变化的趋势图'}).count()===1,'requests chart switches legend and data');
  await page.getByRole('button',{name:'查看数据表',exact:true}).click();
  assert(await page.locator('.us-series-table tbody tr').count()>0,'accessible trend table expands');
  await page.getByRole('button',{name:'按账号',exact:true}).click();
  await page.waitForFunction(()=>document.querySelector('.us-breakdown-table caption')?.textContent.includes('账号'));
  const downloadPromise = page.waitForEvent('download');
  await page.getByRole('button',{name:'导出 CSV ↗',exact:true}).click();
  const download = await downloadPromise;
  assert(download.suggestedFilename().endsWith('.csv'),'CSV export downloads');
  await download.saveAs('output/playwright/usage-export.csv');
  await page.getByRole('button',{name:'用量统计',exact:true}).click();
  await page.getByRole('combobox',{name:'筛选账号',exact:true}).selectOption('');
  await page.getByRole('combobox',{name:'筛选模型',exact:true}).selectOption('');
  await page.getByRole('button',{name:'今日',exact:true}).click();
  await page.getByRole('button',{name:'收起数据表',exact:true}).click();
  await page.getByRole('button',{name:'Token',exact:true}).click();
  await page.getByRole('button',{name:'按模型',exact:true}).click();
  await page.getByRole('button',{name:'刷新数据',exact:true}).waitFor();
  await page.screenshot({path:'output/playwright/usage-desktop.png',fullPage:false});
  await page.getByRole('button',{name:'切换深色外观'}).click();
  assert(await page.locator('.wb-app').getAttribute('data-theme')==='workbuddy-dark','dark theme applies');
  await page.screenshot({path:'output/playwright/usage-dark.png',fullPage:false});
  await page.getByRole('button',{name:'切换浅色外观'}).click();
  for (const width of [1280,920,390]) {
    await page.setViewportSize({width,height:960});
    assert(await page.evaluate(()=>document.documentElement.scrollWidth <= innerWidth+1),`no page overflow at ${width}px`);
  }
  await page.screenshot({path:'output/playwright/usage-mobile.png',fullPage:false});
  await page.setViewportSize({width:1440,height:1080});
  const pages=[['账号保险柜','你的账号，井井有条','vault'],['运维·签到积分','日常运维，轻松搞定','checkin'],['成长任务','让每一份成长，都有收获','growth'],['OpenAI 网关','连接你的 AI 工作流','gateway'],['分享·日志','分享连接，掌握每次请求','share']];
  for(const [name,title,file] of pages) {
    await page.getByRole('button',{name,exact:true}).click();
    await page.getByRole('heading',{name:title,exact:true}).waitFor();
    assert(await page.evaluate(()=>[...document.images].every(i=>i.complete&&i.naturalWidth>0)),`${file} generated icons load`);
    await page.screenshot({path:`output/playwright/${file}-desktop.png`,fullPage:false});
    for (const width of [920,390]) {
      await page.setViewportSize({width,height:960});
      assert(await page.evaluate(()=>document.documentElement.scrollWidth <= innerWidth+1),`${file} no overflow at ${width}px`);
    }
    await page.setViewportSize({width:1440,height:1080});
  }
  assert(errors.length===0,'no page JavaScript errors');
  await page.evaluate(value => window.__qaResults=value, checks);
}
