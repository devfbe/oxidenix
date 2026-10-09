// The pure-JavaScript modules over the runtime: path, url, util, buffer,
// stream, string_decoder, querystring, assert, perf_hooks, async_hooks, vm,
// v8, WebAssembly, ESM imports (static, dynamic, JSON, CommonJS interop).
import assert from 'node:assert/strict';
import path from 'node:path';
import util from 'node:util';
import { Buffer } from 'node:buffer';
import { Readable, Transform, Writable, pipeline } from 'node:stream';
import { pipeline as pipelinep } from 'node:stream/promises';
import { StringDecoder } from 'node:string_decoder';
import querystring from 'node:querystring';
import { performance, PerformanceObserver, monitorEventLoopDelay } from 'node:perf_hooks';
import { AsyncLocalStorage, createHook, executionAsyncId } from 'node:async_hooks';
import vm from 'node:vm';
import v8 from 'node:v8';
import { createRequire } from 'node:module';
import { test, done } from './lib.mjs';
import data from './fixtures/data.json' with { type: 'json' };
import { double } from './fixtures/esm-helper.mjs';

test('path', () => {
    assert.equal(path.join('/a', 'b', '../c'), '/a/c');
    assert.equal(path.resolve('/x', 'y'), '/x/y');
    assert.equal(path.relative('/a/b', '/a/c/d'), '../c/d');
    assert.deepEqual(path.parse('/home/u/f.txt'), { root: '/', dir: '/home/u', base: 'f.txt', ext: '.txt', name: 'f' });
    assert.equal(path.posix.sep, '/');
    assert.ok(path.matchesGlob('/a/b.js', '/a/*.js'));
});

test('url and URLSearchParams', () => {
    const u = new URL('https://user:pw@example.com:8080/p/a?x=1&y=2#h');
    assert.equal(u.port, '8080');
    assert.equal(u.searchParams.get('y'), '2');
    u.searchParams.append('z', 'a b');
    assert.equal(u.search, '?x=1&y=2&z=a+b');
    assert.equal(new URL('../q', 'http://h/a/b/c').href, 'http://h/a/q');
    assert.ok(URL.canParse('file:///etc/passwd'));
    assert.equal(querystring.stringify({ a: [1, 2], b: 'c d' }), 'a=1&a=2&b=c%20d');
});

test('util: format, inspect, promisify, types, parseArgs, styleText', async () => {
    assert.equal(util.format('%s=%d %j', 'n', 42, { a: 1 }), 'n=42 {"a":1}');
    assert.equal(util.inspect({ a: [1, { b: 2 }] }, { depth: 0 }), '{ a: [Array] }');
    const sleep = util.promisify((ms, cb) => setTimeout(() => cb(null, ms), ms));
    assert.equal(await sleep(5), 5);
    assert.ok(util.types.isPromise(Promise.resolve()));
    assert.ok(util.isDeepStrictEqual({ a: [1] }, { a: [1] }));
    const { values } = util.parseArgs({ args: ['--n', '3', '-v'], options: { n: { type: 'string' }, v: { type: 'boolean', short: 'v' } } });
    assert.deepEqual({ ...values }, { n: '3', v: true });
    assert.equal(new util.TextDecoder().decode(new util.TextEncoder().encode('ü')), 'ü');
});

test('buffer: encodings, slicing, compare, Blob, atob', async () => {
    const b = Buffer.from('héllo wörld');
    assert.equal(Buffer.from(b.toString('base64'), 'base64').toString(), 'héllo wörld');
    assert.equal(b.toString('hex').length, b.length * 2);
    assert.equal(b.subarray(0, 1).toString(), 'h');
    assert.equal(Buffer.compare(Buffer.from('a'), Buffer.from('b')), -1);
    const x = Buffer.alloc(8);
    x.writeBigUInt64BE(0x0102030405060708n);
    assert.equal(x.readUInt32LE(4), 0x08070605);
    assert.equal(await new Blob(['ab', 'cd']).text(), 'abcd');
    assert.equal(atob(btoa('hi')), 'hi');
    assert.equal(new StringDecoder('utf8').write(Buffer.from([0xe2, 0x82, 0xac])), '€');
});

test('stream: Readable, Transform, pipeline, async iteration, web streams', async () => {
    const upper = new Transform({ transform(c, _, cb) { cb(null, c.toString().toUpperCase()); } });
    const out = [];
    await pipelinep(Readable.from(['a', 'b', 'c']), upper, new Writable({ write(c, _, cb) { out.push(c.toString()); cb(); } }));
    assert.deepEqual(out, ['A', 'B', 'C']);
    await new Promise((resolve, reject) => pipeline(Readable.from(['x']), new Writable({ write(c, _, cb) { cb(); } }), (e) => (e ? reject(e) : resolve())));
    const rs = new ReadableStream({ start(c) { c.enqueue('w'); c.enqueue('s'); c.close(); } });
    const parts = [];
    for await (const p of rs.pipeThrough(new TransformStream({ transform(ch, c) { c.enqueue(ch + ch); } }))) parts.push(p);
    assert.deepEqual(parts, ['ww', 'ss']);
    assert.deepEqual(await Readable.from([1, 2, 3]).map((x) => x * 2).toArray(), [2, 4, 6]);
});

