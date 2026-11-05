async (page) => {
  await page.getByRole('heading',{name:'用量统计',exact:true}).waitFor();
  await page.evaluate(async () => {
    const original=window.__TAURI_INTERNALS__.invoke;
    window.__renewalUiCalls=[];
    window.__renewalUiSuccess=null;
    window.__TAURI_INTERNALS__.invoke=async (command,args={})=>{
      window.__renewalUiCalls.push({command,args});
      if(command==='keepalive_status') {
        return window.__previewConfig.accounts.map((account,index)=>({uid:account.uid,in_progress:false,last_attempt_at:window.__renewalUiSuccess,last_success_at:index===0?window.__renewalUiSuccess:null,next_attempt_at:window.__previewConfig.schedule.keepalive_enabled===false?null:Date.now()+60000,consecutive_failures:0,error:account.refresh_token?'':'未配置 Refresh Token，请在账号保险柜补充续期凭证'}));
      }
      return original(command,args);
    };
    const {useApp}=await import('/src/stores/appStore.ts');
    window.__previewConfig.accounts[0].refresh_token='';
    useApp.setState({config:structuredClone(window.__previewConfig)});
    window.dispatchEvent(new Event('wb-pool-updated'));
  });
  await page.getByRole('button',{name:'OpenAI 网关',exact:true}).click();
  const settings=page.locator('details').filter({hasText:'高级设置'});
  await settings.locator('summary').click();
  const strategy=settings.getByRole('combobox');
  if(await strategy.inputValue()!=='least_recent') throw new Error('legacy weighted strategy was not mapped to idle-first');
  const sticky=settings.getByRole('checkbox',{name:'保持会话账号一致',exact:false});
  if(!await sticky.isDisabled()||await sticky.isChecked()) throw new Error('idle-first rotation must not pin a conversation');
  await page.getByRole('button',{name:'运维·签到积分',exact:true}).click();
  const auto=page.getByRole('checkbox',{name:'后台自动续期',exact:false});
  if(!await auto.isChecked()) throw new Error('background renewal should default on');
  await page.getByRole('heading',{name:'账号自动续期',exact:true}).waitFor();
  await auto.uncheck();
  await page.waitForFunction(()=>window.__previewConfig.schedule.keepalive_enabled===false);
  await auto.check();
  await page.waitForFunction(()=>window.__previewConfig.schedule.keepalive_enabled===true);
  await page.getByRole('button',{name:'账号保险柜',exact:true}).click();
  const account=page.getByRole('article').filter({has:page.getByRole('heading',{name:'日常开发账号',exact:true})});
  await account.getByRole('button',{name:'补充续期凭证',exact:true}).click();
  await account.getByPlaceholder('填写该账号对应的续期凭证').fill('fictional-new-refresh');
  await account.getByRole('button',{name:'保存续期凭证',exact:true}).click();
  await page.waitForFunction(()=>window.__previewConfig.accounts[0].refresh_token==='fictional-new-refresh');
  const saved=await page.evaluate(()=>window.__renewalUiCalls.filter(c=>c.command==='save_config').at(-1));
  if(saved.args.expectedCredentials?.uid!=='preview-1'||saved.args.cfg.accounts[0].uid!=='preview-1') throw new Error('credential save lost stable account identity or concurrency protection');
  await page.evaluate(async()=>{
    const {useApp}=await import('/src/stores/appStore.ts');
    window.__previewConfig.accounts[0].token='fictional-background-access';
    window.__previewConfig.accounts[0].refresh_token='fictional-background-refresh';
    window.__previewConfig.accounts[0].expires_at=Math.floor(Date.now()/1000)+7200;
    window.__renewalUiSuccess=Date.now();
    await useApp.getState().syncKeepaliveStatus();
    const account=useApp.getState().config.accounts[0];
    if(account.token!=='fictional-background-access'||account.refresh_token!=='fictional-background-refresh') throw new Error('background renewal credentials were not synced to UI');
    if(window.__renewalUiCalls.some(c=>c.command==='switch_account')) throw new Error('renewal must not switch the official client');
    window.__renewalUiResult={idleFirstStrategy:true,stickyDisabled:true,keepaliveSwitchPersisted:true,refreshCredentialSaved:true,backgroundCredentialsSynced:true,officialSwitchCalls:0};
  });
}
