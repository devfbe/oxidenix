// node:os: every function, with what oxidenix reports.
import assert from 'node:assert/strict';
import os from 'node:os';
import { test, done } from './lib.mjs';

test('arch, machine, platform, type, endianness, EOL, devNull', () => {
    assert.equal(os.arch(), 'x64');
    assert.equal(os.machine(), 'x86_64');
    assert.equal(os.platform(), 'linux');
    assert.equal(os.type(), 'oxidenix');
    assert.equal(os.endianness(), 'LE');
    assert.equal(os.EOL, '\n');
    assert.equal(os.devNull, '/dev/null');
});

test('release and version', () => {
    assert.match(os.release(), /^\d+\.\d+/);
    assert.ok(os.version().length > 0);
});

test('hostname', () => assert.equal(os.hostname(), 'oxidenix'));

test('cpus and availableParallelism', () => {
    const cpus = os.cpus();
    assert.equal(cpus.length, 4);
    for (const c of cpus) {
        // libuv reads the speed from cpufreq in /sys, which a virtual
        // machine without a cpufreq driver lacks: 0, as on Linux there.
        assert.ok(c.speed >= 0, JSON.stringify(c));
        assert.ok(c.model.length > 0);
        assert.ok(c.times.user >= 0 && c.times.idle > 0, JSON.stringify(c.times));
    }
    assert.equal(os.availableParallelism(), 4);
});

test('totalmem, freemem', () => {
    const total = os.totalmem(), free = os.freemem();
    assert.ok(total > 200e6 && total < 300e6, `total ${total}`);
    assert.ok(free > 0 && free < total, `free ${free}`);
});

test('loadavg and uptime', () => {
    const l = os.loadavg();
    assert.equal(l.length, 3);
    assert.ok(l.every((x) => x >= 0));
    assert.ok(os.uptime() > 0);
});

test('networkInterfaces', () => {
    const ifs = os.networkInterfaces();
    assert.deepEqual(ifs.lo, [{ address: '127.0.0.1', netmask: '255.0.0.0', family: 'IPv4', mac: '00:00:00:00:00:00', internal: true, cidr: '127.0.0.1/8' }]);
    assert.equal(ifs.eth0[0].address, '10.0.2.15');
    assert.equal(ifs.eth0[0].mac, '52:54:00:12:34:56');
    assert.equal(ifs.eth0[0].internal, false);
});

test('homedir, tmpdir, userInfo', () => {
    assert.equal(os.homedir(), '/root');
    assert.equal(os.tmpdir(), '/tmp');
    const u = os.userInfo();
    assert.equal(u.uid, 0);
    assert.equal(u.gid, 0);
    assert.equal(u.username, 'root');
    assert.equal(u.homedir, '/root');
});

test('getPriority and setPriority', () => {
    assert.equal(os.getPriority(), 0);
    os.setPriority(0, 5);
    assert.equal(os.getPriority(0), 5);
    os.setPriority(0);
    assert.equal(os.getPriority(), 0);
});

test('constants', () => {
    assert.equal(os.constants.signals.SIGINT, 2);
    assert.equal(os.constants.errno.ENOENT, 2);
    assert.equal(os.constants.priority.PRIORITY_NORMAL, 0);
});

await done();
