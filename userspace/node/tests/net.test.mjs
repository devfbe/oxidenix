// Networking: TCP (node:net), UDP (node:dgram), DNS, HTTP and HTTPS
// servers and clients over loopback, fetch, and a request to the outside.
import assert from 'node:assert/strict';
import net from 'node:net';
import dgram from 'node:dgram';
import dns from 'node:dns';
import http from 'node:http';
import https from 'node:https';
import tls from 'node:tls';
import fs from 'node:fs';
import { once } from 'node:events';
import { test, done } from './lib.mjs';

const listen = async (server, host = '127.0.0.1') => {
    server.listen(0, host);
    await once(server, 'listening');
    return server.address().port;
};

test('TCP: echo server and client, 1 MiB both ways', async () => {
    const server = net.createServer((s) => s.pipe(s));
    const port = await listen(server);
    const client = net.connect(port, '127.0.0.1');
    await once(client, 'connect');
    assert.equal(client.remotePort, port);
    const data = Buffer.alloc(1 << 20, 'z');
    let got = 0;
    client.on('data', (c) => { got += c.length; if (got === data.length) client.end(); });
    client.write(data);
    await once(client, 'close');
    assert.equal(got, data.length);
    server.close();
    await once(server, 'close');
});

test('TCP: connection refused, and a server on an address in use', async () => {
    const c = net.connect(1, '127.0.0.1');
    const [err] = await once(c, 'error');
    assert.equal(err.code, 'ECONNREFUSED');
    const a = net.createServer();
    const port = await listen(a);
    const b = net.createServer();
    b.listen(port, '127.0.0.1');
    const [e2] = await once(b, 'error');
    assert.equal(e2.code, 'EADDRINUSE');
    a.close();
});

test('UDP: datagrams over loopback', async () => {
    const server = dgram.createSocket('udp4');
    server.bind(0, '127.0.0.1');
    await once(server, 'listening');
    server.on('message', (msg, rinfo) => server.send(Buffer.concat([msg, Buffer.from('!')]), rinfo.port, rinfo.address));
    const client = dgram.createSocket('udp4');
    client.send('ping', server.address().port, '127.0.0.1');
    const [msg] = await once(client, 'message');
    assert.equal(msg.toString(), 'ping!');
    client.close();
    server.close();
});

test('DNS: lookup of localhost and of the host name (getaddrinfo, thread pool)', async () => {
    const a = await dns.promises.lookup('localhost', { family: 4 });
    assert.equal(a.address, '127.0.0.1');
    const all = await dns.promises.lookup('localhost', { all: true });
    assert.ok(all.some((x) => x.address === '127.0.0.1'));
});

test('DNS: resolve4 through the resolver (c-ares, UDP to 10.0.2.3)', async () => {
    const addrs = await dns.promises.resolve4('example.com');
    assert.ok(addrs.length > 0 && net.isIPv4(addrs[0]), String(addrs));
    assert.deepEqual(dns.getServers(), ['10.0.2.3']);
}, { timeout: 30000 });

test('HTTP: server and client, keep-alive, chunked, a 5 MiB body', async () => {
    const big = Buffer.alloc(5 << 20, 'h');
    const server = http.createServer((req, res) => {
        if (req.url === '/big') return res.end(big);
        let body = '';
        req.on('data', (c) => (body += c));
        req.on('end', () => { res.setHeader('x-method', req.method); res.write('got:'); res.end(body); });
    });
    const port = await listen(server);
    const agent = new http.Agent({ keepAlive: true });
    const request = (path, method, body) => new Promise((resolve, reject) => {
        const req = http.request({ port, host: '127.0.0.1', path, method, agent }, (res) => {
            const chunks = [];
            res.on('data', (c) => chunks.push(c));
            res.on('end', () => resolve({ res, body: Buffer.concat(chunks) }));
        });
        req.on('error', reject);
        req.end(body);
    });
    const r1 = await request('/', 'POST', 'hello');
    assert.equal(r1.body.toString(), 'got:hello');
    assert.equal(r1.res.headers['x-method'], 'POST');
    assert.equal(r1.res.headers['transfer-encoding'], 'chunked');
    const r2 = await request('/big', 'GET');
    assert.equal(r2.body.length, big.length);
    agent.destroy();
    server.close();
});

test('fetch against a local server, JSON both ways', async () => {
    const server = http.createServer((req, res) => {
        let body = '';
        req.on('data', (c) => (body += c));
        req.on('end', () => { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify({ echo: JSON.parse(body) })); });
    });
    const port = await listen(server);
    const r = await fetch(`http://127.0.0.1:${port}/x`, { method: 'POST', body: JSON.stringify({ a: 1 }), headers: { 'content-type': 'application/json' } });
    assert.equal(r.status, 200);
    assert.deepEqual(await r.json(), { echo: { a: 1 } });
    server.close();
});

test('HTTPS: TLS server and client with a self-signed certificate', async () => {
    const key = fs.readFileSync(new URL('./localhost-key.pem', import.meta.url));
    const cert = fs.readFileSync(new URL('./localhost-cert.pem', import.meta.url));
    const server = https.createServer({ key, cert }, (req, res) => res.end(`secure ${req.socket.getProtocol()}`));
    const port = await listen(server);
    const body = await new Promise((resolve, reject) => {
        https.get({ host: 'localhost', port, ca: cert, servername: 'localhost' }, (res) => {
            let b = '';
            res.on('data', (c) => (b += c));
            res.on('end', () => resolve(b));
        }).on('error', reject);
    });
    assert.equal(body, 'secure TLSv1.3');
    const sock = tls.connect({ host: '127.0.0.1', port, ca: cert, servername: 'localhost' });
    await once(sock, 'secureConnect');
    assert.ok(sock.authorized);
    sock.destroy();
    server.close();
});

test('HTTP to the outside world (QEMU user network, DNS, TCP)', async () => {
    const r = await fetch('http://example.com/', { signal: AbortSignal.timeout(20000) });
    assert.equal(r.status, 200);
    assert.match(await r.text(), /Example Domain/);
}, { timeout: 30000 });

await done();
