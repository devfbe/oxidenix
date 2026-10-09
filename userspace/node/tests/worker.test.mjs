// node:worker_threads: workers with messages, shared memory and Atomics,
// transfers, many workers at once, and errors; BroadcastChannel.
import assert from 'node:assert/strict';
import { Worker, MessageChannel, BroadcastChannel } from 'node:worker_threads';
import { once } from 'node:events';
import { test, done } from './lib.mjs';

const run = (code, options = {}) => new Worker(code, { eval: true, ...options });

test('a worker answers a message', async () => {
    const w = run(`const { parentPort } = require('node:worker_threads');
        parentPort.on('message', (n) => { parentPort.postMessage(n * 2); process.exit(0); });`);
    w.postMessage(21);
    const [answer] = await once(w, 'message');
    assert.equal(answer, 42);
    const [code] = await once(w, 'exit');
    assert.equal(code, 0);
});

test('SharedArrayBuffer and Atomics.wait/notify across threads', async () => {
    const sab = new SharedArrayBuffer(8);
    const word = new Int32Array(sab);
    const w = run(`const { workerData } = require('node:worker_threads');
        const w = new Int32Array(workerData);
        Atomics.wait(w, 0, 0);
        Atomics.store(w, 1, Atomics.load(w, 0) + 1);
        Atomics.notify(w, 1);`, { workerData: sab });
    await new Promise((r) => setTimeout(r, 50));
    Atomics.store(word, 0, 41);
    Atomics.notify(word, 0);
    await once(w, 'exit');
    assert.equal(Atomics.load(word, 1), 42);
});

test('transferring an ArrayBuffer and a MessagePort', async () => {
    const { port1, port2 } = new MessageChannel();
    const w = run(`const { workerData } = require('node:worker_threads');
        workerData.port.on('message', (buf) => { workerData.port.postMessage(new Uint8Array(buf)[0]); workerData.port.close(); });`,
    { workerData: { port: port2 }, transferList: [port2] });
    const buf = new ArrayBuffer(1024);
    new Uint8Array(buf)[0] = 7;
    port1.postMessage(buf, [buf]);
    assert.equal(buf.byteLength, 0);
    const [v] = await once(port1, 'message');
    assert.equal(v, 7);
    port1.close();
    await once(w, 'exit');
});

test('eight workers computing at once', async () => {
    const results = await Promise.all(Array.from({ length: 8 }, (_, i) => {
        const w = run(`const { parentPort, workerData } = require('node:worker_threads');
            let s = 0; for (let k = 0; k < 2e6; k++) s += k % (workerData + 2);
            parentPort.postMessage(s);`, { workerData: i });
        return once(w, 'message').then(([s]) => s);
    }));
    assert.equal(results.length, 8);
    assert.ok(results.every((s) => s > 0));
});

test('an exception in a worker reaches the parent', async () => {
    const w = run(`throw new Error('from the worker')`);
    const [err] = await once(w, 'error');
    assert.match(err.message, /from the worker/);
});

test('terminate() stops a busy worker', async () => {
    const w = run(`for (;;) {}`);
    await new Promise((r) => setTimeout(r, 50));
    assert.equal(await w.terminate(), 1);
});

test('BroadcastChannel between threads', async () => {
    const bc = new BroadcastChannel('ox');
    const w = run(`const { BroadcastChannel } = require('node:worker_threads');
        const bc = new BroadcastChannel('ox'); bc.postMessage('hello'); bc.close();`);
    const got = new Promise((resolve) => (bc.onmessage = (ev) => resolve(ev.data)));
    assert.equal(await got, 'hello');
    bc.close();
    await once(w, 'exit');
});

test('a worker past its resourceLimits ends with ERR_WORKER_OUT_OF_MEMORY', async () => {
    const w = run(`const keep = []; for (;;) keep.push(new Array(10000).fill(keep.length));`,
        { resourceLimits: { maxOldGenerationSizeMb: 16 } });
    const [err] = await once(w, 'error');
    assert.equal(err.code, 'ERR_WORKER_OUT_OF_MEMORY');
});

await done();
