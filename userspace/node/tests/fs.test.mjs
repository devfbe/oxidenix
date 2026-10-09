// node:fs on the root tmpfs (/tmp) and on the ext2 data disk (/data):
// the sync, callback and promise APIs, descriptors, streams, directories,
// links, times, permissions, watching.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { pipeline } from 'node:stream/promises';
import { once } from 'node:events';
import { test, done, scratch } from './lib.mjs';

for (const base of ['/tmp', '/data']) {
    const dir = await scratch(base, 'fs');
    const p = (name) => path.join(dir, name);

    test(`${base}: write, read, append (sync)`, () => {
        fs.writeFileSync(p('a.txt'), 'hello');
        fs.appendFileSync(p('a.txt'), ' world');
        assert.equal(fs.readFileSync(p('a.txt'), 'utf8'), 'hello world');
        assert.equal(fs.existsSync(p('a.txt')), true);
        assert.equal(fs.existsSync(p('nope')), false);
    });

    test(`${base}: callbacks`, async () => {
        await new Promise((resolve, reject) =>
            fs.writeFile(p('cb.txt'), 'callback', (e) => (e ? reject(e) : resolve())));
        const data = await new Promise((resolve, reject) =>
            fs.readFile(p('cb.txt'), 'utf8', (e, d) => (e ? reject(e) : resolve(d))));
        assert.equal(data, 'callback');
    });

    test(`${base}: promises, stat and lstat`, async () => {
        await fsp.writeFile(p('b.bin'), Buffer.alloc(10000, 7));
        const st = await fsp.stat(p('b.bin'));
        assert.equal(st.size, 10000);
        assert.ok(st.isFile() && !st.isDirectory());
        assert.ok(st.mtimeMs > 1e12, `mtime ${st.mtimeMs}`);
        assert.ok(st.ino > 0 && st.nlink >= 1);
        const big = await fsp.stat(p('b.bin'), { bigint: true });
        assert.equal(big.size, 10000n);
        assert.ok((await fsp.stat(dir)).isDirectory());
    });

    test(`${base}: descriptors: open, write, read at positions, truncate, fsync`, async () => {
        const fh = await fsp.open(p('fd.bin'), 'w+');
        await fh.write(Buffer.from('0123456789'), 0, 10, 0);
        await fh.write('ab', 4);
        const buf = Buffer.alloc(10);
        const { bytesRead } = await fh.read(buf, 0, 10, 0);
        assert.equal(bytesRead, 10);
        assert.equal(buf.toString(), '0123ab6789');
        await fh.truncate(3);
        await fh.sync();
        assert.equal((await fh.stat()).size, 3);
        await fh.close();
        const fd = fs.openSync(p('fd.bin'), 'r');
        assert.equal(fs.readSync(fd, buf, 0, 10, null), 3);
        fs.closeSync(fd);
        fs.ftruncateSync(fs.openSync(p('fd.bin'), 'r+'), 0);
    });

    test(`${base}: directories: mkdir -p, readdir with types, opendir, rm -r`, async () => {
        await fsp.mkdir(p('x/y/z'), { recursive: true });
        await fsp.writeFile(p('x/y/f'), '');
        const entries = await fsp.readdir(p('x/y'), { withFileTypes: true });
        const kinds = Object.fromEntries(entries.map((e) => [e.name, e.isDirectory() ? 'dir' : 'file']));
        assert.deepEqual(kinds, { z: 'dir', f: 'file' });
        const names = [];
        for await (const e of await fsp.opendir(p('x'))) names.push(e.name);
        assert.deepEqual(names, ['y']);
        assert.deepEqual((await fsp.readdir(p('x'), { recursive: true })).sort(), ['y', 'y/f', 'y/z']);
        await fsp.rm(p('x'), { recursive: true });
        assert.equal(fs.existsSync(p('x')), false);
        await assert.rejects(fsp.rmdir(p('x')), { code: 'ENOENT' });
    });

    test(`${base}: rename, copyFile, cp -r, unlink`, async () => {
        await fsp.writeFile(p('r1'), 'r');
        await fsp.rename(p('r1'), p('r2'));
        await fsp.copyFile(p('r2'), p('r3'));
        await assert.rejects(fsp.copyFile(p('r2'), p('r3'), fs.constants.COPYFILE_EXCL), { code: 'EEXIST' });
        await fsp.mkdir(p('tree/sub'), { recursive: true });
        await fsp.writeFile(p('tree/sub/leaf'), 'leaf');
        await fsp.cp(p('tree'), p('tree2'), { recursive: true });
        assert.equal(await fsp.readFile(p('tree2/sub/leaf'), 'utf8'), 'leaf');
        await fsp.unlink(p('r2'));
        assert.equal(await fsp.readFile(p('r3'), 'utf8'), 'r');
    });

    test(`${base}: symlinks, readlink, realpath`, async () => {
        await fsp.writeFile(p('target'), 't');
        await fsp.symlink(p('target'), p('link'));
        assert.equal(await fsp.readlink(p('link')), p('target'));
        assert.ok((await fsp.lstat(p('link'))).isSymbolicLink());
        assert.equal(await fsp.realpath(p('link')), p('target'));
        assert.equal(fs.realpathSync.native(p('link')), p('target'));
    });

    test(`${base}: chmod, access, mkdtemp`, async () => {
        await fsp.writeFile(p('mode'), '');
        await fsp.chmod(p('mode'), 0o640);
        assert.equal((await fsp.stat(p('mode'))).mode & 0o777, 0o640);
        await fsp.access(p('mode'), fs.constants.R_OK);
        await assert.rejects(fsp.access(p('missing')), { code: 'ENOENT' });
        const tmp = await fsp.mkdtemp(p('tmp-'));
        assert.ok((await fsp.stat(tmp)).isDirectory());
    });

    test(`${base}: utimes sets the times stat reports`, async () => {
        await fsp.writeFile(p('times'), '');
        const when = new Date('2020-01-02T03:04:05Z');
        await fsp.utimes(p('times'), when, when);
        const st = await fsp.stat(p('times'));
        assert.equal(st.mtime.getTime(), when.getTime());
        assert.equal(st.atime.getTime(), when.getTime());
    });

    test(`${base}: writing moves mtime`, async () => {
        await fsp.writeFile(p('mtime'), 'a');
        const before = (await fsp.stat(p('mtime'))).mtimeMs;
        await new Promise((r) => setTimeout(r, 1100));
        await fsp.appendFile(p('mtime'), 'b');
        const after = (await fsp.stat(p('mtime'))).mtimeMs;
        assert.ok(after > before, `${before} -> ${after}`);
    });

    test(`${base}: streams: 4 MiB through a pipeline`, async () => {
        const chunk = Buffer.alloc(64 * 1024, 'q');
        const out = fs.createWriteStream(p('stream.bin'));
        for (let i = 0; i < 64; i++) if (!out.write(chunk)) await once(out, 'drain');
        out.end();
        await once(out, 'finish');
        await pipeline(fs.createReadStream(p('stream.bin')), fs.createWriteStream(p('stream2.bin')));
        assert.equal((await fsp.stat(p('stream2.bin'))).size, 4 * 1024 * 1024);
        let total = 0;
        for await (const c of fs.createReadStream(p('stream2.bin'), { start: 10, end: 1033 })) total += c.length;
        assert.equal(total, 1024);
    });

    test(`${base}: fs.watch sees a new file and a change`, async () => {
        const wdir = p('watched');
        await fsp.mkdir(wdir);
        const seen = [];
        const watcher = fs.watch(wdir, (event, name) => seen.push(`${event}:${name}`));
        await new Promise((r) => setTimeout(r, 100));
        await fsp.writeFile(path.join(wdir, 'new'), 'x');
        for (let i = 0; i < 50 && !seen.some((s) => s.endsWith(':new')); i++) await new Promise((r) => setTimeout(r, 50));
        watcher.close();
        assert.ok(seen.some((s) => s.endsWith(':new')), JSON.stringify(seen));
    });

    test(`${base}: fs.promises.watch (async iterator)`, async () => {
        const wdir = p('watched2');
        await fsp.mkdir(wdir);
        const ac = new AbortController();
        const it = fsp.watch(wdir, { signal: ac.signal });
        setTimeout(() => fsp.writeFile(path.join(wdir, 'f'), 'y'), 100);
        try {
            for await (const ev of it) {
                assert.equal(ev.filename, 'f');
                break;
            }
        } finally {
            ac.abort();
        }
    });

    test(`${base}: fs.watchFile polls for changes`, async () => {
        await fsp.writeFile(p('polled'), '1');
        const changed = new Promise((resolve) =>
            fs.watchFile(p('polled'), { interval: 100 }, (cur) => {
                if (cur.size === 5) resolve(cur.size);
            }));
        try {
            await new Promise((r) => setTimeout(r, 300));
            await fsp.writeFile(p('polled'), '12345');
            assert.equal(await changed, 5);
        } finally {
            fs.unwatchFile(p('polled'));
        }
    });

    test(`${base}: errors carry Linux codes`, async () => {
        await assert.rejects(fsp.readFile(p('none')), { code: 'ENOENT', errno: -2 });
        await assert.rejects(fsp.mkdir(dir), { code: 'EEXIST' });
        await assert.rejects(fsp.readdir(p('mode')), { code: 'ENOTDIR' });
        await assert.rejects(fsp.readFile(dir), { code: 'EISDIR' });
    });

    test(`${base}: statfs`, async () => {
        const s = await fsp.statfs(dir);
        assert.ok(s.bsize > 0 && s.blocks > 0, JSON.stringify(s));
    });

    test(`${base}: clean up`, () => fsp.rm(dir, { recursive: true }));
}

await done();
