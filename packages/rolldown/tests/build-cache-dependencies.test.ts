import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { rolldown, type InputOptions, type Plugin } from 'rolldown';
import { afterEach, beforeEach, expect, test } from 'vitest';

let root: string;
let cacheDir: string;

beforeEach(() => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), 'rolldown-build-cache-dependencies-'));
  cacheDir = path.join(root, '.cache');
  fs.writeFileSync(
    path.join(root, 'main.js'),
    `import { dep } from './dep.js';\nimport { emitted } from './emits.js';\nconsole.log(dep, emitted);\n`,
  );
  fs.writeFileSync(path.join(root, 'dep.js'), `export const dep = 'dep';\n`);
  fs.writeFileSync(path.join(root, 'emits.js'), `export const emitted = 'emitted';\n`);
  fs.writeFileSync(path.join(root, 'data.txt'), 'one');
});

/** The cache entries on disk, by path under the entries dir. */
function entries() {
  const dir = path.join(cacheDir, 'build-v1');
  if (!fs.existsSync(dir)) return {};
  return Object.fromEntries(
    fs
      .readdirSync(dir)
      .flatMap((d) => fs.readdirSync(path.join(dir, d)).map((f) => path.join(d, f)))
      .filter((f) => !f.endsWith('.tmp'))
      .map((f) => [f, fs.readFileSync(path.join(dir, f), 'latin1')]),
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
            buildCache: {
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

/**
 * A build of `./main.js` with `plugins` and a `transform` hook that lists what it transformed
 * (without a hook of its kind the cache turns itself off).
 */
async function run(plugins: Plugin[] = [], options: InputOptions = {}) {
  const transformed: string[] = [];
  const bundle = await rolldown({
    input: './main.js',
    cwd: root,
    ...options,
    plugins: [
      ...plugins,
      {
        name: 'list-transformed',
        transform(_code, id) {
          if (id.startsWith(root)) transformed.push(path.relative(root, id));
        },
      },
    ],
    experimental: { buildCache: { dir: cacheDir } },
  });
  const output = await bundle.generate({ format: 'esm' });
  await bundle.close();
  const files = Object.fromEntries(
    output.output.map((o) => [o.fileName, o.type === 'chunk' ? o.code : String(o.source)]),
  );
  return { code: output.output[0].code, files, transformed: transformed.sort() };
}

test('a new build reuses the entries of unchanged modules', async () => {
  const first = await build();
  expect(first.transformed).toEqual(['dep.js', 'emits.js', 'main.js']);
  expect(Object.keys(entries()).length).toBeGreaterThan(0);

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

test('entries written under another key are not reused', async () => {
  await build({ key: 'a' });
  expect((await build({ key: 'a' })).transformed).toEqual(['emits.js']);
  expect((await build({ key: 'b' })).transformed).toEqual(['dep.js', 'emits.js', 'main.js']);
});

test('a build that reuses every entry leaves the entry files untouched', async () => {
  const mtimes = () =>
    Object.fromEntries(
      Object.keys(entries()).map((f) => [
        f,
        fs.statSync(path.join(cacheDir, 'build-v1', f)).mtimeMs,
      ]),
    );
  await build();
  const before = mtimes();
  // `emits.js` is never cached, so every kept entry is a reused one.
  await build();
  const after = mtimes();
  expect(after).toEqual(before);
});

test('files listed in `invalidate` run their hooks again', async () => {
  await build();
  const rebuilt = await build({ invalidate: [path.join(root, 'dep.js')] });
  expect(rebuilt.transformed).toEqual(['dep.js', 'emits.js']);
});

test('a failed build writes no entries', async () => {
  await build();
  const before = entries();
  fs.writeFileSync(path.join(root, 'main.js'), `import { missing } from './missing.js';\n`);
  await expect(build()).rejects.toThrow();
  expect(entries()).toEqual(before);
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
      experimental: { buildCache: { dir: cacheDir } },
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
      experimental: { buildCache: { dir: cacheDir } },
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
      experimental: { buildCache: { dir: cacheDir } },
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
      experimental: { buildCache: { dir: cacheDir } },
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

test('`meta` a later hook overwrote wins over what `resolveId` sets again for a reused module', async () => {
  const run = async () => {
    let seen: unknown;
    let transformed = false;
    const bundle = await rolldown({
      input: './main.js',
      cwd: root,
      plugins: [
        {
          name: 'meta',
          resolveId(source, importer) {
            if (source === './main.js' && !importer)
              return { id: path.join(root, 'main.js'), meta: { meta: { p: 'resolve' } } };
          },
          transform(_code, id) {
            if (!id.endsWith('main.js')) return;
            transformed = true;
            return { meta: { meta: { p: 'transform' } } };
          },
          renderChunk(_code, chunk) {
            const id = chunk.moduleIds.find((m) => m.endsWith('main.js'));
            if (id) seen = this.getModuleInfo(id)?.meta.meta;
            return null;
          },
        },
      ],
      experimental: { buildCache: { dir: cacheDir } },
    });
    await bundle.generate({ format: 'esm' });
    await bundle.close();
    return { seen, transformed };
  };
  fs.writeFileSync(path.join(root, 'main.js'), `console.log('main');\n`);
  expect(await run()).toEqual({ seen: { p: 'transform' }, transformed: true });
  expect(await run()).toEqual({ seen: { p: 'transform' }, transformed: false });
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
      experimental: { buildCache: { dir: cacheDir } },
    });
    const output = await bundle.generate({ format: 'esm' });
    await bundle.close();
    return output.output[0].code;
  };
  expect(await run()).toContain('"one"');
  // Its dependencies are in no entry, so the build's watched files are written out.
  const unrecorded = JSON.parse(fs.readFileSync(path.join(cacheDir, 'build-deps.json'), 'utf8'));
  expect(unrecorded).toContain(path.join(root, 'data.txt'));
  fs.writeFileSync(path.join(root, 'data.txt'), 'two');
  expect(await run()).toContain('"two"');
});

test('no id of a file is kept when one of its ids has `meta` that is not JSON', async () => {
  // As above, with the main id dropped because its `meta` can't be written.
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
            return { code, meta: { sfc: { size: 1n } } };
          },
        },
      ],
      experimental: { buildCache: { dir: cacheDir } },
    });
    const output = await bundle.generate({ format: 'esm' });
    await bundle.close();
    return output.output[0].code;
  };
  expect(await run()).toContain('"one"');
  fs.writeFileSync(path.join(root, 'data.txt'), 'two');
  expect(await run()).toContain('"two"');
});

