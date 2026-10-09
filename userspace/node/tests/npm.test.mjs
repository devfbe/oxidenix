// An npm-style workload without npm: a project on /data with a
// node_modules tree (package.json "main" and "exports", nested
// dependencies, scoped packages, CommonJS and ESM), installed from a
// tarball-like bundle (gzip + JSON), resolved by Node's loader, and a small
// "build" that reads, transforms and writes many files.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import zlib from 'node:zlib';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import { test, done, scratch } from './lib.mjs';

const root = await scratch('/data', 'npm');

// The "registry": packages as file maps.
const registry = {
    'left-pad': {
        'package.json': { name: 'left-pad', version: '1.3.0', main: 'index.js' },
        'index.js': "module.exports = (s, n, c = ' ') => String(s).padStart(n, c);",
    },
    '@ox/util': {
        'package.json': {
            name: '@ox/util', version: '2.0.0', type: 'module',
            exports: { '.': { import: './esm/index.js', require: './cjs/index.cjs' }, './package.json': './package.json' },
            dependencies: { 'left-pad': '^1.3.0' },
        },
        'esm/index.js': "import pad from 'left-pad'; export const id = (n) => pad(n, 6, '0'); export const kind = 'esm';",
        'cjs/index.cjs': "const pad = require('left-pad'); exports.id = (n) => pad(n, 6, '0'); exports.kind = 'cjs';",
    },
    'deep': {
        'package.json': { name: 'deep', version: '0.1.0', main: 'lib/main.js', dependencies: { 'left-pad': '1.0.0' } },
        'lib/main.js': "module.exports = { pad: require('left-pad'), version: require('left-pad/package.json').version };",
        // A nested copy with another version, as npm puts conflicting ones.
        'node_modules/left-pad/package.json': { name: 'left-pad', version: '1.0.0', main: 'index.js' },
        'node_modules/left-pad/index.js': "module.exports = (s, n) => 'old:' + String(s).padStart(n);",
    },
};

test('install: unpack gzip bundles into node_modules', async () => {
    for (const [name, files] of Object.entries(registry)) {
        const bundle = zlib.gzipSync(JSON.stringify(files));
        const unpacked = JSON.parse(zlib.gunzipSync(bundle));
        for (const [file, content] of Object.entries(unpacked)) {
            const dest = path.join(root, 'node_modules', name, file);
            await fs.mkdir(path.dirname(dest), { recursive: true });
            await fs.writeFile(dest, typeof content === 'string' ? content : JSON.stringify(content, null, 2));
        }
    }
    await fs.writeFile(path.join(root, 'package.json'), JSON.stringify({ name: 'app', version: '1.0.0', type: 'commonjs' }));
    const lock = {};
    for (const name of Object.keys(registry)) lock[name] = JSON.parse(await fs.readFile(path.join(root, 'node_modules', name, 'package.json'))).version;
    await fs.writeFile(path.join(root, 'package-lock.json'), JSON.stringify(lock));
    assert.deepEqual(lock, { 'left-pad': '1.3.0', '@ox/util': '2.0.0', deep: '0.1.0' });
});

test('require: main, exports conditions, nested versions', () => {
    const require = createRequire(path.join(root, 'index.js'));
    assert.equal(require('left-pad')('7', 3, '0'), '007');
    assert.equal(require('@ox/util').kind, 'cjs');
    assert.equal(require('@ox/util').id(42), '000042');
    assert.equal(require('deep').version, '1.0.0');
    assert.equal(require('deep').pad('x', 2), 'old: x');
    assert.throws(() => require('@ox/util/esm/index.js'), { code: 'ERR_PACKAGE_PATH_NOT_EXPORTED' });
    assert.equal(require.resolve('left-pad'), path.join(root, 'node_modules/left-pad/index.js'));
});

test('import: the ESM condition', async () => {
    await fs.writeFile(path.join(root, 'main.mjs'), "export { id, kind } from '@ox/util';");
    const m = await import(pathToFileURL(path.join(root, 'main.mjs')).href);
    assert.equal(m.kind, 'esm');
    assert.equal(m.id(5), '000005');
});

test('build: 500 source files read, transformed, bundled, written', async () => {
    const src = path.join(root, 'src');
    await fs.mkdir(src, { recursive: true });
    await Promise.all(Array.from({ length: 500 }, (_, i) =>
        fs.writeFile(path.join(src, `m${i}.js`), `// module ${i}\nexport const v${i} = ${i};\n`)));
    const names = (await fs.readdir(src)).filter((n) => n.endsWith('.js')).sort();
    assert.equal(names.length, 500);
    const parts = await Promise.all(names.map(async (n) => (await fs.readFile(path.join(src, n), 'utf8')).replace(/^\/\/.*\n/, '').replace('export const', 'const')));
    const out = path.join(root, 'dist');
    await fs.mkdir(out, { recursive: true });
    await fs.writeFile(path.join(out, 'bundle.js'), parts.join('') + 'module.exports = v499;\n');
    await fs.writeFile(path.join(out, 'bundle.js.gz'), zlib.gzipSync(await fs.readFile(path.join(out, 'bundle.js'))));
    const require = createRequire(path.join(root, 'index.js'));
    assert.equal(require('./dist/bundle.js'), 499);
});

test('clean up', () => fs.rm(root, { recursive: true }));

await done();
