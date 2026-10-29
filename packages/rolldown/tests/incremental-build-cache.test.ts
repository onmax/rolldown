import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { rolldown, type Plugin } from 'rolldown';
import { afterEach, beforeEach, expect, test } from 'vitest';

let root: string;
let cacheDir: string;

beforeEach(() => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), 'rolldown-incremental-build-cache-'));
  cacheDir = path.join(root, '.cache');
  fs.writeFileSync(
    path.join(root, 'main.js'),
    `import { dep } from './dep.js';\nimport { emitted } from './emits.js';\nconsole.log(dep, emitted);\n`,
  );
  fs.writeFileSync(path.join(root, 'dep.js'), `export const dep = 'dep';\n`);
  fs.writeFileSync(path.join(root, 'emits.js'), `export const emitted = 'emitted';\n`);
  fs.writeFileSync(path.join(root, 'data.txt'), 'one');
});

/** The record shards on disk, by file name. */
function records() {
  return Object.fromEntries(
    fs
      .readdirSync(cacheDir)
      .filter((f) => f.endsWith('.json'))
      .map((f) => [f, fs.readFileSync(path.join(cacheDir, f), 'utf8')]),
  );
}

afterEach(() => {
  fs.rmSync(root, { recursive: true, force: true });
});

/** Counts hook calls per module and gives `dep.js` a dependency on `data.txt`. */
function countingPlugin() {
  const transformed: string[] = [];
  const resolved: string[] = [];
  const plugin: Plugin = {
    name: 'counting',
    resolveId(source, importer) {
      if (importer?.startsWith(root)) resolved.push(source);
    },
    transform(code, id) {
      if (!id.startsWith(root)) return;
      const name = path.basename(id);
      transformed.push(name);
      if (name === 'dep.js') {
        const data = path.join(root, 'data.txt');
        this.addWatchFile(data);
        return code.replace(`'dep'`, JSON.stringify(fs.readFileSync(data, 'utf8')));
      }
      if (name === 'emits.js') {
        this.emitFile({ type: 'asset', fileName: 'emitted.txt', source: 'from transform' });
      }
    },
  };
  return { plugin, transformed, resolved };
}

async function build(options: { key?: string; invalidate?: string[]; cache?: boolean } = {}) {
  const counter = countingPlugin();
  const bundle = await rolldown({
    input: './main.js',
    cwd: root,
    plugins: [counter.plugin],
    experimental:
      options.cache === false
        ? {}
        : {
            incrementalBuildCache: {
              dir: cacheDir,
              key: options.key,
              invalidate: options.invalidate,
            },
          },
  });
  const output = await bundle.generate({ format: 'esm' });
  await bundle.close();
  const files = Object.fromEntries(
    output.output.map((o) => [o.fileName, o.type === 'chunk' ? o.code : String(o.source)]),
  );
  return { files, transformed: counter.transformed.sort(), resolved: counter.resolved };
}

test('a new build reuses the records of unchanged modules', async () => {
  const first = await build();
  expect(first.transformed).toEqual(['dep.js', 'emits.js', 'main.js']);
  expect(Object.keys(records()).length).toBeGreaterThan(0);

  const second = await build();
  // `emits.js` emitted a file from `transform`, so it is never reused.
  expect(second.transformed).toEqual(['emits.js']);
  expect(second.resolved).toEqual([]);
  expect(second.files).toEqual(first.files);
  expect(second.files['emitted.txt']).toBe('from transform');
});

test('an edited module runs its hooks again and the output matches a build without cache', async () => {
  await build();
  fs.writeFileSync(
    path.join(root, 'main.js'),
    `import { dep } from './dep.js';\nconsole.log('edited', dep);\n`,
  );
  const edited = await build();
  expect(edited.transformed).toEqual(['main.js']);
  expect(edited.files).toEqual((await build({ cache: false })).files);
});

test('a module reruns when a file it added with addWatchFile changes', async () => {
  await build();
  fs.writeFileSync(path.join(root, 'data.txt'), 'two');
  const rebuilt = await build();
  expect(rebuilt.transformed).toEqual(['dep.js', 'emits.js']);
  expect(rebuilt.files['main.js']).toContain('"two"');
});

test('records written under another key are not reused', async () => {
  await build({ key: 'a' });
  expect((await build({ key: 'a' })).transformed).toEqual(['emits.js']);
  expect((await build({ key: 'b' })).transformed).toEqual(['dep.js', 'emits.js', 'main.js']);
});

test('a build that reuses every record leaves the record files untouched', async () => {
  await build();
  const before = Object.fromEntries(
    Object.keys(records()).map((f) => [f, fs.statSync(path.join(cacheDir, f)).mtimeMs]),
  );
  // `emits.js` is never recorded, so every kept record is a reused one.
  await build();
  const after = Object.fromEntries(
    Object.keys(records()).map((f) => [f, fs.statSync(path.join(cacheDir, f)).mtimeMs]),
  );
  expect(after).toEqual(before);
});

test('files listed in `invalidate` run their hooks again', async () => {
  await build();
  const rebuilt = await build({ invalidate: [path.join(root, 'dep.js')] });
  expect(rebuilt.transformed).toEqual(['dep.js', 'emits.js']);
});

test('a failed build keeps the previous records', async () => {
  await build();
  const before = records();
  fs.writeFileSync(path.join(root, 'main.js'), `import { missing } from './missing.js';\n`);
  await expect(build()).rejects.toThrow();
  expect(records()).toEqual(before);
});