test('a local package import follows a change to its package.json entry', async () => {
  const pkg = path.join(root, 'packages/pkg');
  fs.mkdirSync(pkg, { recursive: true });
  fs.writeFileSync(path.join(pkg, 'a.js'), `export default 'a';\n`);
  fs.writeFileSync(path.join(pkg, 'b.js'), `export default 'b';\n`);
  const manifest = (main: string) =>
    fs.writeFileSync(path.join(pkg, 'package.json'), JSON.stringify({ name: 'pkg', main }));
  manifest('./a.js');
  fs.mkdirSync(path.join(root, 'node_modules'));
  fs.symlinkSync('../packages/pkg', path.join(root, 'node_modules/pkg'), 'dir');
  fs.writeFileSync(path.join(root, 'main.js'), `import v from 'pkg';\nconsole.log(v);\n`);
  expect((await run()).code).toContain('"a"');
  expect((await run()).transformed).toEqual([]);
  manifest('./b.js');
  expect((await run()).code).toContain('"b"');
});

test('a relative import of a directory follows its package.json entry', async () => {
  const lib = path.join(root, 'lib');
  fs.mkdirSync(lib);
  fs.writeFileSync(path.join(lib, 'a.js'), `export default 'a';\n`);
  fs.writeFileSync(path.join(lib, 'b.js'), `export default 'b';\n`);
  fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ name: 'app' }));
  const manifest = (main: string) =>
    fs.writeFileSync(path.join(lib, 'package.json'), JSON.stringify({ main }));
  manifest('./a.js');
  fs.writeFileSync(path.join(root, 'main.js'), `import v from './lib';\nconsole.log(v);\n`);
  expect((await run()).code).toContain('"a"');
  expect((await run()).transformed).toEqual([]);
  manifest('./b.js');
  expect((await run()).code).toContain('"b"');
});

