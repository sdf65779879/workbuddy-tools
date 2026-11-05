async (page) => {
  await page.getByRole('heading', {name: '用量统计', exact: true}).waitFor();
  await page.evaluate(() => {
    window.__regressionCalls = [];
    const original = window.__TAURI_INTERNALS__.invoke;
    const models = ['workbuddy-fast','workbuddy-fast-plus'];
    const metric = {requests:1, errors:0, prompt_tokens:10, completion_tokens:5, total_tokens:15, avg_latency_ms:100, credit:0, cache_hit_tokens:0, cache_miss_tokens:0};
    const logs = models.map((model,index) => ({time:Date.now()-index,request_id:'regression-'+index,path:'/v1/chat/completions',account:'虚构测试账号',account_uid:'preview-1',model,status:200,ok:true,outcome:'success',duration_ms:100,ttfb_ms:20,attempts:1,prompt_tokens:10,completion_tokens:5,total_tokens:15,credit:0,credit_known:true,client_ip:'127.0.0.1',user_agent:'isolated test'}));
    window.__TAURI_INTERNALS__.invoke = async (command,args={}) => {
      window.__regressionCalls.push({command,args});
      const result=await original(command,args);
      if(command==='usage_dashboard') {
        const rows=models.filter(model=>!args.query?.model||model===args.query.model).map(model=>({model,...metric}));
        return {...result,total_requests:rows.length,total_tokens:rows.length*15,prompt_tokens:rows.length*10,completion_tokens:rows.length*5,by_model:rows};
      }
      if(command==='reqlog_read') {
        return logs.filter(log=>(!args.filter?.model_exact||log.model===args.filter.model_exact)&&(!args.filter?.model||log.model.toLowerCase().includes(args.filter.model.toLowerCase())));
      }
      return result;
    };
  });
  await page.getByRole('button',{name:'刷新数据',exact:true}).click();
  await page.getByRole('combobox',{name:'筛选模型',exact:true}).selectOption('workbuddy-fast');
  await page.waitForFunction(()=>document.querySelector('.us-kpi-value')?.textContent==='1');
  const logRows=await page.locator('.us-recent-card tbody tr').count();
  if(logRows!==1) throw new Error('exact model filter returned '+logRows+' logs');
  const requests=await page.evaluate(()=>window.__regressionCalls.filter(c=>c.command==='reqlog_read').at(-1).args.filter);
  if(requests.model_exact!=='workbuddy-fast'||requests.model!=='') throw new Error('dashboard did not request exact model');
  await page.getByRole('button',{name:'分享·日志',exact:true}).click();
  await page.getByPlaceholder('模型名称',{exact:true}).fill('workbuddy-fast');
  await page.getByRole('button',{name:'筛选',exact:true}).click();
  await page.waitForFunction(()=>document.querySelectorAll('.wb-log-table tbody tr').length===2);

  await page.clock.install();
  await page.evaluate(async()=>{
    const {useApp}=await import('/src/stores/appStore.ts');
    const original=window.__TAURI_INTERNALS__.invoke;
    window.__failedCheckinAttempts=0;
    window.__checkinSucceeds=false;
    window.__TAURI_INTERNALS__.invoke=async(command,args={})=>{
      if(command==='pool_status') {
        const result=await original(command,args);
        return result.map(account=>({...account,checkin_done:false}));
      }
      if(command==='checkin_one') {
        window.__failedCheckinAttempts++;
        return {uid:args.uid,nickname:'虚构测试账号',checkin:window.__checkinSucceeds?'ok':'failed',accepted:0,claimed:0,credit:0,energy:0,error:window.__checkinSucceeds?null:'业务错误（code=10001）: token expired'};
      }
      return original(command,args);
    };
    for(const key of Object.keys(localStorage)) if(key.startsWith('wb-checkin-date:preview-')) localStorage.removeItem(key);
    const current=useApp.getState().config;
    useApp.setState({config:{...current,schedule:{...current.schedule,checkin_enabled:true,checkin_hours:[0]}}});
    window.dispatchEvent(new Event('wb-automation-change'));
  });
  await page.waitForFunction(()=>window.__failedCheckinAttempts===4);
  const datesAfterFailure=await page.evaluate(()=>Object.keys(localStorage).filter(key=>key.startsWith('wb-checkin-date:preview-')));
  if(datesAfterFailure.length) throw new Error('failed checkin marked day complete');
  await page.evaluate(()=>{window.__checkinSucceeds=true;});
  await page.clock.fastForward(660000);
  await page.waitForFunction(()=>window.__failedCheckinAttempts===8);
  const datesAfterSuccess=await page.evaluate(()=>Object.keys(localStorage).filter(key=>key.startsWith('wb-checkin-date:preview-')));
  if(datesAfterSuccess.length!==4) throw new Error('successful retry did not mark all accounts');
  await page.evaluate(()=>{
    window.__regressionResult={summaryRequests:1,recentExactModelLogs:1,freeTextLogs:2,failedCheckinDayMarkers:0,successfulRetryDayMarkers:4};
  });
}
