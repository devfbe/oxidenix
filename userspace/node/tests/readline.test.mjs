// node:readline on standard input (run-node.sh pipes three lines in), and
// on a stream of its own.
import assert from 'node:assert/strict';
import readline from 'node:readline';
import readlinep from 'node:readline/promises';
import { PassThrough } from 'node:stream';
import { test, done } from './lib.mjs';

test('lines from stdin (a pipe)', async () => {
    const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
    const lines = [];
    for await (const line of rl) lines.push(line);
    assert.deepEqual(lines, ['first line', 'second line', 'third']);
});

test('question() on a stream', async () => {
    const input = new PassThrough(), output = new PassThrough();
    const rl = readlinep.createInterface({ input, output, terminal: false });
    setTimeout(() => input.write('forty-two\n'), 5);
    assert.equal(await rl.question('answer? '), 'forty-two');
    rl.close();
});

await done();