test('a bare import under tsconfig paths with several targets follows a file added to an earlier one', async () => {
  fs.mkdirSync(path.join(root, 'preferred'));
  fs.mkdirSync(path.join(root, 'fallback'));
  fs.writeFileSync(path.join(root, 'fallback/foo.js'), `export default 'fallback';\n`);
  fs.writeFileSync(
    path.join(root, 'tsconfig.json'),
    JSON.stringify({ compilerOptions: { paths: { '@lib/*': ['./preferred/*', './fallback/*'] } } }),
  );
  fs.writeFileSync(path.join(root, 'main.js'), `import v from '@lib/foo';\nconsole.log(v);\n`);
  const options = { tsconfig: path.join(root, 'tsconfig.json') };
  expect((await run([], options)).code).toContain('"fallback"');
  expect((await run([], options)).transformed).toEqual([]);
  fs.writeFileSync(path.join(root, 'preferred/foo.js'), `export default 'preferred';\n`);
  expect((await run([], options)).code).toContain('"preferred"');
});

test('files a virtual module watches are listed for whole-build reuse', async () => {
  fs.writeFileSync(path.join(root, 'main.js'), `import v from 'virtual:data';\nconsole.log(v);\n`);
  const bundle = await rolldown({
    input: './main.js',
    cwd: root,
    plugins: [
      {
        name: 'virtual',
        resolveId(id) {
          if (id === 'virtual:data') return '\0virtual:data';
        },
        load(id) {
          if (id !== '\0virtual:data') return;
          this.addWatchFile(path.join(root, 'data.txt'));
          return `export default ${JSON.stringify(fs.readFileSync(path.join(root, 'data.txt'), 'utf8'))};`;
        },
      },
    ],
    experimental: { buildCache: { dir: cacheDir } },
  });
  await bundle.generate({ format: 'esm' });
  await bundle.close();
  const listed = JSON.parse(fs.readFileSync(path.join(cacheDir, 'build-deps.json'), 'utf8'));
  expect(listed).toContain(path.join(root, 'data.txt'));
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
      experimental: { buildCache: { dir: cacheDir } },
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

test('a module whose `load` emitted a file is not reused: asset module types', async () => {
  fs.writeFileSync(path.join(root, 'logo.png'), 'png bytes');
  fs.writeFileSync(
    path.join(root, 'main.js'),
    `import logo from './logo.png';\nconsole.log(logo);\n`,
  );
  const options: InputOptions = { moduleTypes: { '.png': 'asset' } };
  const first = await run([], options);
  expect(Object.values(first.files)).toContain('png bytes');
  const second = await run([], options);
  expect(second.files).toEqual(first.files);
  expect(second.transformed).toEqual(['logo.png']);
});

test('a module whose JS `load` emitted a file is not reused', async () => {
  fs.writeFileSync(path.join(root, 'main.js'), `import v from './data.txt';\nconsole.log(v);\n`);
  const plugin: Plugin = {
    name: 'emit-on-load',
    load(id) {
      if (!id.endsWith('data.txt')) return;
      this.emitFile({ type: 'asset', fileName: 'copied.txt', source: 'from load' });
      return `export default ${JSON.stringify(fs.readFileSync(id, 'utf8'))};`;
    },
  };
  await run([plugin]);
  const second = await run([plugin]);
  expect(second.files['copied.txt']).toBe('from load');
  expect(second.transformed).toEqual(['data.txt']);
});

test('an importer whose import emitted a file while resolving is not reused', async () => {
  fs.writeFileSync(
    path.join(root, 'main.js'),
    `import { dep } from 'emit-on-resolve';\nconsole.log(dep);\n`,
  );
  const plugin: Plugin = {
    name: 'emit-on-resolve',
    resolveId(source) {
      if (source !== 'emit-on-resolve') return;
      this.emitFile({ type: 'asset', fileName: 'resolved.txt', source: 'from resolveId' });
      return path.join(root, 'dep.js');
    },
  };
  await run([plugin]);
  const second = await run([plugin]);
  expect(second.files['resolved.txt']).toBe('from resolveId');
  expect(second.transformed).toEqual(['main.js']);
});

test('a file `resolveId` watched for an importer re-runs the importer when it changes', async () => {
  fs.writeFileSync(path.join(root, 'a.js'), `export default 'a';\n`);
  fs.writeFileSync(path.join(root, 'b.js'), `export default 'b';\n`);
  const routing = path.join(root, 'routing.json');
  fs.writeFileSync(routing, JSON.stringify({ target: 'a.js' }));
  fs.writeFileSync(path.join(root, 'main.js'), `import v from 'route';\nconsole.log(v);\n`);
  const plugin: Plugin = {
    name: 'route',
    resolveId(source) {
      if (source !== 'route') return;
      this.addWatchFile(routing);
      return path.join(root, JSON.parse(fs.readFileSync(routing, 'utf8')).target);
    },
  };
  expect((await run([plugin])).code).toContain('"a"');
  expect((await run([plugin])).transformed).toEqual([]);
  fs.writeFileSync(routing, JSON.stringify({ target: 'b.js' }));
  expect((await run([plugin])).code).toContain('"b"');
});

test("an import within the importer's own package follows its package.json `sideEffects`", async () => {
  const manifest = (sideEffects: boolean) =>
    fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ name: 'app', sideEffects }));
  manifest(false);
  fs.writeFileSync(path.join(root, 'side.js'), `console.log('side effect');\n`);
  fs.writeFileSync(path.join(root, 'main.js'), `import './side.js';\nconsole.log('main');\n`);
  expect((await run()).code).not.toContain('side effect');
  expect((await run()).transformed).toEqual([]);
  manifest(true);
  expect((await run()).code).toContain('side effect');
});

