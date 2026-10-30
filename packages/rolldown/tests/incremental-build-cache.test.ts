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
test('`meta` returned by `transform` is still there for reused modules', async () => {
  const run = async () => {
    const seen: unknown[] = [];
    const bundle = await rolldown({
      input: './main.js',
      cwd: root,
      plugins: [
        {
          name: 'meta',
          transform(_code, id) {
            if (id.endsWith('main.js')) return { meta: { meta: { tag: 'from transform' } } };
          },
          renderChunk(_code, chunk) {
            for (const id of chunk.moduleIds) {
              if (id.endsWith('main.js')) seen.push(this.getModuleInfo(id)?.meta.meta);
            }
            return null;
          },
        },
      ],
      experimental: { incrementalBuildCache: { dir: cacheDir } },
    });
    await bundle.generate({ format: 'esm' });
    await bundle.close();
    return seen;
  };
  expect(await run()).toEqual([{ tag: 'from transform' }]);
  expect(await run()).toEqual([{ tag: 'from transform' }]);
});

test('ids of one file that differ by query share their dependencies', async () => {
  // Like an SFC compiler: the main id's `transform` reads `data.txt` and declares it; the
  // `?variant` id is produced in `load` from what that transform read, without declaring it.
  fs.writeFileSync(
    path.join(root, 'main.js'),
    `import { dep } from './dep.js';\nimport variant from './dep.js?variant';\nconsole.log(dep, variant);\n`,
  );
  const run = async () => {
    const loaded: string[] = [];
    let seen = '';
    const bundle = await rolldown({
      input: './main.js',
      cwd: root,
      plugins: [
        {
          name: 'sfc-like',
          load(id) {
            if (!id.endsWith('dep.js?variant')) return;
            loaded.push('variant');
            return `export default ${JSON.stringify(seen || fs.readFileSync(path.join(root, 'data.txt'), 'utf8'))};`;
          },
          transform(code, id) {
            if (!id.endsWith('dep.js')) return;
            this.addWatchFile(path.join(root, 'data.txt'));
            seen = fs.readFileSync(path.join(root, 'data.txt'), 'utf8');
            return code;
          },
        },
      ],
      experimental: { incrementalBuildCache: { dir: cacheDir } },
    });
    const output = await bundle.generate({ format: 'esm' });
    await bundle.close();
    return { loaded, code: output.output[0].code };
  };
  expect((await run()).code).toContain('"one"');
  expect((await run()).loaded).toEqual([]);
  fs.writeFileSync(path.join(root, 'data.txt'), 'two');
  const rebuilt = await run();
  expect(rebuilt.loaded).toEqual(['variant']);
  expect(rebuilt.code).toContain('"two"');
});

test('`meta` of a module whose hooks run again comes only from this build', async () => {
  const run = async () => {
    let seen: unknown;
    const bundle = await rolldown({
      input: './main.js',
      cwd: root,
      plugins: [
        {
          name: 'meta',
          transform(code, id) {
            if (id.endsWith('main.js') && code.includes('tagged'))
              return { meta: { meta: { tag: 1 } } };
          },
          renderChunk(_code, chunk) {
            const id = chunk.moduleIds.find((m) => m.endsWith('main.js'));
            if (id) seen = this.getModuleInfo(id)?.meta.meta;
            return null;
          },
        },
      ],
      experimental: { incrementalBuildCache: { dir: cacheDir } },
    });
    await bundle.generate({ format: 'esm' });
    await bundle.close();
    return seen;
  };
  fs.writeFileSync(path.join(root, 'main.js'), `console.log('tagged');\n`);
  expect(await run()).toEqual({ tag: 1 });
  fs.writeFileSync(path.join(root, 'main.js'), `console.log('plain');\n`);
  expect(await run()).toBeUndefined();
});

