// The process object: signals, clocks, resource use, umask, cwd.
import assert from 'node:assert/strict';
import { test, done } from './lib.mjs';

test('process.kill with a handler: SIGUSR1, SIGINT, SIGTERM', async () => {
    for (const sig of ['SIGUSR1', 'SIGINT', 'SIGTERM']) {
        const got = new Promise((resolve) => process.once(sig, resolve));
        process.kill(process.pid, sig);
        assert.equal(await got, sig);
    }
});

test('signal 0 checks a process exists', () => {
    assert.equal(process.kill(process.pid, 0), true);
    assert.throws(() => process.kill(999999, 0), { code: 'ESRCH' });
});

test('hrtime and hrtime.bigint move forward', async () => {
    const a = process.hrtime.bigint();
    const t = process.hrtime();
    await new Promise((r) => setTimeout(r, 20));
    const d = process.hrtime(t);
    assert.ok(d[0] * 1e9 + d[1] >= 19e6, String(d));
    assert.ok(process.hrtime.bigint() - a >= 19_000_000n);
});

test('memoryUsage', () => {
    const m = process.memoryUsage();
    assert.ok(m.rss > 10e6 && m.heapTotal > 0 && m.heapUsed > 0, JSON.stringify(m));
    assert.ok(process.memoryUsage.rss() > 10e6);
});

test('resourceUsage and cpuUsage', () => {
    let x = 0;
    for (let i = 0; i < 5e6; i++) x += i;
    const r = process.resourceUsage();
    assert.ok(r.userCPUTime > 0 && r.maxRSS > 10000, JSON.stringify(r));
    const c = process.cpuUsage();
    assert.ok(c.user > 0, JSON.stringify(c));
    assert.ok(process.cpuUsage(c).user >= 0);
    return x;
});

test('uptime', () => assert.ok(process.uptime() > 0));

test('umask', () => {
    const old = process.umask(0o077);
    assert.equal(process.umask(), 0o077);
    process.umask(old);
    assert.equal(old, 0o022);
});

test('chdir and cwd', () => {
    const old = process.cwd();
    process.chdir('/tmp');
    assert.equal(process.cwd(), '/tmp');
    assert.throws(() => process.chdir('/no/such/dir'), { code: 'ENOENT' });
    process.chdir(old);
});

test('ids, title, pid, ppid, env, argv, execPath', () => {
    assert.equal(process.getuid(), 0);
    assert.equal(process.geteuid(), 0);
    assert.equal(process.getgid(), 0);
    assert.deepEqual(process.getgroups(), [0].slice(0, process.getgroups().length));
    assert.ok(process.pid > 1 && process.ppid > 0);
    assert.equal(process.execPath, '/data/bin/node');
    assert.ok(process.argv[1].endsWith('process.test.mjs'));
    process.env.OXIDENIX_TEST_VAR = 'x';
    assert.equal(process.env.OXIDENIX_TEST_VAR, 'x');
    process.title = 'renamed';
    assert.equal(process.title, 'renamed');
});

test('nextTick, queueMicrotask and emitWarning', async () => {
    const order = [];
    await new Promise((resolve) => {
        setImmediate(() => { order.push('immediate'); resolve(); });
        process.nextTick(() => order.push('tick'));
        queueMicrotask(() => order.push('micro'));
    });
    assert.deepEqual(order.sort(), ['immediate', 'micro', 'tick']);
    assert.equal(order.length, 3);
    const w = new Promise((resolve) => process.once('warning', resolve));
    process.emitWarning('careful', 'TestWarning');
    assert.equal((await w).name, 'TestWarning');
});

test('report and constrainedMemory', () => {
    assert.ok(process.constrainedMemory() >= 0);
    const r = process.report.getReport();
    assert.equal(r.header.osName, 'oxidenix');
});

await done();