test('an import through a symlink follows the symlink to a new target', async () => {
  fs.writeFileSync(path.join(root, 'a.js'), `export default 'a';\n`);
  fs.writeFileSync(path.join(root, 'b.js'), `export default 'b';\n`);
  fs.symlinkSync('a.js', path.join(root, 'alias.js'));
  fs.writeFileSync(path.join(root, 'main.js'), `import v from './alias.js';\nconsole.log(v);\n`);
  expect((await run()).code).toContain('"a"');
  expect((await run()).transformed).toEqual([]);
  fs.rmSync(path.join(root, 'alias.js'));
  fs.symlinkSync('b.js', path.join(root, 'alias.js'));
  expect((await run()).code).toContain('"b"');
});

test('an import through a symlinked directory follows it to a new target', async () => {
  for (const name of ['one', 'two']) {
    fs.mkdirSync(path.join(root, name));
    fs.writeFileSync(path.join(root, name, 'x.js'), `export default '${name}';\n`);
  }
  fs.symlinkSync('one', path.join(root, 'linked'), 'dir');
  fs.writeFileSync(path.join(root, 'main.js'), `import v from './linked/x.js';\nconsole.log(v);\n`);
  expect((await run()).code).toContain('"one"');
  expect((await run()).transformed).toEqual([]);
  fs.rmSync(path.join(root, 'linked'));
  fs.symlinkSync('two', path.join(root, 'linked'), 'dir');
  expect((await run()).code).toContain('"two"');
});

test('a tsconfig `paths` change in an extended config re-runs the importer', async () => {
  fs.writeFileSync(path.join(root, 'a.js'), `export default 'a';\n`);
  fs.writeFileSync(path.join(root, 'b.js'), `export default 'b';\n`);
  fs.writeFileSync(path.join(root, 'tsconfig.json'), JSON.stringify({ extends: './base.json' }));
  const base = (target: string) =>
    fs.writeFileSync(
      path.join(root, 'base.json'),
      JSON.stringify({ compilerOptions: { paths: { '@lib': [target] } } }),
    );
  base('./a.js');
  fs.writeFileSync(path.join(root, 'main.js'), `import v from '@lib';\nconsole.log(v);\n`);
  const options = { tsconfig: path.join(root, 'tsconfig.json') };
  expect((await run([], options)).code).toContain('"a"');
  expect((await run([], options)).transformed).toEqual([]);
  base('./b.js');
  expect((await run([], options)).code).toContain('"b"');
});

