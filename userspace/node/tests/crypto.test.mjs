// node:crypto and Web Crypto: hashes, MACs, randomness, key derivation,
// ciphers, key pairs and signatures (OpenSSL in the static node).
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { test, done } from './lib.mjs';

test('hashes', () => {
    assert.equal(crypto.createHash('sha256').update('abc').digest('hex'), 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad');
    assert.equal(crypto.createHash('md5').update('abc').digest('hex'), '900150983cd24fb0d6963f7d28e17f72');
    assert.equal(crypto.createHash('sha512').update('').digest('base64').length, 88);
    assert.equal(crypto.hash('sha1', 'abc'), 'a9993e364706816aba3e25717850c26c9cd0d89d');
});

test('hmac', () => {
    const mac = crypto.createHmac('sha256', 'key').update('The quick brown fox jumps over the lazy dog').digest('hex');
    assert.equal(mac, 'f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8');
});

test('randomBytes, randomInt, randomUUID, getRandomValues', async () => {
    const a = crypto.randomBytes(32), b = crypto.randomBytes(32);
    assert.equal(a.length, 32);
    assert.notDeepEqual(a, b);
    const c = await new Promise((res, rej) => crypto.randomBytes(16, (e, buf) => (e ? rej(e) : res(buf))));
    assert.equal(c.length, 16);
    const n = crypto.randomInt(10, 20);
    assert.ok(n >= 10 && n < 20);
    assert.match(crypto.randomUUID(), /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    const arr = crypto.getRandomValues(new Uint32Array(8));
    assert.ok(arr.some((x) => x !== 0));
});

test('pbkdf2 and scrypt, sync and async', async () => {
    const expect = '120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b';
    assert.equal(crypto.pbkdf2Sync('password', 'salt', 1, 32, 'sha256').toString('hex'), expect);
    const k = await new Promise((res, rej) => crypto.pbkdf2('password', 'salt', 1, 32, 'sha256', (e, d) => (e ? rej(e) : res(d))));
    assert.equal(k.toString('hex'), expect);
    const s = crypto.scryptSync('password', 'salt', 16, { N: 1024 });
    const s2 = await new Promise((res, rej) => crypto.scrypt('password', 'salt', 16, { N: 1024 }, (e, d) => (e ? rej(e) : res(d))));
    assert.deepEqual(s, s2);
    const h = crypto.hkdfSync('sha256', 'key', 'salt', 'info', 32);
    assert.equal(h.byteLength, 32);
});

test('ciphers: aes-256-cbc, aes-256-gcm, chacha20-poly1305', () => {
    const key = crypto.randomBytes(32), iv = crypto.randomBytes(16);
    const c = crypto.createCipheriv('aes-256-cbc', key, iv);
    const enc = Buffer.concat([c.update('secret message'), c.final()]);
    const d = crypto.createDecipheriv('aes-256-cbc', key, iv);
    assert.equal(Buffer.concat([d.update(enc), d.final()]).toString(), 'secret message');
    for (const alg of ['aes-256-gcm', 'chacha20-poly1305']) {
        const nonce = crypto.randomBytes(12);
        const g = crypto.createCipheriv(alg, key, nonce, { authTagLength: 16 });
        const ct = Buffer.concat([g.update('authenticated'), g.final()]);
        const tag = g.getAuthTag();
        const gd = crypto.createDecipheriv(alg, key, nonce, { authTagLength: 16 });
        gd.setAuthTag(tag);
        assert.equal(Buffer.concat([gd.update(ct), gd.final()]).toString(), 'authenticated');
        tag[0] ^= 1;
        const bad = crypto.createDecipheriv(alg, key, nonce, { authTagLength: 16 });
        bad.setAuthTag(tag);
        bad.update(ct);
        assert.throws(() => bad.final());
    }
    assert.ok(crypto.getCiphers().includes('aes-128-ctr'));
});

test('key pairs and signatures: RSA, ECDSA, Ed25519; ECDH; X25519', async () => {
    const rsa = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
    const sig = crypto.sign('sha256', Buffer.from('data'), rsa.privateKey);
    assert.ok(crypto.verify('sha256', Buffer.from('data'), rsa.publicKey, sig));
    const ct = crypto.publicEncrypt(rsa.publicKey, Buffer.from('hi'));
    assert.equal(crypto.privateDecrypt(rsa.privateKey, ct).toString(), 'hi');
    const ec = await new Promise((res, rej) => crypto.generateKeyPair('ec', { namedCurve: 'P-256' }, (e, pub, priv) => (e ? rej(e) : res({ pub, priv }))));
    assert.ok(crypto.verify('sha256', Buffer.from('x'), ec.pub, crypto.sign('sha256', Buffer.from('x'), ec.priv)));
    const ed = crypto.generateKeyPairSync('ed25519');
    assert.ok(crypto.verify(null, Buffer.from('y'), ed.publicKey, crypto.sign(null, Buffer.from('y'), ed.privateKey)));
    const a = crypto.createECDH('prime256v1'), b = crypto.createECDH('prime256v1');
    a.generateKeys();
    b.generateKeys();
    assert.deepEqual(a.computeSecret(b.getPublicKey()), b.computeSecret(a.getPublicKey()));
    const x1 = crypto.generateKeyPairSync('x25519'), x2 = crypto.generateKeyPairSync('x25519');
    assert.deepEqual(crypto.diffieHellman({ privateKey: x1.privateKey, publicKey: x2.publicKey }),
        crypto.diffieHellman({ privateKey: x2.privateKey, publicKey: x1.publicKey }));
});

test('webcrypto: digest, HMAC, AES-GCM, ECDSA, PBKDF2', async () => {
    const { subtle } = globalThis.crypto;
    const digest = await subtle.digest('SHA-256', new TextEncoder().encode('abc'));
    assert.equal(Buffer.from(digest).toString('hex').slice(0, 8), 'ba7816bf');
    const hmac = await subtle.generateKey({ name: 'HMAC', hash: 'SHA-256' }, true, ['sign', 'verify']);
    const mac = await subtle.sign('HMAC', hmac, new Uint8Array([1, 2, 3]));
    assert.ok(await subtle.verify('HMAC', hmac, mac, new Uint8Array([1, 2, 3])));
    const aes = await subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
    const iv = globalThis.crypto.getRandomValues(new Uint8Array(12));
    const ct = await subtle.encrypt({ name: 'AES-GCM', iv }, aes, new TextEncoder().encode('web'));
    assert.equal(new TextDecoder().decode(await subtle.decrypt({ name: 'AES-GCM', iv }, aes, ct)), 'web');
    const ec = await subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-384' }, true, ['sign', 'verify']);
    const sig = await subtle.sign({ name: 'ECDSA', hash: 'SHA-384' }, ec.privateKey, new Uint8Array([9]));
    assert.ok(await subtle.verify({ name: 'ECDSA', hash: 'SHA-384' }, ec.publicKey, sig, new Uint8Array([9])));
    const base = await subtle.importKey('raw', new TextEncoder().encode('pw'), 'PBKDF2', false, ['deriveBits']);
    const bits = await subtle.deriveBits({ name: 'PBKDF2', hash: 'SHA-256', salt: new Uint8Array(8), iterations: 1000 }, base, 256);
    assert.equal(bits.byteLength, 32);
    const jwk = await subtle.exportKey('jwk', ec.publicKey);
    assert.equal(jwk.crv, 'P-384');
});

test('timingSafeEqual and KeyObject', () => {
    assert.ok(crypto.timingSafeEqual(Buffer.from('abc'), Buffer.from('abc')));
    const k = crypto.createSecretKey(Buffer.alloc(16, 1));
    assert.equal(k.symmetricKeySize, 16);
});

await done();
