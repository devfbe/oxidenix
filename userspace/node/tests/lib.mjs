// The Node.js smoke tests' harness: test(name, fn) registers a check,
// done() runs them in order (each with a time limit), prints "ok" or
// "FAIL" per check and sets the exit status.
const tests = [];

export function test(name, fn, { timeout = 20000 } = {}) {
    tests.push({ name, fn, timeout });
}

export async function done() {
    let failed = 0;
    for (const { name, fn, timeout } of tests) {
        let timer;
        const limit = new Promise((_, reject) => {
            timer = setTimeout(() => reject(new Error(`timed out after ${timeout} ms`)), timeout);
        });
        try {
            await Promise.race([fn(), limit]);
            console.log(`  ok   ${name}`);
        } catch (e) {
            failed++;
            console.log(`  FAIL ${name}: ${e && e.stack ? e.stack.split('\n').slice(0, 4).join(' | ') : e}`);
        } finally {
            clearTimeout(timer);
        }
    }
    // A failed check may leave handles open (a watcher, a server): end
    // anyway. After a clean run the process ends by itself, as Node does
    // when nothing is left to wait for, which also checks that.
    if (failed) process.exit(1);
}

// A fresh directory for a test's files under `base`.
export async function scratch(base, name) {
    const { mkdir, rm } = await import('node:fs/promises');
    const dir = `${base}/node-${name}-${process.pid}`;
    await rm(dir, { recursive: true, force: true });
    await mkdir(dir, { recursive: true });
    return dir;
}