test('a module with `meta` JSON would not bring back the same is not reused', async () => {
  const years: number[] = [];
  const plugin: Plugin = {
    name: 'dated',
    transform(code, id) {
      if (id.endsWith('main.js')) return { code, meta: { dated: { at: new Date(0) } } };
    },
    generateBundle() {
      const main = [...this.getModuleIds()].find((id) => id.endsWith('main.js'))!;
      years.push((this.getModuleInfo(main)!.meta.dated.at as Date).getUTCFullYear());
    },
  };
  await run([plugin]);
  const second = await run([plugin]);
  expect(years).toEqual([1970, 1970]);
  expect(second.transformed).toContain('main.js');
});

test('`meta` is kept as the build left it, and `moduleParsed` runs again on a reused module', async () => {
  const counts: number[] = [];
  const plugin: Plugin = {
    name: 'count-parses',
    transform(code, id) {
      if (id.endsWith('main.js')) return { code, meta: { parsed: { count: 0 } } };
    },
    moduleParsed(info) {
      if (info.id.endsWith('main.js')) info.meta.parsed.count++;
    },
    generateBundle() {
      const main = [...this.getModuleIds()].find((id) => id.endsWith('main.js'))!;
      counts.push(this.getModuleInfo(main)!.meta.parsed.count);
    },
  };
  await run([plugin]);
  const second = await run([plugin]);
  expect(second.transformed).not.toContain('main.js');
  expect(counts).toEqual([1, 2]);
});

test("files a virtual module's `transform` watches are listed for whole-build reuse", async () => {
  fs.writeFileSync(path.join(root, 'main.js'), `import v from 'virtual:data';\nconsole.log(v);\n`);
  const data = path.join(root, 'data.txt');
  await run([
    {
      name: 'virtual',
      resolveId(id) {
        if (id === 'virtual:data') return '\0virtual:data';
      },
      load(id) {
        if (id === '\0virtual:data') return 'export default 1;';
      },
      transform(code, id) {
        if (id !== '\0virtual:data') return;
        this.addWatchFile(data);
        return code.replace('1', JSON.stringify(fs.readFileSync(data, 'utf8')));
      },
    },
  ]);
  const listed = JSON.parse(fs.readFileSync(path.join(cacheDir, 'build-deps.json'), 'utf8'));
  expect(listed).toContain(data);
});

test('what a resolver emits or watches for `this.resolve` without an importer belongs to the caller', async () => {
  fs.writeFileSync(path.join(root, 'a.js'), `export default 'a';\n`);
  fs.writeFileSync(path.join(root, 'b.js'), `export default 'b';\n`);
  const routing = path.join(root, 'routing.json');
  fs.writeFileSync(routing, JSON.stringify({ target: 'a.js' }));
  fs.writeFileSync(path.join(root, 'main.js'), `console.log(ROUTE);\n`);
  const plugins: Plugin[] = [
    {
      name: 'route',
      resolveId(source) {
        if (source !== 'route') return;
        this.addWatchFile(routing);
        if (fs.existsSync(path.join(root, 'emit'))) {
          this.emitFile({ type: 'asset', fileName: 'route.txt', source: 'from resolveId' });
        }
        return path.join(root, JSON.parse(fs.readFileSync(routing, 'utf8')).target);
      },
    },
    {
      name: 'inline-route',
      async transform(code, id) {
        if (!id.endsWith('main.js')) return;
        const resolved = await this.resolve('route');
        return code.replace('ROUTE', JSON.stringify(path.basename(resolved!.id)));
      },
    },
  ];
  expect((await run(plugins)).code).toContain('"a.js"');
  expect((await run(plugins)).transformed).toEqual([]);
  fs.writeFileSync(routing, JSON.stringify({ target: 'b.js' }));
  expect((await run(plugins)).code).toContain('"b.js"');
  fs.writeFileSync(path.join(root, 'emit'), '');
  fs.rmSync(cacheDir, { recursive: true });
  await run(plugins);
  const second = await run(plugins);
  expect(second.files['route.txt']).toBe('from resolveId');
  expect(second.transformed).toEqual(['main.js']);
});