test('perf_hooks: marks, measures, observers, event loop delay', async () => {
    performance.mark('a');
    await new Promise((r) => setTimeout(r, 10));
    performance.mark('b');
    const m = performance.measure('ab', 'a', 'b');
    assert.ok(m.duration >= 9, String(m.duration));
    const seen = new Promise((resolve) => {
        const obs = new PerformanceObserver((list) => { obs.disconnect(); resolve(list.getEntries()[0].name); });
        obs.observe({ entryTypes: ['mark'] });
    });
    performance.mark('observed');
    assert.equal(await seen, 'observed');
    const h = monitorEventLoopDelay({ resolution: 10 });
    h.enable();
    await new Promise((r) => setTimeout(r, 100));
    h.disable();
    assert.ok(h.max > 0);
    assert.ok(performance.timeOrigin > 1e12);
    assert.ok(performance.eventLoopUtilization().utilization >= 0);
});

test('async_hooks: AsyncLocalStorage across awaits and timers, hooks', async () => {
    const als = new AsyncLocalStorage();
    const value = await als.run({ id: 7 }, async () => {
        await new Promise((r) => setTimeout(r, 5));
        return new Promise((r) => setImmediate(() => r(als.getStore().id)));
    });
    assert.equal(value, 7);
    let inits = 0;
    const hook = createHook({ init() { inits++; } }).enable();
    await new Promise((r) => setTimeout(r, 1));
    hook.disable();
    assert.ok(inits > 0);
    assert.ok(executionAsyncId() >= 0);
});

test('vm: contexts, scripts, timeouts', () => {
    const ctx = vm.createContext({ x: 2 });
    assert.equal(vm.runInContext('x * 21', ctx), 42);
    const script = new vm.Script('y = (typeof y === "number" ? y : 0) + 1');
    const c2 = vm.createContext({});
    script.runInContext(c2);
    script.runInContext(c2);
    assert.equal(c2.y, 2);
    assert.throws(() => vm.runInNewContext('for (;;) {}', {}, { timeout: 50 }), { code: 'ERR_SCRIPT_EXECUTION_TIMEOUT' });
});

test('v8: heap statistics, serialize, snapshot of the heap', () => {
    const s = v8.getHeapStatistics();
    assert.ok(s.total_heap_size > 0 && s.heap_size_limit > 0);
    assert.deepEqual(v8.deserialize(v8.serialize({ a: new Map([[1, 2]]) })), { a: new Map([[1, 2]]) });
});

test('WebAssembly: compile and run a module (add, memory)', async () => {
    // (module (memory (export "mem") 1)
    //   (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))
    const bytes = new Uint8Array([
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f,
        0x03, 0x02, 0x01, 0x00, 0x05, 0x03, 0x01, 0x00, 0x01, 0x07, 0x0d, 0x02, 0x03, 0x6d, 0x65, 0x6d, 0x02, 0x00,
        0x03, 0x61, 0x64, 0x64, 0x00, 0x00, 0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b,
    ]);
    const { instance } = await WebAssembly.instantiate(bytes);
    assert.equal(instance.exports.add(40, 2), 42);
    instance.exports.mem.grow(10);
    assert.equal(instance.exports.mem.buffer.byteLength, 11 * 65536);
    assert.ok(WebAssembly.validate(bytes));
});

test('ESM: static and dynamic imports, JSON, import.meta, CommonJS interop', async () => {
    assert.equal(double(21), 42);
    assert.equal(data.name, 'fixture');
    const dyn = await import('./fixtures/esm-helper.mjs');
    assert.equal(dyn.double, double);
    assert.equal(import.meta.filename, '/usr/lib/node-tests/modules.test.mjs');
    assert.equal(import.meta.dirname, '/usr/lib/node-tests');
    const require = createRequire(import.meta.url);
    const cjs = require('./fixtures/cjs-helper.cjs');
    assert.equal(cjs.triple(3), 9);
    const { triple } = await import('./fixtures/cjs-helper.cjs');
    assert.equal(triple(2), 6);
    assert.equal(import.meta.resolve('./fixtures/esm-helper.mjs'), 'file:///usr/lib/node-tests/fixtures/esm-helper.mjs');
});

test('structuredClone, Intl, Temporal-free dates', () => {
    const o = structuredClone({ d: new Date(0), m: new Map([[1, { a: 1 }]]) });
    assert.equal(o.d.getTime(), 0);
    assert.equal(new Intl.NumberFormat('de-DE').format(1234567.5), '1.234.567,5');
    assert.equal(new Intl.DateTimeFormat('en-US', { timeZone: 'UTC', month: 'long' }).format(new Date(0)), 'January');
    assert.equal('ß'.localeCompare('ss', 'de', { sensitivity: 'base' }), 0);
});

await done();
