import test from 'node:test';
import assert from 'node:assert/strict';
import { isOtherDevice, isThisDevice, pickerOptions, rowIsCurrent } from '../../web/js/output-pick.js';

const here = { id: 'browser:abc', kind: 'browser', name: 'Chrome on Mac' };
const there = { id: 'browser:xyz', kind: 'browser', name: 'Safari on iPhone' };
const available = [{ kind: 'browser', name: 'This device', needsAddress: false }, { kind: 'nidoo', name: 'Nidoo', needsAddress: true }];

test('isThisDevice / isOtherDevice compare the device id', () => {
  assert.equal(isThisDevice(here, 'abc'), true);
  assert.equal(isThisDevice(there, 'abc'), false);
  assert.equal(isOtherDevice(there, 'abc'), true);
  assert.equal(isOtherDevice(here, 'abc'), false);
  assert.equal(isOtherDevice({ id: 'nidoo:1', kind: 'nidoo' }, 'abc'), false);
  assert.equal(isOtherDevice(null, 'abc'), false);
});

test('the This device row is current only for this device', () => {
  assert.equal(rowIsCurrent(here, available[0], 'abc'), true);
  assert.equal(rowIsCurrent(there, available[0], 'abc'), false);
  assert.equal(rowIsCurrent({ kind: 'nidoo' }, available[1], 'abc'), true);
  assert.equal(rowIsCurrent(null, available[0], 'abc'), false);
});

test('another browser gets its own current row first', () => {
  const opts = pickerOptions({ current: there, available }, 'abc');
  assert.equal(opts.length, 3);
  assert.equal(opts[0].name, 'Safari on iPhone');
  assert.equal(rowIsCurrent(there, opts[0], 'abc'), true);
  assert.equal(pickerOptions({ current: here, available }, 'abc'), available);
});

test('a fixed output with no row is added', () => {
  const opts = pickerOptions({ current: { kind: 'log', name: 'Log' }, available }, 'abc');
  assert.equal(opts[0].kind, 'log');
});