test('a module whose `transform` loaded another module is not reused', async () => {
  fs.writeFileSync(path.join(root, 'main.js'), `console.log(DEP);\n`);
  const plugin: Plugin = {
    name: 'inline-dep',
    async transform(code, id) {
      if (!id.endsWith('main.js')) return;
      const dep = await this.load({ id: path.join(root, 'dep.js') });
      return code.replace('DEP', JSON.stringify(dep.code));
    },
  };
  await run([plugin]);
  expect((await run([plugin])).transformed).toContain('main.js');
});

test('a bare import follows a file added where tsconfig `paths` or `baseUrl` points before `node_modules`', async () => {
  fs.mkdirSync(path.join(root, 'node_modules/foo'), { recursive: true });
  fs.writeFileSync(
    path.join(root, 'node_modules/foo/package.json'),
    JSON.stringify({ name: 'foo', main: 'index.js' }),
  );
  fs.writeFileSync(path.join(root, 'node_modules/foo/index.js'), `export default 'PACKAGE';\n`);
  fs.mkdirSync(path.join(root, 'src'));
  fs.writeFileSync(path.join(root, 'main.js'), `import v from 'foo';\nconsole.log(v);\n`);
  for (const compilerOptions of [{ paths: { foo: ['./src/foo.js'] } }, { baseUrl: './src' }]) {
    fs.rmSync(path.join(root, 'src/foo.js'), { force: true });
    fs.rmSync(cacheDir, { recursive: true, force: true });
    fs.writeFileSync(path.join(root, 'tsconfig.json'), JSON.stringify({ compilerOptions }));
    const options = { tsconfig: path.join(root, 'tsconfig.json') };
    expect((await run([], options)).code).toContain('"PACKAGE"');
    expect((await run([], options)).transformed).toEqual([]);
    fs.writeFileSync(path.join(root, 'src/foo.js'), `export default 'LOCAL';\n`);
    expect((await run([], options)).code).toContain('"LOCAL"');
  }
});

test('an import follows a `package.json` added closer to the file it resolves to', async () => {
  fs.writeFileSync(
    path.join(root, 'package.json'),
    JSON.stringify({ name: 'app', sideEffects: false }),
  );
  fs.mkdirSync(path.join(root, 'lib'));
  fs.writeFileSync(path.join(root, 'lib/dep.js'), `console.log('side effect');\n`);
  fs.writeFileSync(path.join(root, 'main.js'), `import './lib/dep.js';\nconsole.log('main');\n`);
  expect((await run()).code).not.toContain('side effect');
  expect((await run()).transformed).toEqual([]);
  fs.writeFileSync(path.join(root, 'lib/package.json'), JSON.stringify({ sideEffects: true }));
  expect((await run()).code).toContain('side effect');
});

test('an import through a symlink chain follows a link further along it', async () => {
  fs.writeFileSync(path.join(root, 'a.js'), `export default 'AAA';\n`);
  fs.writeFileSync(path.join(root, 'b.js'), `export default 'BBB';\n`);
  fs.symlinkSync('a.js', path.join(root, 'middle.js'));
  fs.symlinkSync('middle.js', path.join(root, 'alias.js'));
  fs.writeFileSync(path.join(root, 'main.js'), `import v from './alias.js';\nconsole.log(v);\n`);
  expect((await run()).code).toContain('"AAA"');
  expect((await run()).transformed).toEqual([]);
  fs.rmSync(path.join(root, 'middle.js'));
  fs.symlinkSync('b.js', path.join(root, 'middle.js'));
  expect((await run()).code).toContain('"BBB"');
});

test('`meta` keys a hook removed stay removed on a reused module', async () => {
  const seen: string[][] = [];
  const plugin: Plugin = {
    name: 'remove-meta',
    resolveId(source, importer) {
      if (importer == null && source === './main.js') {
        return { id: path.join(root, 'main.js'), meta: { removed: 'RESOLVE', kept: 'KEEP' } };
      }
    },
    transform(code, id) {
      if (!id.endsWith('main.js')) return;
      delete this.getModuleInfo(id)!.meta.removed;
      return code;
    },
    generateBundle() {
      seen.push(Object.keys(this.getModuleInfo(path.join(root, 'main.js'))!.meta).sort());
    },
  };
  await run([plugin]);
  const second = await run([plugin]);
  expect(second.transformed).not.toContain('main.js');
  expect(seen).toEqual([['kept'], ['kept']]);
});

