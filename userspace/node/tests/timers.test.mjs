// Timers and events: setTimeout, setInterval, setImmediate, the promise
// forms, AbortSignal, EventEmitter and EventTarget.
import assert from 'node:assert/strict';
import { EventEmitter, once, on } from 'node:events';
import timersp from 'node:timers/promises';
import { test, done } from './lib.mjs';

test('setTimeout fires in order and not early', async () => {
    const start = performance.now();
    const order = [];
    await new Promise((resolve) => {
        setTimeout(() => order.push(30), 30);
        setTimeout(() => order.push(10), 10);
        setTimeout(() => { order.push(50); resolve(); }, 50);
    });
    assert.deepEqual(order, [10, 30, 50]);
    assert.ok(performance.now() - start >= 49);
});

test('setInterval and clearInterval', async () => {
    let n = 0;
    await new Promise((resolve) => {
        const t = setInterval(() => { if (++n === 5) { clearInterval(t); resolve(); } }, 5);
    });
    assert.equal(n, 5);
});

test('setImmediate, clearTimeout, ref/unref', async () => {
    let fired = false;
    const t = setTimeout(() => (fired = true), 5);
    clearTimeout(t);
    await new Promise((r) => setImmediate(r));
    await new Promise((r) => setTimeout(r, 20));
    assert.equal(fired, false);
    const u = setTimeout(() => {}, 100000);
    u.unref();
    assert.equal(u.hasRef(), false);
});

test('timers/promises: setTimeout, setImmediate, setInterval, abort', async () => {
    assert.equal(await timersp.setTimeout(10, 'v'), 'v');
    assert.equal(await timersp.setImmediate('i'), 'i');
    let n = 0;
    for await (const _ of timersp.setInterval(5)) if (++n === 3) break;
    const ac = new AbortController();
    const p = timersp.setTimeout(10000, null, { signal: ac.signal });
    ac.abort();
    await assert.rejects(p, { name: 'AbortError' });
    await assert.rejects(timersp.setTimeout(1000, null, { signal: AbortSignal.timeout(10) }), { name: 'AbortError' });
});

test('EventEmitter: on, once, error, listeners', async () => {
    const e = new EventEmitter();
    let sum = 0;
    e.on('n', (x) => (sum += x));
    e.emit('n', 2);
    e.emit('n', 3);
    assert.equal(sum, 5);
    setTimeout(() => e.emit('ready', 42), 5);
    assert.deepEqual(await once(e, 'ready'), [42]);
    assert.throws(() => e.emit('error', new Error('boom')), /boom/);
    assert.equal(e.listenerCount('n'), 1);
    setTimeout(() => { e.emit('tick', 1); e.emit('tick', 2); }, 5);
    const got = [];
    for await (const [v] of on(e, 'tick')) { got.push(v); if (got.length === 2) break; }
    assert.deepEqual(got, [1, 2]);
});

test('EventTarget and CustomEvent', () => {
    const t = new EventTarget();
    let detail;
    t.addEventListener('x', (ev) => (detail = ev.detail), { once: true });
    t.dispatchEvent(new CustomEvent('x', { detail: 7 }));
    assert.equal(detail, 7);
});

await done();
