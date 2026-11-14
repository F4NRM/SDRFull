// Test the actual wasm-bindgen inline module, with no browser hardware.
// node --test tools/web-startup/permissions.test.mjs
import {readFileSync} from 'node:fs';
import {execFileSync} from 'node:child_process';
import assert from 'node:assert/strict';
import {test} from 'node:test';

const source = process.env.PTT_TEST_REV
  ? execFileSync('git', ['show', process.env.PTT_TEST_REV + ':crates/sdroxide-ui/src/operator_ptt_web.rs'], {encoding: 'utf8'})
  : readFileSync(new URL('../../crates/sdroxide-ui/src/operator_ptt_web.rs', import.meta.url), 'utf8');
const js = source.split('inline_js = r#"')[1].split('"#)]')[0];
let asks = 0;
Object.defineProperty(globalThis, 'navigator', {configurable: true, value: {
  serial: {getPorts: async () => []},
  hid: {getDevices: async () => []},
  requestMIDIAccess: async () => { asks++; throw new DOMException('denied', 'NotAllowedError'); },
}});
const module = await import('data:text/javascript;base64,' + Buffer.from(js).toString('base64'));

test('listing and restoring a MIDI slot never ask for permission', async () => {
  assert.deepEqual(await module.pttMidiList(), []);
  await module.pttMidiOpen(0, 1, 'missing');
  assert.equal(asks, 0, 'loading settings or opening Controls must not prompt');
});
test('only the Allow gesture asks; refusal is an error without a rejected promise', async () => {
  const before = asks;
  assert.match(await module.pttMidiRequest(), /denied|NotAllowedError/);
  assert.equal(asks, before + 1);
  assert.deepEqual(await module.pttMidiList(), []);
});
test('the same Allow gesture can retry after refusal', async () => {
  navigator.requestMIDIAccess = async () => { asks++; return {inputs: new Map([['pedal', {
    id: 'pedal', name: 'Pedal', state: 'connected', addEventListener() {}, removeEventListener() {},
  }]])}; };
  assert.equal(await module.pttMidiRequest(), '');
  assert.deepEqual(await module.pttMidiList(), [['pedal', 'Pedal']]);
  assert.equal(await module.pttMidiOpen(0, 2, 'pedal'), '');
  assert.equal(module.pttMidiAlive(0, 2), true);
  await module.pttMidiClose(0, 2);
});
test('missing API objects and methods are unsupported', () => {
  assert.equal(module.pttSupport(), 7);
  navigator.serial = undefined;
  navigator.hid = {};
  navigator.requestMIDIAccess = undefined;
  assert.equal(module.pttSupport(), 0);
});
