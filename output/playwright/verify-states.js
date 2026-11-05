async (page) => {
  await page.getByRole('button',{name:'用量统计',exact:true}).click();
  await page.getByRole('heading',{name:'用量统计',exact:true}).waitFor();
  await page.evaluate(()=>window.__previewEmpty=true);
  await page.getByRole('button',{name:'刷新数据',exact:true}).click();
  await page.getByText('这段时间，尚无用量记录',{exact:true}).waitFor();
  if(await page.locator('.us-kpi-value').first().innerText()!=='0') throw new Error('empty requests must be zero');
  await page.evaluate(()=>{
    window.__originalPreviewInvoke=window.__TAURI_INTERNALS__.invoke;
    window.__TAURI_INTERNALS__.invoke=async(command,args)=>{
      if(command==='usage_dashboard') throw new Error('QA: statistics temporarily unavailable');
      return window.__originalPreviewInvoke(command,args);
    };
  });
  await page.getByRole('button',{name:'刷新数据',exact:true}).click();
  await page.getByText('暂时无法读取用量统计',{exact:true}).waitFor();
  await page.evaluate(()=>{
    window.__previewEmpty=false;
    window.__TAURI_INTERNALS__.invoke=window.__originalPreviewInvoke;
  });
  await page.getByRole('button',{name:'重新加载',exact:true}).click();
  await page.getByRole('button',{name:'刷新数据',exact:true}).waitFor();
  await page.evaluate(()=>{
    window.__TAURI_INTERNALS__.invoke=async(command,args)=>{
      const result=await window.__originalPreviewInvoke(command,args);
      if(command!=='refresh_balances') return result;
      return result.slice(0,3).map((a,i)=>i===0?{...a,credits:1234,remaining_credits:1234.5,total_credits:2000,used_credits:765.5}:i===1?{...a,credits:null,remaining_credits:null,total_credits:null,used_credits:null,error:'QA: expired account'}:{...a,credits:null,remaining_credits:null,total_credits:null,used_credits:null,unlimited:true});
    };
  });
  await page.getByRole('button',{name:'刷新账号实际额度',exact:true}).click();
  await page.waitForFunction(()=>document.querySelector('.wb-balance-summary strong')?.textContent.includes('1,234.5'));
  const partial=await page.locator('.wb-balance-summary').innerText();
  if(!partial.includes('∞')||!partial.includes('部分读取失败')||!partial.includes('765.5')) throw new Error('partial/unlimited/decimal quota incorrect');
  await page.evaluate(()=>window.__TAURI_INTERNALS__.invoke=window.__originalPreviewInvoke);
  await page.getByRole('button',{name:'刷新账号实际额度',exact:true}).click();
  await page.waitForFunction(()=>document.querySelector('.wb-balance-summary strong')?.textContent.includes('15,732'));
  await page.evaluate(()=>{
    window.__qaResults.push('empty statistics state','statistics error and retry','decimal quota preserved','unlimited quota not summed as fake number','partial balance error visible');
    window.scrollTo(0,0);
  });
  await page.screenshot({path:'output/playwright/usage-desktop.png',fullPage:false});
  await page.getByRole('button',{name:'切换深色外观'}).click();
  await page.screenshot({path:'output/playwright/usage-dark.png',fullPage:false});
  await page.getByRole('button',{name:'切换浅色外观'}).click();
  await page.setViewportSize({width:390,height:960});
  await page.evaluate(()=>window.scrollTo(0,0));
  await page.screenshot({path:'output/playwright/usage-mobile.png',fullPage:false});
  await page.setViewportSize({width:1440,height:1080});
  await page.evaluate(()=>window.scrollTo(0,0));
}
