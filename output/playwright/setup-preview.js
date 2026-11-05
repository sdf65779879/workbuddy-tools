async (page) => {
  await page.addInitScript(() => {
    window.__previewCalls = [];
    window.__previewEmpty = false;
    window.__TAURI_INTERNALS__ = {
      invoke: async (command, args = {}) => {
        window.__previewCalls.push({ command, args });
        const { DEFAULT_CONFIG } = await import('/src/types.ts');
        const now = Date.now();
        const names = ['日常开发账号', '设计工作室', '灵感实验室', '备用账号'];
        const accounts = names.map((nickname, i) => ({ uid: `preview-${i+1}`, nickname, token: 'preview-token-only', refresh_token: 'preview-refresh-only', email: '', enterprise_id: '', enterprise_name: '', domain: '', expires_at: Math.floor(now / 1000) + (20+i)*86400, realm: 'cn', enabled: true, paused: false }));
        window.__previewConfig ??= { ...DEFAULT_CONFIG, accounts, schedule: { ...DEFAULT_CONFIG.schedule, checkin_enabled: false } };
        const config = window.__previewEmpty ? { ...window.__previewConfig, accounts: [] } : window.__previewConfig;
        const pool = config.accounts.map((a,i) => ({ ...a, token_len: 18, credits: [5709, 3430, 2625, 3968][i] ?? 100, cooling: false, cool_kind: '', cool_remaining_secs: 0, reason: '', soft_streak: 0, consecutive_fails: 0, success_count: 40 + i*13, err_total: i, in_flight: 0, checkin_done: true }));
        const series = Array.from({ length: 24 }, (_,i) => ({ time_ms: new Date(new Date(now + 8*3600000).toISOString().slice(0,10) + 'T00:00:00+08:00').getTime()+i*3600000, requests: [4,2,1,0,0,1,4,9,17,28,31,23,18,24,30,37,29,21,15,18,12,8,5,2][i], errors: i%9===0?1:0, prompt_tokens: (i+2)*620, completion_tokens:(i+2)*320, total_tokens:(i+2)*940, credit:(i+2)*.43 }));
        const breakdown = (requests, i) => ({ requests, errors: i, prompt_tokens: requests*620, completion_tokens:requests*320, total_tokens:requests*940, avg_latency_ms:980+i*110, credit:requests*.43, cache_hit_tokens:requests*120, cache_miss_tokens:requests*500 });
        const selected = args.query || {};
        const models = ['workbuddy-fast', 'deepseek-v4-pro', 'glm-5.3'];
        const by_model = models.filter(m=>!selected.model || selected.model===m).map((model,i)=>({model,...breakdown([215,96,41][i],i)}));
        const by_account = config.accounts.filter(a=>!selected.uid||selected.uid===a.uid).map((a,i)=>({uid:a.uid,realm:'cn',...breakdown([143,102,66,41][i],i)}));
        const total = by_model.reduce((sum,m)=>sum+m.requests,0);
        const zero = window.__previewEmpty;
        const stats = { saved_at: now, total_requests:zero?0:total, total_errors:zero?0:3, prompt_tokens:zero?0:218240, completion_tokens:zero?0:112640, total_tokens:zero?0:330880, avg_latency_ms:zero?0:1180, total_credit:zero?0:151.36, by_model:zero?[]:by_model, by_account:zero?[]:by_account };
        const recent = zero?[]:Array.from({length:12},(_,i)=>({time:now-i*53000,request_id:`preview-request-${i}`,path:'/v1/chat/completions',account:accounts[i%4].nickname,account_uid:accounts[i%4].uid,model:models[i%3],status:i===7?429:200,ok:i!==7,outcome:i===7?'upstream_error':'success',duration_ms:980+i*78,ttfb_ms:180+i*9,attempts:1,prompt_tokens:620+i*15,completion_tokens:320+i*8,total_tokens:940+i*23,credit:.43,credit_known:true,client_ip:'127.0.0.1',user_agent:'Preview fixture'}));
        if(command==='get_config') return config;
        if(command==='read_current_workbuddy_account') return { ...accounts[0], uid:'preview-local', workbuddy_uid:'preview-local', nickname:'预览本机账号', token:'preview-local-access', refresh_token:'preview-local-refresh' };
        if(command==='save_config') { window.__previewConfig=args.cfg; return args.cfg; }
        if(command==='gateway_status'||command==='start_gateway') return {running:true,port:15722,base_url:'http://127.0.0.1:15722/v1',models};
        if(command==='stop_gateway') return {running:false,port:15722,base_url:'http://127.0.0.1:15722/v1',models};
        if(command==='pool_status') return pool;
        if(command==='refresh_balances') return pool.map(a=>({uid:a.uid,nickname:a.nickname,credits:a.credits,remaining_credits:a.credits,total_credits:a.credits+800,used_credits:800,unlimited:false,fetched_at:now,error:null}));
        if(command==='usage_snapshot') return stats;
        if(command==='usage_dashboard') return {...stats,window_from_ms:selected.from_ms??series[0].time_ms,window_to_ms:selected.to_ms??now,granularity:selected.granularity??'hour',success_rate:zero?0:99.15,avg_ttfb_ms:zero?null:218,avg_tokens_per_second:zero?null:46.8,cache_hit_tokens:zero?0:42240,cache_miss_tokens:zero?0:176000,cache_hit_rate:zero?null:19.35,live_rpm:zero?0:8,live_tpm:zero?0:9250,series:zero?[]:series};
        if(command==='reqlog_snapshot') return {started_at_ms:now-3600000,completed:zero?0:total,received:zero?0:total,in_flight:0,succeeded:zero?0:total-3,failed:zero?0:3,success_rate:99.15,http_success_rate:99.15,avg_duration_ms:1180,recent,archive:{enabled:true,dir:'C:/Preview/request-logs',files:3,bytes:48220,dropped_writes:0,last_error:''}};
        if(command==='reqlog_read') return recent.filter(r=>(!args.filter?.account_uid||r.account_uid===args.filter.account_uid)&&(!args.filter?.model||r.model===args.filter.model)&&(!args.filter?.outcome||r.outcome===args.filter.outcome)).slice(0,args.limit||100);
        if(command==='list_api_keys') return [];
        if(command==='task_scan_all') return accounts.map(a=>({uid:a.uid,nickname:a.nickname,error:null,tasks:[{task_code:'preview-task',title:'完成每日对话',description:'在工作中与 Buddy 聊一聊',task_desc:'每日对话',credit:100,energy:10,has_reward:true,task_type:'daily',locked:false,target:1,current:1,accept_status:'accepted',status:'completed',claimed:false,claimable:true}]}));
        if(command==='checkin_one'||command==='task_run_one') return {uid:args.uid,nickname:accounts.find(a=>a.uid===args.uid)?.nickname||'',checkin:'已签到',accepted:0,claimed:1,credit:100,energy:10,error:null};
        if(command==='task_run_all') return {items:accounts.map(a=>({uid:a.uid,nickname:a.nickname,checkin:'已签到',accepted:0,claimed:1,credit:100,energy:10,error:null})),total_credit:400,total_energy:40};
        if(command==='get_data_dir') return 'C:/Preview';
        if(command==='test_connection') return {ok:true,status:200,latency_ms:218,content:'预览测试成功',error:null};
        if(command==='refresh_workbuddy_token'||command==='switch_account') return {ok:true,uid:args.uid,nickname:'预览账号',auth_file:'preview',message:'预览操作成功'};
        throw new Error('Preview command not mocked: '+command);
      }
    };
  });
  await page.setViewportSize({width:1440,height:1080});
  await page.reload();
}


