// Run the built client in a real Chromium process, with no devices or prior
// browser permissions. Node >=22, no npm dependencies. See README.md.
import {spawn} from 'node:child_process';
import {mkdtemp, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import assert from 'node:assert/strict';

const [browser, url, screenshot, mode = 'normal'] = process.argv.slice(2);
assert(browser && url && screenshot, 'node smoke.mjs <chrome> <url> <screenshot.png> [normal|unsupported|denied]');
const profile = await mkdtemp(join(tmpdir(), 'sdroxide-startup-'));
const proc = spawn(browser, [
  '--headless=new', '--no-first-run', '--no-default-browser-check', '--disable-sync',
  '--disable-extensions', '--remote-debugging-port=0', '--remote-allow-origins=*',
  '--enable-unsafe-swiftshader', '--use-angle=swiftshader', '--window-size=1280,900',
  '--user-data-dir=' + profile, 'about:blank',
], {stdio: ['ignore', 'ignore', 'pipe'], windowsHide: true});
let stderr = '';
let cdp;
try {
  const endpoint = await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error('Chrome did not expose DevTools: ' + stderr)), 20000);
    proc.stderr.on('data', b => {
      stderr += b.toString();
      const match = stderr.match(/DevTools listening on (ws:\/\/[^\s]+)/);
      if (match) {clearTimeout(timeout); resolve(match[1]);}
    });
    proc.once('exit', code => {clearTimeout(timeout); reject(new Error('Chrome exited ' + code));});
  });
  const base = endpoint.replace('ws://', 'http://').split('/devtools/')[0];
  const tabs = await (await fetch(base + '/json/list')).json();
  cdp = new WebSocket(tabs.find(t => t.type === 'page').webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {cdp.onopen = resolve; cdp.onerror = reject;});
  let seq = 0;
  const pending = new Map();
  const failures = [], errors = [];
  let received = 0;
  cdp.onmessage = ({data}) => {
    const m = JSON.parse(data);
    if (m.id) {
      const p = pending.get(m.id);
      if (!p) return;
      pending.delete(m.id);
      if (m.error) p.reject(new Error(JSON.stringify(m.error))); else p.resolve(m.result);
    }
    if (m.method === 'Runtime.exceptionThrown') errors.push(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text);
    if (m.method === 'Network.responseReceived' && m.params.response.status >= 400 &&
        /\.(js|wasm)(\?|$)/.test(m.params.response.url)) failures.push(m.params.response);
    if (m.method === 'Network.loadingFailed' && !m.params.canceled) failures.push(m.params);
    if (m.method === 'Network.webSocketFrameReceived') received++;
  };
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    const id = ++seq;
    pending.set(id, {resolve, reject});
    cdp.send(JSON.stringify({id, method, params}));
  });
  await send('Runtime.enable');
  await send('Page.enable');
  await send('Network.enable');
  await send('Page.addScriptToEvaluateOnNewDocument', {source: `
    window.__pttPermissionCalls = 0;
    const denied = () => { window.__pttPermissionCalls++; return Promise.reject(new DOMException('Test refusal', 'NotAllowedError')); };
    if ('${mode}' === 'unsupported') {
      Object.defineProperty(navigator, 'serial', {value: undefined});
      Object.defineProperty(navigator, 'hid', {value: undefined});
      Object.defineProperty(navigator, 'requestMIDIAccess', {value: undefined});
    } else {
      Object.defineProperty(navigator, 'requestMIDIAccess', {value: denied});
    }
  `});
  await send('Page.navigate', {url});
  const started = Date.now();
  while (received === 0 && errors.length === 0 && failures.length === 0 && Date.now() - started < 60000) {
    await new Promise(r => setTimeout(r, 250));
  }
  await new Promise(r => setTimeout(r, 1500));
  const result = await send('Runtime.evaluate', {expression: `JSON.stringify({
    title: document.title, permissionCalls: window.__pttPermissionCalls,
    width: document.querySelector('canvas')?.width, height: document.querySelector('canvas')?.height
  })`, returnByValue: true});
  const state = JSON.parse(result.result.value);
  const shot = await send('Page.captureScreenshot', {format: 'png'});
  await writeFile(screenshot, Buffer.from(shot.data, 'base64'));
  console.log(JSON.stringify({url, mode, state, received, errors, failures}, null, 2));
  assert.equal(errors.length, 0, 'uncaught startup exception');
  assert.equal(failures.length, 0, 'failed module or network request');
  assert(received > 0, 'no server WebSocket response: WASM did not connect');
  assert(state.width > 0 && state.height > 0, 'canvas not initialized');
  assert.equal(state.permissionCalls, 0, 'startup asked for device permission');
} finally {
  if (cdp?.readyState === WebSocket.OPEN) {
    cdp.send(JSON.stringify({id: 999999, method: 'Browser.close'}));
    cdp.close();
  } else { proc.kill(); }
}
