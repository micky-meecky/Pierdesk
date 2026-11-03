/* Local-only screenshot editor. All model coordinates and exports use the
   captured image's physical pixels; CSS coordinates are only for the controls. */
(() => {
  'use strict';
  const $ = id => document.getElementById(id);
  const canvas = $('screen'), ctx = canvas.getContext('2d'), image = new Image();
  const text = $('text'), toolbar = $('toolbar'), cursor = $('tool-cursor');
  let hover=null;
  let windows=[], candidates=[], candidateIndex=0, detectId=0, detectFloor=0, detectTimer=0, candidateWindow=null, manualCandidate=false;
  const candidate=()=>candidates[candidateIndex]||null;
  const visualCache=new Map(), regionKey=r=>`${r.x}:${r.y}:${r.w}:${r.h}`;
  let W=1,H=1, selection=null, items=[], tool='select', selected=-1, gesture=null;
  let history=[], future=[], textEdit=null, language='zh-CN', busy=false, pinned=false, pinnedImage='';
  const labels={
    select:['选择 / 移动标注 (V)','Select / move (V)'],rect:['矩形 (R)','Rectangle (R)'],ellipse:['椭圆 (O)','Ellipse (O)'],arrow:['箭头 (A)','Arrow (A)'],pen:['画笔 (P)','Pen (P)'],mosaic:['马赛克 (M)','Mosaic (M)'],text:['文字 (T)','Text (T)'],
    undo:['撤销 (Ctrl+Z)','Undo (Ctrl+Z)'],redo:['重做 (Ctrl+Y)','Redo (Ctrl+Y)'],reset:['重新框选','Reselect'],pin:['贴图置顶','Pin screenshot'],save:['保存并复制 (Ctrl+S)','Save and copy (Ctrl+S)'],cancel:['取消 (Esc)','Cancel (Esc)'],complete:['完成 (Enter)','Done (Enter)'],
    color:['颜色','Color'],width:['粗细','Width'],font:['字号','Font'],hint:['悬停识别 · 单击选区 · 滚轮切换范围 · 拖动自由框选 · Esc 取消','Hover to detect · Click to select · Wheel for regions · Drag freely · Esc to cancel'],pinHint:['拖动移动 · 滚轮缩放 · 双击关闭','Drag to move · Wheel to zoom · Double click to close'],copy:['复制','Copy'],savePin:['保存','Save'],close:['关闭','Close'],done:['完成','Done']
  };
  const paths={select:'M4 3l7 17 2-7 7-2z',rect:'M4 4h16v16H4z',ellipse:'M20 12a8 8 0 1 1-16 0 8 8 0 0 1 16 0',arrow:'M5 19L19 5M8 5h11v11',pen:'M4 20l4-1L20 7l-3-3L5 16zM14 7l3 3',mosaic:'M4 4h16v16H4zM4 9h16M4 15h16M9 4v16M15 4v16',text:'M5 5h14M12 5v15M8 20h8',undo:'M9 5L4 10l5 5M4 10h10a5 5 0 0 1 0 10',redo:'M15 5l5 5-5 5M20 10H10a5 5 0 0 0 0 10',reset:'M4 9V4h5M15 4h5v5M20 15v5h-5M9 20H4v-5',pin:'M8 3h8l-1 6 3 4H6l3-4zM12 13v8',save:'M4 13v7h16v-7M12 3v12M7 10l5 5 5-5',cancel:'M5 5l14 14M19 5L5 19',complete:'M4 12l5 5L20 6'};
  const tr=key => (labels[key]||[key,key])[language==='en'?1:0];
  function send(value){ if(window.chrome?.webview) window.chrome.webview.postMessage(value); else window.dispatchEvent(new CustomEvent('screenshot-command',{detail:value})); }
  const clone=v=>JSON.parse(JSON.stringify(v));
  const clamp=(v,min,max)=>Math.max(min,Math.min(max,v));
  const snapshot=()=>({selection:clone(selection),items:clone(items)});
  function remember(){history.push(snapshot());if(history.length>80)history.shift();future=[];}
  function restore(value){selection=value.selection;items=value.items;selected=-1;render();}
  function undo(){commitText();if(history.length){future.push(snapshot());restore(history.pop());}}
  function redo(){commitText();if(future.length){history.push(snapshot());restore(future.pop());}}
  function setup(){
    $('hint').querySelector('span').textContent=tr('hint');
    $('tools').replaceChildren();
    const keys=['select','rect','ellipse','arrow','pen','mosaic','text','undo','redo','reset','pin','save','cancel','complete'];
    for(const key of keys){
      if(['undo','pin','cancel'].includes(key)){const divider=document.createElement('span');divider.className='divider';$('tools').append(divider);}
      const button=document.createElement('button');button.type='button';button.dataset.tool=key;button.title=tr(key);button.setAttribute('aria-label',tr(key));button.innerHTML=`<svg viewBox="0 0 24 24" aria-hidden="true"><path d="${paths[key]}"/></svg>`;
      if(key==='complete'){button.className='complete';const label=document.createElement('span');label.textContent=tr('done');button.append(label);}
      if(key==='cancel')button.className='cancel';
      button.addEventListener('click',()=>action(key));$('tools').append(button);
    }
    document.querySelectorAll('[data-i18n]').forEach(el=>el.textContent=tr(el.dataset.i18n));
    document.querySelector('[data-pin=copy]').textContent=tr('copy');document.querySelector('[data-pin=save]').textContent=tr('savePin');document.querySelector('[data-pin=close]').textContent=tr('close');$('pininfo').textContent=tr('pinHint');
  }
  function choose(value){commitText();tool=value;selected=-1;render();}
  function action(value){
    if(busy)return;
    if(value==='cancel'){send({action:'cancel'});return;}
    if(value==='undo'){undo();return;}if(value==='redo'){redo();return;}
    if(value==='reset'){commitText();remember();selection=null;items=[];selected=-1;tool='select';clearCandidate();render();return;}
    if(['complete','save','pin'].includes(value)){finish(value);return;}
    choose(value);
  }
  function normalized(a,b){return {x:Math.floor(Math.min(a.x,b.x)),y:Math.floor(Math.min(a.y,b.y)),w:Math.max(1,Math.round(Math.abs(a.x-b.x))),h:Math.max(1,Math.round(Math.abs(a.y-b.y)))};}
  function point(event){return {x:clamp(event.clientX*W/innerWidth,0,W),y:clamp(event.clientY*H/innerHeight,0,H)};}
  function inRect(p,r,pad=0){return p.x>=r.x-pad&&p.y>=r.y-pad&&p.x<=r.x+r.w+pad&&p.y<=r.y+r.h+pad;}
  function clearCandidate(){clearTimeout(detectTimer);detectTimer=0;detectFloor=++detectId;candidates=[];candidateIndex=0;candidateWindow=null;manualCandidate=false;}
  function detectAt(p){
    if(selection||gesture||busy||pinned)return;
    const before=candidate();
    const target=windows.find(r=>inRect(p,r));
    if(target!==candidateWindow){clearCandidate();candidateWindow=target;candidates=target?[target]:[];candidateIndex=0;}
    else if(candidates.length){const current=candidate();candidates=candidates.filter(r=>inRect(p,r));candidateIndex=Math.max(0,candidates.indexOf(current));}
    if(target&&visualCache.has(regionKey(target))){
      const current=candidate();const pixels=visualCache.get(regionKey(target)).filter(r=>inRect(p,r));
      candidates=[...pixels,...candidates].filter((r,i,a)=>a.findIndex(v=>regionKey(v)===regionKey(r))===i).sort((a,b)=>a.w*a.h-b.w*b.h);
      candidateIndex=manualCandidate?Math.max(0,candidates.findIndex(r=>current&&regionKey(r)===regionKey(current))):0;
    }
    if(candidate()!==before)render();
    if(target&&!detectTimer)detectTimer=setTimeout(()=>{
      detectTimer=0;if(selection||gesture||!hover||busy||pinned||!candidateWindow)return;
      send({action:'detect',id:++detectId,x:Math.floor(hover.x),y:Math.floor(hover.y)});
    },60);
  }
  function limited(p){return {x:clamp(p.x,selection.x,selection.x+selection.w),y:clamp(p.y,selection.y,selection.y+selection.h)};}
  function style(){return {color:$('color').value,width:Number($('width').value),font:Number($('font').value)};}
  const mosaicBlock=width=>Math.max(8,width*3);
  function updatePreview(){
    const sx=innerWidth/W,sy=innerHeight/H,current=style(),kind=tool==='select'&&items[selected]?items[selected].type:tool,active=kind!=='select';
    $('color').closest('label').hidden=!active||kind==='mosaic';
    $('width').closest('label').hidden=!active||kind==='text';
    $('font').closest('label').hidden=kind!=='text';
    $('width-value').textContent=current.width+' px';
    $('tool-sample-wrap').hidden=!active;
    if(active){
      const sample=$('tool-sample'),dpr=devicePixelRatio||1;
      const sampleHeight=kind==='text'?Math.max(56,Math.ceil(current.font*sy*1.25+8)):kind==='mosaic'?Math.max(56,Math.ceil(mosaicBlock(current.width)*sy+16)):Math.max(56,Math.ceil(current.width*sy+32));
      sample.style.height=sampleHeight+'px';sample.width=Math.round(164*dpr);sample.height=Math.round(sampleHeight*dpr);
      const c=sample.getContext('2d');c.scale(dpr*sx,dpr*sy);
      const cx=82/sx,cy=sampleHeight/2/sy;
      let description;
      if(kind==='text'){
        drawItem(c,{type:'text',x:8/sx,y:Math.max(0,cy-current.font*.625),text:language==='en'?'Aa':'文字',...current});
        description=language==='en'?`Font ${current.font} px`:`字号 ${current.font} px`;
      }else if(kind==='mosaic'){
        const block=mosaicBlock(current.width);
        for(let y=-1;y<=1;y++)for(let x=-2;x<=2;x++){c.fillStyle=(x+y)%2?'#98a5ae':'#46535d';c.fillRect(cx+(x-.5)*block,cy+(y-.5)*block,block,block);}
        c.strokeStyle='#fff';c.lineWidth=1/sx;c.strokeRect(cx-block/2,cy-block/2,block,block);
        description=language==='en'?`Area ${block} × ${block} px`:`范围 ${block} × ${block} px`;
      }else{
        const shape=kind==='ellipse'||kind==='rect';
        const edge=Math.max(12,current.width*sy/2+4)/sy;
        drawItem(c,{type:kind,...current,a:{x:18/sx,y:shape?edge:cy},b:{x:146/sx,y:shape?sampleHeight/sy-edge:cy},points:[{x:18/sx,y:cy},{x:146/sx,y:cy}]});
        description=language==='en'?`Stroke ${current.width} px`:`线宽 ${current.width} px`;
      }
      $('tool-caption').textContent=description+(language==='en'?' · image pixels':' · 原图像素');
    }
    updateCursor();
  }
  function updateCursor(){
    cursor.hidden=true;
    if(!hover||!selection||busy||pinned||textEdit||tool==='select'||!inRect(hover,selection)||handleAt(hover)||gesture&&gesture.kind!=='draw')return;
    const sx=innerWidth/W,sy=innerHeight/H,current=style();
    let x=hover.x,y=hover.y,w=current.width,h=current.width;
    cursor.textContent='';cursor.style.cssText='';
    if(tool==='text'){
      cursor.className='letter';cursor.textContent=language==='en'?'Text Aa':'文字 Aa';
      cursor.style.fontSize=current.font+'px';cursor.style.color=current.color;
      cursor.style.transform=`scale(${sx},${sy})`;
      // Text previews have the same clipping boundary as exported annotations.
      cursor.style.width=(selection.x+selection.w-x)+'px';cursor.style.height=(selection.y+selection.h-y)+'px';cursor.style.overflow='hidden';
    }else if(tool==='mosaic'){
      const block=mosaicBlock(current.width),left=Math.floor(x/block)*block,top=Math.floor(y/block)*block;
      x=Math.max(left,selection.x);y=Math.max(top,selection.y);
      w=Math.min(left+block,selection.x+selection.w)-x;h=Math.min(top+block,selection.y+selection.h)-y;
      cursor.className='mosaic';
    }else{x-=w/2;y-=h/2;cursor.className='brush';}
    cursor.style.left=x*sx+'px';cursor.style.top=y*sy+'px';
    if(tool!=='text'){cursor.style.width=w*sx+'px';cursor.style.height=h*sy+'px';}
    cursor.hidden=false;
  }
  function bounds(item){
    if(item.type==='text'){const lines=item.text.split('\n');ctx.font=`${item.font}px "Segoe UI","Microsoft YaHei",sans-serif`;return {x:item.x,y:item.y,w:Math.max(...lines.map(s=>ctx.measureText(s).width),10),h:lines.length*item.font*1.25};}
    if(item.points){const xs=item.points.map(p=>p.x),ys=item.points.map(p=>p.y);return {x:Math.min(...xs),y:Math.min(...ys),w:Math.max(...xs)-Math.min(...xs),h:Math.max(...ys)-Math.min(...ys)};}
    return normalized(item.a,item.b);
  }
  function hit(p){for(let i=items.length-1;i>=0;i--)if(inRect(p,bounds(items[i]),6*W/innerWidth))return i;return -1;}
  function handles(){if(!selection)return[];const r=selection;return [[r.x,r.y,'nw'],[r.x+r.w/2,r.y,'n'],[r.x+r.w,r.y,'ne'],[r.x+r.w,r.y+r.h/2,'e'],[r.x+r.w,r.y+r.h,'se'],[r.x+r.w/2,r.y+r.h,'s'],[r.x,r.y+r.h,'sw'],[r.x,r.y+r.h/2,'w']];}
  function handleAt(p){const radius=7*W/innerWidth;return handles().find(([x,y])=>Math.abs(x-p.x)<radius&&Math.abs(y-p.y)<radius)?.[2];}
  function drawItem(c,item){
    c.save();c.strokeStyle=item.color;c.fillStyle=item.color;c.lineWidth=item.width;c.lineCap='round';c.lineJoin='round';
    if(item.type==='text'){c.font=`${item.font}px "Segoe UI","Microsoft YaHei",sans-serif`;c.textBaseline='top';item.text.split('\n').forEach((line,i)=>c.fillText(line,item.x,item.y+i*item.font*1.25));}
    else if(item.type==='mosaic'){
      const block=mosaicBlock(item.width),tiles=new Set();
      const put=p=>{const tx=Math.floor(p.x/block),ty=Math.floor(p.y/block);tiles.add(tx+','+ty);};
      item.points.forEach((p,i)=>{const prev=item.points[i-1]||p,n=Math.max(1,Math.ceil(Math.hypot(p.x-prev.x,p.y-prev.y)/(block/3)));for(let j=0;j<=n;j++)put({x:prev.x+(p.x-prev.x)*j/n,y:prev.y+(p.y-prev.y)*j/n});});
      const sample=document.createElement('canvas');sample.width=sample.height=1;const s=sample.getContext('2d');
      for(const tile of tiles){const [tx,ty]=tile.split(',').map(Number),x=tx*block,y=ty*block,w=Math.min(block,W-x),h=Math.min(block,H-y);if(w<=0||h<=0)continue;s.clearRect(0,0,1,1);s.drawImage(image,x,y,w,h,0,0,1,1);c.imageSmoothingEnabled=false;c.drawImage(sample,0,0,1,1,x,y,w,h);}
    }else if(item.type==='pen'){
      c.beginPath();item.points.forEach((p,i)=>{if(i)c.lineTo(p.x,p.y);else c.moveTo(p.x,p.y);});if(item.points.length===1)c.lineTo(item.points[0].x+.1,item.points[0].y);c.stroke();
    }else{
      const {a,b}=item,r=normalized(a,b);c.beginPath();
      if(item.type==='rect')c.rect(r.x,r.y,r.w,r.h);
      if(item.type==='ellipse')c.ellipse(r.x+r.w/2,r.y+r.h/2,r.w/2,r.h/2,0,0,Math.PI*2);
      if(item.type==='arrow'){
        c.moveTo(a.x,a.y);c.lineTo(b.x,b.y);const angle=Math.atan2(b.y-a.y,b.x-a.x),len=Math.max(12,item.width*4);c.moveTo(b.x-len*Math.cos(angle-.45),b.y-len*Math.sin(angle-.45));c.lineTo(b.x,b.y);c.lineTo(b.x-len*Math.cos(angle+.45),b.y-len*Math.sin(angle+.45));
      }c.stroke();
    }
    c.restore();
  }
  function drawScene(c){c.drawImage(image,0,0,W,H);if(selection){c.save();c.beginPath();c.rect(selection.x,selection.y,selection.w,selection.h);c.clip();items.forEach(item=>drawItem(c,item));c.restore();}}
  function positionToolbar(){
    if(!selection||gesture)return;
    const r=selection,sx=innerWidth/W,sy=innerHeight/H,tw=toolbar.offsetWidth,th=toolbar.offsetHeight;
    let top=(r.y+r.h)*sy+12;if(top+th>innerHeight-8)top=r.y*sy-th-12;if(top<8)top=clamp((r.y+r.h)*sy-th-10,8,innerHeight-th-8);
    toolbar.style.left=`${clamp((r.x+r.w)*sx-tw,8,innerWidth-tw-8)}px`;toolbar.style.top=`${top}px`;
  }
  function render(){
    if(!image.complete||!image.naturalWidth||pinned)return;
    ctx.clearRect(0,0,W,H);drawScene(ctx);
    const region=selection||(!gesture&&candidate());
    ctx.save();ctx.fillStyle='#0009';ctx.beginPath();ctx.rect(0,0,W,H);if(region)ctx.rect(region.x,region.y,region.w,region.h);ctx.fill('evenodd');ctx.restore();
    $('hint').hidden=!!selection;$('size').hidden=!region;toolbar.style.display=selection&&!gesture?'flex':'none';
    if(region&&!selection){
      const sx=innerWidth/W,sy=innerHeight/H;ctx.strokeStyle='#1cd69a';ctx.lineWidth=1.5/sx;ctx.strokeRect(region.x,region.y,region.w,region.h);
      $('size').textContent=`${region.w} × ${region.h}${candidates.length>1?` · ${candidateIndex+1}/${candidates.length}`:''}`;
      $('size').style.left=`${clamp(region.x*sx,6,Math.max(6,innerWidth-160))}px`;$('size').style.top=`${Math.max(5,region.y*sy-29)}px`;
    }
    updatePreview();
    if(selection){
      const scale=W/innerWidth,r=selection;ctx.strokeStyle='#1cd69a';ctx.lineWidth=1.5*scale;ctx.strokeRect(r.x,r.y,r.w,r.h);ctx.fillStyle='#1cd69a';for(const [x,y]of handles())ctx.fillRect(x-3*scale,y-3*scale,6*scale,6*scale);
      if(selected>=0&&items[selected]){const b=bounds(items[selected]);ctx.strokeStyle='#fff';ctx.lineWidth=scale;ctx.setLineDash([4*scale,3*scale]);ctx.strokeRect(b.x-4*scale,b.y-4*scale,b.w+8*scale,b.h+8*scale);ctx.setLineDash([]);}
      const sx=innerWidth/W,sy=innerHeight/H;$('size').textContent=`${r.w} × ${r.h}`;$('size').style.left=`${clamp(r.x*sx,6,innerWidth-120)}px`;$('size').style.top=`${Math.max(5,r.y*sy-29)}px`;
      positionToolbar();
    }
    document.querySelectorAll('[data-tool]').forEach(button=>{const key=button.dataset.tool;if(['select','rect','ellipse','arrow','pen','mosaic','text'].includes(key))button.setAttribute('aria-pressed',String(key===tool));if(key==='undo')button.disabled=!history.length;if(key==='redo')button.disabled=!future.length;});
    canvas.style.cursor=tool==='select'?'default':tool==='text'?'text':'crosshair';
  }
  function translateItem(item,dx,dy){if(item.points)item.points.forEach(p=>{p.x+=dx;p.y+=dy;});else if(item.type==='text'){item.x+=dx;item.y+=dy;}else{item.a.x+=dx;item.a.y+=dy;item.b.x+=dx;item.b.y+=dy;}}
  canvas.addEventListener('pointerdown',event=>{
    if(busy||event.button!==0)return;event.preventDefault();commitText();const p=point(event);canvas.setPointerCapture(event.pointerId);
    if(!selection){remember();clearTimeout(detectTimer);detectTimer=0;detectFloor=++detectId;gesture={kind:'region',start:p,candidate:clone(candidate())};selection=normalized(p,p);candidates=[];}
    else{
      const handle=handleAt(p);
      if(handle){remember();gesture={kind:'resize',start:p,before:clone(selection),handle};}
      else if(tool==='select'){
        const index=hit(p);selected=index;
        if(index>=0){const item=items[index];$('color').value=item.color;$('width').value=item.width;$('font').value=item.font;remember();gesture={kind:'item',start:p,index,before:clone(item)};}
        else if(inRect(p,selection)){remember();gesture={kind:'move',start:p,before:clone(selection),items:clone(items)};}
      }else if(inRect(p,selection)){
        if(items.length>=1000){toast(language==='en'?'Too many annotations':'标注数量已达上限');return;}
        if(tool==='text'){startText(p);return;}
        remember();const item={type:tool,...style()};if(['pen','mosaic'].includes(tool))item.points=[p];else{item.a=p;item.b=p;}items.push(item);selected=-1;gesture={kind:'draw',index:items.length-1};
      }
    }render();
  });
  canvas.addEventListener('pointermove',event=>{
    const p=point(event);hover=p;updateCursor();
    if(!gesture){if(!selection)detectAt(p);const h=handleAt(p);canvas.style.cursor=h?`${h}-resize`:tool==='select'?(hit(p)>=0||selection&&inRect(p,selection)?'move':'crosshair'):tool==='text'?'text':'crosshair';return;}
    const g=gesture;
    if(g.kind==='region'){if(Math.hypot((p.x-g.start.x)*innerWidth/W,(p.y-g.start.y)*innerHeight/H)>=4)g.dragged=true;selection=normalized(g.start,p);}
    if(g.kind==='resize'){
      const b=g.before;let left=b.x,right=b.x+b.w,top=b.y,bottom=b.y+b.h;
      if(g.handle.includes('w'))left=p.x;if(g.handle.includes('e'))right=p.x;if(g.handle.includes('n'))top=p.y;if(g.handle.includes('s'))bottom=p.y;selection=normalized({x:left,y:top},{x:right,y:bottom});
    }
    if(g.kind==='move'){
      const dx=Math.round(clamp(p.x-g.start.x,-g.before.x,W-g.before.x-g.before.w)),dy=Math.round(clamp(p.y-g.start.y,-g.before.y,H-g.before.y-g.before.h));selection={...g.before,x:g.before.x+dx,y:g.before.y+dy};items=clone(g.items);items.forEach(item=>translateItem(item,dx,dy));
    }
    if(g.kind==='item'){items[g.index]=clone(g.before);translateItem(items[g.index],p.x-g.start.x,p.y-g.start.y);}
    if(g.kind==='draw'){const item=items[g.index],q=limited(p);if(item.points){const previous=item.points.at(-1);if(item.points.length<12000&&Math.hypot(previous.x-q.x,previous.y-q.y)>1)item.points.push(q);}else item.b=q;}
    render();
  });
  canvas.addEventListener('pointerleave',()=>{hover=null;if(!selection&&!gesture){clearCandidate();render();}updateCursor();});
  function endGesture(event){if(!gesture)return;
    if(gesture.kind==='region'&&!gesture.dragged){selection=event?.type==='pointerup'?gesture.candidate:null;if(!selection)history.pop();}
    gesture=null;clearCandidate();if(selection){selection.x=clamp(selection.x,0,W-1);selection.y=clamp(selection.y,0,H-1);selection.w=clamp(selection.w,1,W-selection.x);selection.h=clamp(selection.h,1,H-selection.y);}render();}
  canvas.addEventListener('wheel',event=>{
    if(selection||gesture||!candidates.length)return;event.preventDefault();clearTimeout(detectTimer);detectTimer=0;detectFloor=++detectId;manualCandidate=true;
    candidateIndex=(candidateIndex+(event.deltaY>=0?1:-1)+candidates.length)%candidates.length;render();
  },{passive:false});
  canvas.addEventListener('pointerup',endGesture);canvas.addEventListener('pointercancel',endGesture);canvas.addEventListener('lostpointercapture',endGesture);
  canvas.addEventListener('dblclick',event=>{
    if(busy)return;const index=hit(point(event));if(tool==='select'&&index>=0&&items[index].type==='text'){startText({x:items[index].x,y:items[index].y},index);return;}if(selection&&tool==='select'&&index<0)finish('complete');
  });
  canvas.addEventListener('contextmenu',event=>{event.preventDefault();action('cancel');});
  function startText(p,index=-1){
    textEdit={p,index};updateCursor();text.value=index>=0?items[index].text:'';const current=index>=0?items[index]:style();text.style.left=`${p.x*innerWidth/W}px`;text.style.top=`${p.y*innerHeight/H}px`;text.style.fontSize=`${current.font*innerWidth/W}px`;text.style.color=current.color;text.style.maxWidth=`${Math.max(60,innerWidth-p.x*innerWidth/W-8)}px`;text.style.display='block';text.focus();
  }
  function commitText(){
    if(!textEdit)return;const edit=textEdit;const content=text.value;const before=edit.index>=0?items[edit.index].text:'';textEdit=null;text.style.display='none';
    if(content!==before){remember();if(edit.index>=0){if(content)items[edit.index].text=content;else items.splice(edit.index,1);}else if(content)items.push({type:'text',x:edit.p.x,y:edit.p.y,text:content,...style()});}render();
  }
  text.addEventListener('blur',commitText);text.addEventListener('input',()=>{text.style.width='130px';text.style.width=`${Math.max(130,text.scrollWidth+8)}px`;text.style.height='45px';text.style.height=`${Math.max(45,text.scrollHeight+8)}px`;});
  text.addEventListener('keydown',event=>{if(event.key==='Escape'){event.stopPropagation();textEdit=null;text.style.display='none';canvas.focus();}else if(event.key==='Enter'&&(event.ctrlKey||event.metaKey)){event.preventDefault();event.stopPropagation();commitText();}});
  function exportImage(){commitText();if(!selection)return null;const full=document.createElement('canvas');full.width=W;full.height=H;drawScene(full.getContext('2d'));const output=document.createElement('canvas');output.width=selection.w;output.height=selection.h;output.getContext('2d').drawImage(full,selection.x,selection.y,selection.w,selection.h,0,0,selection.w,selection.h);return output.toDataURL('image/png');}
  function finish(action){if(busy||!selection)return;const result=exportImage();busy=true;updateCursor();send({action,image:result,x:selection.x,y:selection.y,width:selection.w,height:selection.h});}
  function toast(message){busy=false;$('toast').textContent=message;$('toast').style.display='block';setTimeout(()=>{$('toast').style.display='none';},5000);}
  document.addEventListener('keydown',event=>{
    if(event.target===text||event.target.matches('input,select')||event.isComposing)return;
    if(pinned){if(event.key==='Escape')send({action:'cancel'});if(event.ctrlKey&&event.key.toLowerCase()==='c'){event.preventDefault();send({action:'complete',image:pinnedImage});}return;}
    if(event.key==='Escape'){event.preventDefault();action('cancel');}else if(event.key==='Enter'){event.preventDefault();finish('complete');}
    else if(event.ctrlKey||event.metaKey){const key=event.key.toLowerCase();if(key==='z'){event.preventDefault();event.shiftKey?redo():undo();}if(key==='y'){event.preventDefault();redo();}if(key==='s'){event.preventDefault();finish('save');}if(key==='c'){event.preventDefault();finish('complete');}}
    else if(['Delete','Backspace'].includes(event.key)&&selected>=0){event.preventDefault();remember();items.splice(selected,1);selected=-1;render();}
    else{const key=event.key.toLowerCase(),value={v:'select',r:'rect',o:'ellipse',a:'arrow',p:'pen',m:'mosaic',t:'text'}[key];if(value){event.preventDefault();choose(value);}}
  });
  for(const id of ['color','width','font'])$(id).addEventListener('input',()=>{
    if(selected>=0&&items[selected]){remember();Object.assign(items[selected],style());render();}else{updatePreview();positionToolbar();}
  });
  $('pin').addEventListener('pointerdown',event=>{if(event.button===0&&!event.target.closest('button'))send({action:'drag'});});
  $('pin').addEventListener('dblclick',event=>{if(!event.target.closest('button'))send({action:'cancel'});});
  $('pin').addEventListener('wheel',event=>{event.preventDefault();const factor=event.deltaY>0?.9:1.1;send({action:'zoom',width:Math.round(innerWidth*devicePixelRatio*factor),height:Math.round(innerHeight*devicePixelRatio*factor)});},{passive:false});
  document.querySelectorAll('[data-pin]').forEach(button=>button.addEventListener('click',()=>{const action=button.dataset.pin;if(action==='close')send({action:'cancel'});else send({action:action==='save'?'save':'complete',image:pinnedImage});}));
  async function receive(message){
    if(message.type==='capture'){
      clearCandidate();visualCache.clear();windows=message.windows||[];language=message.language||'zh-CN';setup();W=message.width;H=message.height;canvas.width=W;canvas.height=H;image.src=message.image;await image.decode();render();send({action:'shown'});
    }else if(message.type==='candidates'){
      if(message.window&&Array.isArray(message.visual)){
        if(visualCache.size>=4&&!visualCache.has(regionKey(message.window)))visualCache.delete(visualCache.keys().next().value);
        visualCache.set(regionKey(message.window),message.visual);
      }
      if(message.id<detectFloor||message.id>detectId||selection||gesture||!hover||busy||pinned)return;
      const root=candidateWindow;if(!root)return;
      const current=candidate();
      candidates=[...(message.regions||[]),...(visualCache.get(regionKey(root))||[])].filter(r=>Number.isFinite(r.x)&&Number.isFinite(r.y)&&r.w>=24&&r.h>=24&&r.x>=root.x&&r.y>=root.y&&r.x+r.w<=root.x+root.w&&r.y+r.h<=root.y+root.h&&inRect(hover,r));
      candidates=candidates.filter((r,i,a)=>a.findIndex(v=>v.x===r.x&&v.y===r.y&&v.w===r.w&&v.h===r.h)===i);
      if(!candidates.some(r=>r.x===root.x&&r.y===root.y&&r.w===root.w&&r.h===root.h))candidates.push(root);
      candidates.sort((a,b)=>a.w*a.h-b.w*b.h);
      candidateIndex=!manualCandidate?0:Math.max(0,candidates.findIndex(r=>current&&r.x===current.x&&r.y===current.y&&r.w===current.w&&r.h===current.h));render();
    }else if(message.type==='error')toast(message.message);
    else if(message.type==='idle')busy=false;
    else if(message.type==='pinned'){
      pinned=true;busy=false;pinnedImage=message.image;document.body.classList.add('pinned');$('pin').querySelector('img').src=message.image;image.src='';canvas.width=canvas.height=1;items=[];history=[];future=[];
    }
  }
  if(window.chrome?.webview)window.chrome.webview.addEventListener('message',event=>receive(event.data).catch(error=>toast(String(error))));
  window.addEventListener('screenshot-input',event=>receive(event.detail).catch(error=>toast(String(error))));
  window.addEventListener('resize',()=>{if(!pinned)render();});
  send({action:'ready'});
})();
