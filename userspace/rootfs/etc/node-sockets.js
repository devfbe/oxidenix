// Node.js over the Linux server's AF_UNIX sockets: child_process (spawn,
// exec, execFile, execSync, spawnSync, fork with its IPC channel, sockets
// and servers passed as handles) and net servers and clients on a Unix path.
// Node is not part of the self-tests (CI never builds it): run it with
// `OXIDENIX_NODE=1 OXIDENIX_AUTORUN=<script> cargo run`, the script saying
// `/data/bin/node /etc/node-sockets.js` (see README.md).
'use strict';
const cp = require('child_process');
const net = require('net');
const fs = require('fs');

if (process.argv[2] === 'child') {
    let server = null;
    process.on('message', (m, handle) => {
        if (m === 'ping') {
            process.send('pong');
        } else if (m === 'socket') {
            handle.end('from the child through a passed socket\n');
            process.send('wrote');
        } else if (m === 'server') {
            server = handle;
            handle.on('connection', (s) => s.end('the child accepted\n'));
            process.send('serving');
        } else if (m === 'exit') {
            if (server) server.close();
            process.disconnect();
        }
    });
    return;
}

let failures = 0;
function check(name, ok) {
    console.log(name.padEnd(60) + (ok ? 'ok' : 'FAIL'));
    if (!ok) failures++;
}

const once = (emitter, event) => new Promise((resolve) => emitter.once(event, (...args) => resolve(args)));

function readAll(stream) {
    return new Promise((resolve) => {
        let data = '';
        stream.setEncoding('utf8');
        stream.on('data', (d) => (data += d));
        stream.on('end', () => resolve(data));
    });
}

async function main() {
    check('execSync', cp.execSync('echo hello').toString() === 'hello\n');
    const r = cp.spawnSync('cat', [], { input: 'piped in' });
    check('spawnSync with input', r.status === 0 && r.stdout.toString() === 'piped in');
    let failed = false;
    try {
        cp.execSync('exit 3', { stdio: 'pipe' });
    } catch (e) {
        failed = e.status === 3;
    }
    check('execSync of a failing command throws its status', failed);

    await new Promise((res) =>
        cp.exec('echo $((6 * 7))', (err, out) => {
            check('exec through the shell', !err && out === '42\n');
            res();
        }),
    );
    await new Promise((res) =>
        cp.execFile('/bin/ls', ['/'], (err, out) => {
            check('execFile', !err && out.split('\n').includes('data'));
            res();
        }),
    );

    {
        const c = cp.spawn('sh', ['-c', 'read x; echo got:$x; echo oops >&2; exit 5']);
        const out = readAll(c.stdout);
        const err = readAll(c.stderr);
        c.stdin.end('abc\n');
        const [code] = await once(c, 'close');
        check('spawn with piped stdin, stdout and stderr', code === 5 && (await out) === 'got:abc\n' && (await err) === 'oops\n');
    }

    {
        // A lot of output through the pipe, more than its buffer.
        const c = cp.spawn('sh', ['-c', 'i=0; while [ $i -lt 2000 ]; do echo line $i; i=$((i+1)); done']);
        const out = await readAll(c.stdout);
        const [code] = await once(c, 'close');
        check('spawn: 2000 lines of output', code === 0 && out.split('\n').length === 2001 && out.includes('line 1999\n'));
    }

    {
        // A pipeline of children, one's stdout the next one's stdin.
        const a = cp.spawn('sh', ['-c', 'echo one; echo two; echo three']);
        const b = cp.spawn('grep', ['t'], { stdio: [a.stdout, 'pipe', 'inherit'] });
        const out = await readAll(b.stdout);
        check('one child piped into another', out === 'two\nthree\n');
    }

    const path = '/tmp/node.sock';
    try {
        fs.unlinkSync(path);
    } catch {}

    {
        const server = net.createServer((s) => {
            s.setEncoding('utf8');
            s.on('data', (d) => s.end('echo:' + d));
        });
        server.listen(path);
        await once(server, 'listening');
        check('net server on a Unix path', fs.statSync(path).isSocket());
        const client = net.connect(path);
        await once(client, 'connect');
        client.write('hi');
        const answer = await readAll(client);
        check('net client on a Unix path', answer === 'echo:hi');
        server.close();
        await once(server, 'close');
        try {
            fs.unlinkSync(path);
        } catch {}
    }

    const child = cp.fork(__filename, ['child']);
    {
        child.send('ping');
        const [m] = await once(child, 'message');
        check('fork: messages both ways over the IPC channel', m === 'pong');
    }

    {
        // A connected socket handed to the child, which answers on it.
        const server = net.createServer();
        server.listen(path);
        await once(server, 'listening');
        server.once('connection', (s) => child.send('socket', s));
        const client = net.connect(path);
        const answer = await readAll(client);
        const [m] = await once(child, 'message');
        check('fork: a socket passed to the child', m === 'wrote' && answer === 'from the child through a passed socket\n');
        server.close();
        try {
            fs.unlinkSync(path);
        } catch {}
    }

    {
        // A listening server handed to the child, which accepts on it.
        const server = net.createServer();
        server.listen(path);
        await once(server, 'listening');
        child.send('server', server);
        const [m] = await once(child, 'message');
        // Closing a server removes its path (libuv unlinks it): the
        // socket inode, renamed, still leads to the child's listener.
        const moved = path + '.moved';
        fs.renameSync(path, moved);
        server.close();
        const client = net.connect(moved);
        const answer = await readAll(client);
        check('fork: a server passed to the child', m === 'serving' && answer === 'the child accepted\n');
        fs.unlinkSync(moved);
    }

    child.send('exit');
    const [code] = await once(child, 'exit');
    check('fork: the child exits once disconnected', code === 0);
    try {
        fs.unlinkSync(path);
    } catch {}

    console.log(failures ? 'sockets.js: FAILURES' : 'sockets.js: all ok');
    process.exitCode = failures ? 1 : 0;
}

main().catch((e) => {
    console.log('sockets.js: exception', e);
    process.exitCode = 1;
});