test('an importer whose import resolves to an external module with `meta` is not reused', async () => {
  fs.writeFileSync(path.join(root, 'main.js'), `import 'ext';\nconsole.log('main');\n`);
  const tags: unknown[] = [];
  const plugin: Plugin = {
    name: 'external-meta',
    resolveId(source) {
      if (source === 'ext') return { id: 'ext', external: true, meta: { tag: 'EXT' } };
    },
    generateBundle() {
      tags.push(this.getModuleInfo('ext')?.meta.tag);
    },
  };
  await run([plugin]);
  const second = await run([plugin]);
  expect(tags).toEqual(['EXT', 'EXT']);
  expect(second.transformed).toEqual(['main.js']);
});

test('`moduleSideEffects` a hook returned is visible on a reused module', async () => {
  const values: unknown[] = [];
  const plugin: Plugin = {
    name: 'side-effects',
    transform(code, id) {
      if (id.endsWith('dep.js')) return { code, moduleSideEffects: false };
    },
    generateBundle() {
      values.push(this.getModuleInfo(path.join(root, 'dep.js'))!.moduleSideEffects);
    },
  };
  await run([plugin]);
  const second = await run([plugin]);
  expect(second.transformed).not.toContain('dep.js');
  expect(values).toEqual([false, false]);
});

test('a module with `meta` that has symbol or accessor properties is not reused', async () => {
  const marks: unknown[] = [];
  const mark = Symbol('mark');
  let reads = 0;
  const plugin: Plugin = {
    name: 'odd-meta',
    transform(code, id) {
      if (id.endsWith('main.js')) {
        const list = Object.assign([1], { [mark]: 'MARK' });
        return { code, meta: { list } };
      }
      if (id.endsWith('dep.js')) {
        // On the live `meta` (`meta` a hook returns is copied, getters included).
        Object.defineProperty(this.getModuleInfo(id)!.meta, 'value', {
          enumerable: true,
          get: () => (reads++ === 0 ? 1 : 1n),
        });
      }
    },
    generateBundle() {
      marks.push(this.getModuleInfo(path.join(root, 'main.js'))!.meta.list[mark]);
    },
  };
  await run([plugin]);
  const second = await run([plugin]);
  expect(marks).toEqual(['MARK', 'MARK']);
  expect(second.transformed).toEqual(expect.arrayContaining(['dep.js', 'main.js']));
});

test('`build-deps.json` lists the tsconfig that governs a module with its marker', async () => {
  fs.writeFileSync(path.join(root, 'tsconfig.json'), JSON.stringify({ compilerOptions: {} }));
  await run([], { tsconfig: true });
  const listed: string[] = JSON.parse(
    fs.readFileSync(path.join(cacheDir, 'build-deps.json'), 'utf8'),
  );
  expect(listed).toContain(`${path.join(root, 'tsconfig.json')}\0\0tsconfig`);
});

test('a tsconfig `paths` target that is a directory wins once it gets an index, or with another extension', async () => {
  fs.mkdirSync(path.join(root, 'preferred/x'), { recursive: true });
  fs.mkdirSync(path.join(root, 'fallback'));
  fs.writeFileSync(path.join(root, 'fallback/x.js'), `export default 'FALLBACK';\n`);
  fs.writeFileSync(path.join(root, 'fallback/y.js'), `export default 'FALLBACK_Y';\n`);
  fs.writeFileSync(
    path.join(root, 'tsconfig.json'),
    JSON.stringify({
      compilerOptions: {
        paths: {
          '@x': ['./preferred/x', './fallback/x'],
          '@y': ['./preferred/y.js', './fallback/y.js'],
        },
      },
    }),
  );
  fs.writeFileSync(
    path.join(root, 'main.js'),
    `import x from '@x';\nimport y from '@y';\nconsole.log(x, y);\n`,
  );
  const options = { tsconfig: path.join(root, 'tsconfig.json') };
  expect((await run([], options)).code).toContain('"FALLBACK"');
  expect((await run([], options)).transformed).toEqual([]);
  fs.writeFileSync(path.join(root, 'preferred/x/index.js'), `export default 'PREFERRED';\n`);
  expect((await run([], options)).code).toContain('"PREFERRED"');
  const clean = async () => {
    fs.rmSync(cacheDir, { recursive: true, force: true });
    return (await run([], options)).code;
  };
  fs.writeFileSync(path.join(root, 'preferred/y.ts'), `export default 'PREFERRED_Y';\n`);
  const warm = (await run([], options)).code;
  expect(warm).toBe(await clean());
});