test('`meta` nobody reads in a build is still kept for the next one', async () => {
  let read = true;
  const run = async () => {
    let seen: unknown;
    const bundle = await rolldown({
      input: './main.js',
      cwd: root,
      plugins: [
        {
          name: 'meta',
          transform(_code, id) {
            if (id.endsWith('main.js')) return { meta: { meta: { tag: 1 } } };
          },
          renderChunk(_code, chunk) {
            const id = chunk.moduleIds.find((m) => m.endsWith('main.js'));
            if (id && read) seen = this.getModuleInfo(id)?.meta.meta;
            return null;
          },
        },
      ],
      experimental: { incrementalBuildCache: { dir: cacheDir } },
    });
    await bundle.generate({ format: 'esm' });
    await bundle.close();
    return seen;
  };
  expect(await run()).toEqual({ tag: 1 });
  read = false;
  await run();
  read = true;
  expect(await run()).toEqual({ tag: 1 });
});

test('no id of a file is kept when one of its ids emitted a file', async () => {
  // The main id declares `data.txt` and emits a file, so it is never recorded; the `?variant` id
  // built from what it read must not be reused with its dependency missing.
  fs.writeFileSync(
    path.join(root, 'main.js'),
    `import { dep } from './dep.js';\nimport variant from './dep.js?variant';\nconsole.log(dep, variant);\n`,
  );
  const run = async () => {
    let seen = '';
    const bundle = await rolldown({
      input: './main.js',
      cwd: root,
      plugins: [
        {
          name: 'sfc-like',
          load(id) {
            if (!id.endsWith('dep.js?variant')) return;
            return `export default ${JSON.stringify(seen || fs.readFileSync(path.join(root, 'data.txt'), 'utf8'))};`;
          },
          transform(code, id) {
            if (!id.endsWith('dep.js')) return;
            this.addWatchFile(path.join(root, 'data.txt'));
            seen = fs.readFileSync(path.join(root, 'data.txt'), 'utf8');
            this.emitFile({ type: 'asset', fileName: 'side.txt', source: seen });
            return code;
          },
        },
      ],
      experimental: { incrementalBuildCache: { dir: cacheDir } },
    });
    const output = await bundle.generate({ format: 'esm' });
    await bundle.close();
    return output.output[0].code;
  };
  expect(await run()).toContain('"one"');
  fs.writeFileSync(path.join(root, 'data.txt'), 'two');
  expect(await run()).toContain('"two"');
});

test('a directory passed to addWatchFile invalidates when its entries change', async () => {
  const data = path.join(root, 'data');
  fs.mkdirSync(data);
  fs.writeFileSync(path.join(data, 'a.txt'), '');
  const run = async () => {
    const bundle = await rolldown({
      input: './main.js',
      cwd: root,
      plugins: [
        {
          name: 'list-dir',
          transform(_code, id) {
            if (!id.endsWith('main.js')) return;
            this.addWatchFile(data);
            return `console.log(${JSON.stringify(fs.readdirSync(data).sort())});`;
          },
        },
      ],
      experimental: { incrementalBuildCache: { dir: cacheDir } },
    });
    const output = await bundle.generate({ format: 'esm' });
    await bundle.close();
    return output.output[0].code;
  };
  expect(await run()).toContain('"a.txt"');
  fs.writeFileSync(path.join(data, 'b.txt'), '');
  expect(await run()).toContain('"b.txt"');
});

test('a new file that wins a resolution re-runs the importer', async () => {
  fs.writeFileSync(path.join(root, 'main.js'), `import { lib } from './lib';\nconsole.log(lib);\n`);
  fs.mkdirSync(path.join(root, 'lib'));
  fs.writeFileSync(path.join(root, 'lib', 'index.js'), `export const lib = 'index';\n`);
  const first = await build();
  expect(first.files['main.js']).toContain('"index"');
  fs.writeFileSync(path.join(root, 'lib.js'), `export const lib = 'file';\n`);
  const rebuilt = await build();
  expect(rebuilt.files['main.js']).toEqual((await build({ cache: false })).files['main.js']);
  expect(rebuilt.files['main.js']).toContain('"file"');
});
