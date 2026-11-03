const {readFileSync} = require('node:fs');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const {test} = require('node:test');
const html = readFileSync(require('node:path').join(__dirname, '../assets/transfer_window/index.html'), 'utf8');
const rendering = html.slice(html.indexOf('    function fileDepth('), html.indexOf('    function setupMarquees('));
const file = {key:'f',name:'video.mp4',sizeLabel:'11.5 GiB',state:'Running',stateLabel:'传输中',progress:3,speedLabel:'7.7 MiB/s',isDirectory:false};
function render(files=[file],speedLabel='7.7 MiB/s') {
 const task={files,key:'t',name:'video.mp4',state:'Running',stateLabel:'传输中',speedLabel};
 const ctx=vm.createContext({task,state:{selectedFiles:new Set(),model:{current:[task],history:[]}},escapeHtml:v=>String(v??''),svg:()=>'',statusClass:()=>'',taskIcon:()=>'',pathRow:()=>''});
 return vm.runInContext(rendering+'\ndetailView(task,false)',ctx);
}
test('transfer scripts parse',()=>{for(const m of html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/g))new vm.Script(m[1]);});
test('details and each file progress display current speed',()=>{const out=render();assert.equal((out.match(/7.7 MiB\/s/g)||[]).length,2);assert.match(out,/fact-label">实时速度/);assert.match(out,/class="file-speed"/);});
test('directories do not claim file throughput',()=>{const out=render([{...file,isDirectory:true}]);assert.doesNotMatch(out,/class="file-speed"/);});
test('zero throughput and pause labels remain visible',()=>{assert.match(render([{...file,speedLabel:'0 B/s'}],'0 B/s'),/0 B\/s/);assert.match(render([{...file,speedLabel:'已暂停'}],'已暂停'),/已暂停/);});