test('an import the `external` option makes external after `resolveId` gave it `meta`', async () => {
  fs.writeFileSync(path.join(root, 'main.js'), `import 'external-pkg';\nconsole.log('main');\n`);
  const tags: unknown[] = [];
  const plugin: Plugin = {
    name: 'external-meta-late',
    resolveId(source) {
      if (source === 'external-pkg') return { id: 'resolved-pkg', meta: { tag: 'PRESENT' } };
    },
    generateBundle() {
      tags.push(this.getModuleInfo('resolved-pkg')?.meta.tag);
    },
  };
  // Only the resolved id is external: the option applies after `resolveId`.
  const external = (id: string) => id === 'resolved-pkg';
  await run([plugin], { external });
  await run([plugin], { external });
  expect(tags).toEqual(['PRESENT', 'PRESENT']);
});

test('what `this.resolve` from a hook resolved is a dependency of the module', async () => {
  fs.mkdirSync(path.join(root, 'mod'));
  fs.writeFileSync(path.join(root, 'mod/index.js'), `export default 1;\n`);
  fs.writeFileSync(path.join(root, 'main.js'), `console.log(RESOLVED);\n`);
  const plugin: Plugin = {
    name: 'embed-resolved',
    async transform(code, id) {
      if (!id.endsWith('main.js')) return;
      const resolved = await this.resolve('./mod', id);
      return code.replace('RESOLVED', JSON.stringify(path.relative(root, resolved!.id)));
    },
  };
  expect((await run([plugin])).code).toContain('"mod/index.js"');
  expect((await run([plugin])).transformed).toEqual([]);
  fs.writeFileSync(path.join(root, 'mod.js'), `export default 2;\n`);
  expect((await run([plugin])).code).toContain('"mod.js"');
});

test('a module with `meta` holding an array with a hole is not reused', async () => {
  const seen: string[] = [];
  const plugin: Plugin = {
    name: 'holey-meta',
    transform(code, id) {
      if (!id.endsWith('main.js')) return;
      const list: unknown[] = [];
      list.length = 1; // a hole at 0
      Object.assign(list, { extra: 'PRESENT' });
      return { code, meta: { list } };
    },
    generateBundle() {
      const { list } = this.getModuleInfo(path.join(root, 'main.js'))!.meta;
      seen.push(JSON.stringify({ present: 0 in list, extra: list.extra }));
    },
  };
  await run([plugin]);
  const second = await run([plugin]);
  expect(seen[1]).toBe(seen[0]);
  expect(second.transformed).toContain('main.js');
});

test('an import of a file replaced by a directory of the same name resolves again', async () => {
  fs.writeFileSync(path.join(root, 'main.js'), `import v from './lib.js';\nconsole.log(v);\n`);
  fs.writeFileSync(path.join(root, 'lib.js'), `export default 'FILE';\n`);
  expect((await run()).code).toContain('"FILE"');
  expect((await run()).transformed).toEqual([]);
  fs.rmSync(path.join(root, 'lib.js'));
  fs.mkdirSync(path.join(root, 'lib.js'));
  fs.writeFileSync(path.join(root, 'lib.js/index.js'), `export default 'DIRECTORY';\n`);
  expect((await run()).code).toContain('"DIRECTORY"');
});

test('rolldown/experimental reports the buildCache capability', async () => {
  const { experimentalCapabilities } = await import('rolldown/experimental');
  expect(experimentalCapabilities.buildCache).toBe(1);
  expect(Object.isFrozen(experimentalCapabilities)).toBe(true);
});
