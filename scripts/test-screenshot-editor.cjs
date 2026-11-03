const fs=require('fs');
const assert=require('node:assert/strict');
const {chromium}=require(process.env.PIERDESK_PLAYWRIGHT_PACKAGE || 'playwright');
const path=require('path');
const root=path.resolve(__dirname,'..');
const screenshotPath=path.join(require('os').tmpdir(),'pierdesk-screenshot-editor-qa.png');
let browser;
const html=fs.readFileSync(root+'/assets/screenshot/index.html','utf8').replace('/*__EDITOR__*/',fs.readFileSync(root+'/assets/screenshot/editor.js','utf8'));
const errors=[];
async function drag(page,x1,y1,x2,y2){await page.mouse.move(x1,y1);await page.mouse.down();await page.mouse.move(x2,y2,{steps:8});await page.mouse.up();}
async function load(page,width,height,scale=1,language='zh-CN'){
  await page.setViewportSize({width,height});
  await page.goto('about:blank');
  await page.evaluate(()=>{window.__commands=[];window.chrome={webview:{postMessage:v=>window.__commands.push(v),addEventListener(){}}};});
  await page.setContent(html);
  await page.evaluate(({width,height,scale,language})=>{
    const c=document.createElement('canvas');c.width=width*scale;c.height=height*scale;const x=c.getContext('2d');
    x.fillStyle='#f5f2ec';x.fillRect(0,0,c.width,c.height);
    for(let i=0;i<c.width;i+=4){x.fillStyle=(i%8===0)?'#eff1f5':'#262d35';x.fillRect(i,100*scale,4,320*scale);}
    x.fillStyle='#116b80';x.fillRect(20,20,180,40);
    window.dispatchEvent(new CustomEvent('screenshot-input',{detail:{type:'capture',image:c.toDataURL(),width:c.width,height:c.height,language}}));
  },{width,height,scale,language});
  await page.waitForFunction(()=>window.__commands.some(v=>v.action==='shown'));
}
async function imageInfo(page,action='complete'){
  return page.evaluate(async action=>{
    const command=window.__commands.findLast(v=>v.action===action);if(!command)return null;
    const img=new Image();img.src=command.image;await img.decode();const c=document.createElement('canvas');c.width=img.width;c.height=img.height;const ctx=c.getContext('2d');ctx.drawImage(img,0,0);const pixels=ctx.getImageData(0,0,img.width,img.height).data;let red=0;for(let i=0;i<pixels.length;i+=4)if(pixels[i]>180&&pixels[i+1]<150&&pixels[i+2]<170)red++;
    return{width:img.width,height:img.height,red,action:command.action};
  },action);
}
(async()=>{
  browser=await chromium.launch({headless:true});
  const page=await browser.newPage({viewport:{width:1200,height:800},deviceScaleFactor:2});
  page.on('pageerror',error=>errors.push(error.message));
  // Window candidates are instant; provider replies are asynchronous and must
  // not change a selection after a click, drag, or a newer pointer request.
  await load(page,1200,800,2);
  await page.evaluate(()=>window.dispatchEvent(new CustomEvent('screenshot-input',{detail:{type:'capture',image:document.querySelector('#screen').toDataURL(),width:2400,height:1600,windows:[{x:100,y:80,w:1600,h:1200},{x:0,y:0,w:2400,h:1600}]}})));
  await page.mouse.move(300,250);
  await page.waitForFunction(()=>window.__commands.some(v=>v.action==='detect'));
  assert.equal(await page.locator('#size').textContent(),'1600 × 1200');
  assert.ok(await page.locator('#toolbar').isHidden());
  const request=await page.evaluate(()=>window.__commands.findLast(v=>v.action==='detect'));
  await page.evaluate(id=>window.dispatchEvent(new CustomEvent('screenshot-input',{detail:{type:'candidates',id,regions:[{x:200,y:100,w:1100,h:1000},{x:100,y:80,w:1600,h:1200}]}})),request.id);
  assert.equal(await page.locator('#size').textContent(),'1100 × 1000 · 1/2');
  // Later visual refinement must replace a coarse UIA pane without moving the mouse.
  await page.evaluate(id=>window.dispatchEvent(new CustomEvent('screenshot-input',{detail:{type:'candidates',id,regions:[{x:400,y:300,w:600,h:500},{x:100,y:80,w:1600,h:1200}]}})),request.id);
  assert.equal(await page.locator('#size').textContent(),'600 × 500 · 1/2');
  await page.evaluate(id=>window.dispatchEvent(new CustomEvent('screenshot-input',{detail:{type:'candidates',id,window:{x:100,y:80,w:1600,h:1200},visual:[{x:400,y:300,w:600,h:500},{x:1050,y:300,w:400,h:500}],regions:[{x:100,y:80,w:1600,h:1200}]}})),request.id);
  await page.mouse.move(600,250);
  assert.equal(await page.locator('#size').textContent(),'400 × 500 · 1/2','Cached visual bounds must follow the pointer without waiting for the backend');
  await page.mouse.move(300,250);
  assert.equal(await page.locator('#size').textContent(),'600 × 500 · 1/2');
  await page.mouse.wheel(0,100);
  assert.equal(await page.locator('#size').textContent(),'1600 × 1200 · 2/2');
  await page.mouse.wheel(0,-100);
  await page.mouse.down();await page.mouse.up();
  assert.equal(await page.locator('#size').textContent(),'600 × 500');
  assert.ok(await page.locator('#toolbar').isVisible());
  await page.evaluate(id=>window.dispatchEvent(new CustomEvent('screenshot-input',{detail:{type:'candidates',id,regions:[{x:0,y:0,w:2400,h:1600}]}})),request.id);
  assert.equal(await page.locator('#size').textContent(),'600 × 500');
  await page.keyboard.press('Enter');
  assert.equal((await imageInfo(page)).width,600);
  assert.equal((await imageInfo(page)).height,500);
  await page.keyboard.press('Escape');
  // Manual drag overrides automatic candidates, even with a reply in flight.
  await load(page,1200,800,2);
  await page.evaluate(()=>window.dispatchEvent(new CustomEvent('screenshot-input',{detail:{type:'capture',image:document.querySelector('#screen').toDataURL(),width:2400,height:1600,windows:[{x:0,y:0,w:2400,h:1600}]}})));
  await page.mouse.move(100,80);
  await drag(page,100,80,720,520);
  assert.equal(await page.locator('#size').textContent(),'1240 × 880');
  // Undo restores an unselected state; stale candidates cannot reappear.
  await page.keyboard.press('Control+z');assert.ok(await page.locator('#size').isHidden());
  await page.keyboard.press('Control+y');assert.equal(await page.locator('#size').textContent(),'1240 × 880');
  await load(page,1200,800,2);
  await drag(page,100,80,720,520);
  assert.equal(await page.locator('#size').textContent(),'1240 × 880');
  await page.locator('[data-tool=arrow]').click();await drag(page,180,170,560,350);
  await page.locator('[data-tool=rect]').click();await drag(page,140,110,300,210);
  await page.locator('[data-tool=ellipse]').click();await drag(page,400,250,600,400);
  await page.locator('[data-tool=pen]').click();await drag(page,150,430,400,450);
  await page.locator('[data-tool=text]').click();await page.mouse.click(450,430);await page.locator('#text').fill('中文标注 Pierdesk');await page.locator('#text').press('Control+Enter');
  await page.locator('[data-tool=select]').click();await page.mouse.click(470,435);await page.keyboard.press('Delete');await page.keyboard.press('Control+z');
  await page.locator('[data-tool=mosaic]').click();await drag(page,330,120,600,140);
  await page.keyboard.press('Control+z');await page.keyboard.press('Control+y');
  await page.screenshot({path:screenshotPath});
  await page.locator('[data-tool=complete]').click();
  const info=await imageInfo(page);assert.equal(info.width,1240);assert.equal(info.height,880);assert.ok(info.red>500);
  await page.evaluate(()=>window.dispatchEvent(new CustomEvent('screenshot-input',{detail:{type:'error',message:'Clipboard busy test'}})));
  assert.equal(await page.locator('#toast').textContent(),'Clipboard busy test');
  // Preview dimensions must follow physical image pixels, not CSS/DPI pixels.
  await load(page,1200,800,2);await drag(page,100,80,720,520);
  await page.locator('[data-tool=pen]').click();
  await page.locator('#width').fill('20');await page.mouse.move(350,260);
  let cursor=await page.locator('#tool-cursor').boundingBox();assert.equal(cursor.width,10);assert.equal(cursor.height,10);
  assert.match(await page.locator('#tool-caption').textContent(),/20 px/);
  await page.locator('[data-tool=mosaic]').click();await page.mouse.move(350,260);
  cursor=await page.locator('#tool-cursor').boundingBox();assert.equal(cursor.width,30);assert.equal(cursor.height,30);
  assert.equal(cursor.x,330);assert.equal(cursor.y,240); // Same physical grid as the mosaic renderer.
  await page.locator('[data-tool=text]').click();await page.locator('#font').selectOption('48');await page.mouse.move(350,260);
  assert.equal(await page.locator('#tool-cursor').textContent(),'文字 Aa');
  assert.equal(await page.locator('#tool-cursor').evaluate(el=>getComputedStyle(el).fontSize),'48px');
  assert.equal(await page.locator('#tool-cursor').evaluate(el=>getComputedStyle(el).transform),'matrix(0.5, 0, 0, 0.5, 0, 0)');
  await page.mouse.click(350,260);assert.ok(await page.locator('#tool-cursor').isHidden());
  assert.equal(await page.locator('#text').evaluate(el=>getComputedStyle(el).fontSize),'24px');await page.locator('#text').press('Escape');
  await page.mouse.move(350,260);await page.keyboard.press('Enter');
  assert.equal((await imageInfo(page)).red,0,'Hover previews must never enter the exported image');
  await load(page,940,680,1,'en');await drag(page,900,620,200,200);
  assert.equal(await page.locator('#size').textContent(),'700 × 420');
  await page.locator('[data-tool=text]').click();await page.locator('#font').selectOption('64');
  assert.ok((await page.locator('#tool-sample').boundingBox()).height>=80);
  const box=await page.locator('#toolbar').boundingBox();assert.ok(box.x>=0&&box.y>=0&&box.x+box.width<=940&&box.y+box.height<=680);
  await page.locator('[data-tool=select]').click();
  await page.locator('[data-tool=reset]').click();await drag(page,30,30,500,350);
  await drag(page,500,350,550,380);
  assert.equal(await page.locator('#size').textContent(),'520 × 350');
  await page.locator('[data-tool=pin]').click();assert.equal((await imageInfo(page,'pin')).width,520);
  await page.evaluate(()=>{const command=window.__commands.findLast(v=>v.action==='pin');window.dispatchEvent(new CustomEvent('screenshot-input',{detail:{type:'pinned',image:command.image}}));});
  assert.ok(await page.locator('body.pinned').count());await page.mouse.move(100,100);await page.mouse.wheel(0,-120);assert.ok(await page.evaluate(()=>window.__commands.some(v=>v.action==='zoom')));
  await page.locator('[data-pin=copy]').click();assert.ok(await page.evaluate(()=>window.__commands.some(v=>v.action==='complete')));
  await page.locator('[data-pin=close]').click();assert.equal(await page.evaluate(()=>window.__commands.at(-1).action),'cancel');
  assert.deepEqual(errors,[]);
  await browser.close();console.log(JSON.stringify({ok:true,checks:['hover window candidate','async internal region candidate','wheel candidate switching','click to select','stale reply ignored after selection','drag overrides candidate','candidate selection undo/redo','2x pixel coordinates','all drawing tools','Chinese text','annotation selection/delete/undo','mosaic undo/redo','PNG export dimensions and markings','error retry','physical-size pen/mosaic/text previews','previews excluded from export','narrow-screen toolbar bounds','reverse selection','region resize','pin/zoom/copy/close'],export:info}));
})().catch(error=>{console.error(error);process.exitCode=1;}).finally(async()=>{await browser?.close();});
