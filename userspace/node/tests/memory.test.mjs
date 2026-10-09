// Large allocations and garbage collector pressure in a 256 MiB machine:
// big buffers outside the heap, typed arrays, many short-lived objects,
// a heap grown and given back, and an allocation that cannot succeed.
import assert from 'node:assert/strict';
import v8 from 'node:v8';
import vm from 'node:vm';
import { test, done } from './lib.mjs';

v8.setFlagsFromString('--expose-gc');
const gc = vm.runInNewContext('gc');

test('a 64 MiB Buffer, written and read', () => {
    const b = Buffer.alloc(64 << 20);
    for (let i = 0; i < b.length; i += 4096) b[i] = (i >> 12) & 0xff;
    let sum = 0;
    for (let i = 0; i < b.length; i += 4096) sum += b[i];
    assert.ok(sum > 0);
});

test('typed arrays: 32 MiB Float64Array', () => {
    const a = new Float64Array(4 << 20);
    for (let i = 0; i < a.length; i++) a[i] = i * 0.5;
    assert.equal(a[a.length - 1], (a.length - 1) * 0.5);
});

test('1 GiB allocated in short-lived objects (scavenges)', () => {
    let keep = 0;
    for (let round = 0; round < 1000; round++) {
        const objs = [];
        for (let i = 0; i < 10000; i++) objs.push({ i, s: 'x' + i, a: [i, i + 1] });
        keep += objs[round % objs.length].i;
    }
    assert.ok(keep > 0);
});

test('a heap of 100 MiB kept, then freed (mark-compact)', () => {
    let big = [];
    for (let i = 0; i < 100; i++) big.push(new Array(128 * 1024).fill(i));
    const used = v8.getHeapStatistics().used_heap_size;
    assert.ok(used > 90e6, String(used));
    big = null;
    gc();
    const after = v8.getHeapStatistics().used_heap_size;
    assert.ok(after < used / 4, `${after} vs ${used}`);
});

test('strings and maps: 1e6 entries', () => {
    const m = new Map();
    for (let i = 0; i < 1e6; i++) m.set('k' + i, i);
    assert.equal(m.get('k999999'), 999999);
});

test('an impossible allocation throws, the process lives on', () => {
    assert.throws(() => Buffer.alloc(2 ** 40));
    assert.throws(() => new ArrayBuffer(Number.MAX_SAFE_INTEGER));
    assert.equal(Buffer.alloc(16).length, 16);
});

await done();
