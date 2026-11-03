// Pure rendering checks: no browser, live clipboard, or user input is touched.
const {readFileSync} = require('node:fs');
const {join} = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const {test} = require('node:test');
const html = readFileSync(join(__dirname, '../assets/settings_window/index.html'), 'utf8');
const rendering = html.slice(html.indexOf('    function renderTransfer()'), html.indexOf('    function stepper('));

function render(tasks, language = 'en') {
  const model = {language, transfer: {tasks, historyLimit: 100, rowHeight: 44}};
  const context = vm.createContext({
    currentModel: () => model, heading: () => '', svg: () => '',
    chip: value => String(value), switchButton: () => '', stepper: () => '',
    escapeHtml: value => String(value).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('"', '&quot;'),
  });
  return vm.runInContext(rendering + '\nrenderTransfer()', context);
}
function task(key, receiving, stateCode = 'waiting') {
  return {key, name: key, receiving, stateCode,
    controllable: ['waiting', 'running', 'paused'].includes(stateCode),
    state: stateCode, files: 1, directories: 0, transferred: '0 B', total: '10 B', speed: '—'};
}
function buttonDisabled(markup, action, key) {
  const button = [...markup.matchAll(/<button\b[^>]*>/g)].map(match => match[0])
    .find(tag => tag.includes(`data-action="${action}"`) && (!key || tag.includes(`data-task="${key}"`)));
  assert.ok(button, `missing ${action} ${key || ''}`);
  return /\bdisabled\b/.test(button);
}

test('all inline settings scripts still parse', () => {
  for (const match of html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/g)) new vm.Script(match[1]);
});
test('sending tasks support pause, resume and cancellation', () => {
  const output = render([task('send', false)]);
  for (const action of ['pause-all', 'pause-task', 'cancel-task']) assert.ok(!buttonDisabled(output, action));
  assert.ok(buttonDisabled(output, 'resume-all'));
  assert.match(output, /data-confirm="cancel-all" >|data-confirm="cancel-all">/);
  assert.match(output, /This transfer can be controlled from either device/);
  assert.doesNotMatch(output, /class="button details"/);
});
test('mixed tasks all render; receiving pause, resume and cancel remain usable', () => {
  const output = render([task('send', false), task('running', true, 'running'), task('paused', true, 'paused')]);
  for (const key of ['send', 'running', 'paused']) assert.match(output, new RegExp(`<strong>${key}</strong>`));
  assert.ok(!buttonDisabled(output, 'pause-task', 'send'));
  assert.ok(!buttonDisabled(output, 'pause-task', 'running'));
  assert.ok(!buttonDisabled(output, 'pause-task', 'paused'));
  assert.ok(!buttonDisabled(output, 'cancel-task', 'paused'));
  assert.ok(!buttonDisabled(output, 'pause-all'));
  assert.ok(!buttonDisabled(output, 'resume-all'));
  assert.match(output, /data-task="paused" >继续/);
});
test('terminal, empty and paused-only task lists have the right bulk capabilities', () => {
  for (const tasks of [[], [task('done', true, 'completed')]]) {
    const output = render(tasks);
    assert.ok(buttonDisabled(output, 'pause-all'));
    assert.ok(buttonDisabled(output, 'resume-all'));
  }
  const output = render([task('paused', true, 'paused')]);
  assert.ok(buttonDisabled(output, 'pause-all'));
  assert.ok(!buttonDisabled(output, 'resume-all'));
  assert.match(render([task('send', false)], 'zh-CN'), /可在发送或接收设备控制此任务/);
});

test('settings summary and task progress expose the same labeled current speed', () => {
  const active = {...task('speed', false, 'running'), speed: '7.7 MiB/s'};
  for (const language of ['en', 'zh-CN']) {
    const output = render([active], language);
    assert.equal((output.match(/7.7 MiB\/s/g) || []).length, 2);
    assert.match(output, language === 'en' ? /Current speed/ : /实时速度/);
  }
});
