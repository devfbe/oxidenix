// node:zlib: gzip, deflate, brotli and zstd, sync, async (thread pool) and
// as streams.
import assert from 'node:assert/strict';
import zlib from 'node:zlib';
import { promisify } from 'node:util';
import { pipeline } from 'node:stream/promises';
import { Readable, Writable } from 'node:stream';
import { test, done } from './lib.mjs';

const text = Buffer.from('oxidenix '.repeat(10000));

test('gzip and gunzip, sync', () => {
    const z = zlib.gzipSync(text);
    assert.ok(z.length < text.length / 20);
    assert.deepEqual(zlib.gunzipSync(z), text);
});

test('deflate, inflate, raw and unzip, async', async () => {
    const d = await promisify(zlib.deflate)(text, { level: 9 });
    assert.deepEqual(await promisify(zlib.inflate)(d), text);
    const raw = await promisify(zlib.deflateRaw)(text);
    assert.deepEqual(await promisify(zlib.inflateRaw)(raw), text);
    assert.deepEqual(await promisify(zlib.unzip)(zlib.gzipSync(text)), text);
});

test('brotli', async () => {
    const b = await promisify(zlib.brotliCompress)(text);
    assert.deepEqual(zlib.brotliDecompressSync(b), text);
});

test('zstd', () => {
    if (!zlib.zstdCompressSync) return;
    assert.deepEqual(zlib.zstdDecompressSync(zlib.zstdCompressSync(text)), text);
});

test('streams: 8 MiB through gzip and gunzip', async () => {
    let produced = 0, consumed = 0;
    const source = Readable.from((function* () {
        for (let i = 0; i < 128; i++) {
            produced += 65536;
            yield Buffer.alloc(65536, i % 256);
        }
    })());
    const sink = new Writable({ write(chunk, _, cb) { consumed += chunk.length; cb(); } });
    await pipeline(source, zlib.createGzip(), zlib.createGunzip(), sink);
    assert.equal(consumed, produced);
});

test('crc32', () => {
    if (!zlib.crc32) return;
    assert.equal(zlib.crc32('hello'), 907060870);
});

test('corrupt input is an error', () => {
    assert.throws(() => zlib.gunzipSync(Buffer.from('not gzip')), { code: 'Z_DATA_ERROR' });
});

await done();
