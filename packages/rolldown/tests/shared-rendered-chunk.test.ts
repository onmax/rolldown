import { rolldown, type Plugin, type RenderedChunk } from 'rolldown';
import { expect, test } from 'vitest';

const input = {
  name: 'input',
  resolveId(id) {
    if (id === 'main' || id === 'lazy') return id;
  },
  load(id) {
    if (id === 'main') return `import('lazy'); console.log('main');`;
    if (id === 'lazy') return `console.log('lazy');`;
  },
} satisfies Plugin;

async function generate(plugins: Plugin[]) {
  const bundle = await rolldown({ input: 'main', plugins: [input, ...plugins] });
  const output = await bundle.generate({ format: 'esm' });
  await bundle.close();
  return output.output;
}

test('renderChunk and augmentChunkHash hooks get one object per chunk, the one in meta.chunks', async () => {
  const seen = new Map<string, RenderedChunk[]>();
  const note = (chunk: RenderedChunk) =>
    seen.set(chunk.name, [...(seen.get(chunk.name) ?? []), chunk]);
  await generate([
    {
      name: 'first',
      renderChunk(_code, chunk, _options, meta) {
        note(chunk);
        expect(meta.chunks[chunk.fileName]).toBe(chunk);
        (chunk as RenderedChunk & { mark?: string }).mark = `first saw ${chunk.name}`;
      },
    },
    {
      name: 'second',
      renderChunk(_code, chunk) {
        note(chunk);
        expect((chunk as RenderedChunk & { mark?: string }).mark).toBe(`first saw ${chunk.name}`);
      },
      augmentChunkHash(chunk) {
        note(chunk);
        expect((chunk as RenderedChunk & { mark?: string }).mark).toBe(`first saw ${chunk.name}`);
      },
    },
  ]);
  expect([...seen.keys()].sort()).toEqual(['lazy', 'main']);
  for (const objects of seen.values()) {
    expect(objects).toHaveLength(3);
    expect(new Set(objects).size).toBe(1);
  }
});

test('a rendered chunk reads the same values on every access', async () => {
  await generate([
    {
      name: 'reader',
      renderChunk(_code, chunk) {
        expect(chunk.exports).toBe(chunk.exports);
        expect(chunk.moduleIds).toBe(chunk.moduleIds);
        expect(chunk.modules).toBe(chunk.modules);
        expect(chunk.imports).toBe(chunk.imports);
        expect(chunk.facadeModuleId).toBe(chunk.isEntry ? 'main' : 'lazy');
      },
    },
  ]);
});

test('augmentChunkHash hooks share one object per chunk without any renderChunk hook', async () => {
  const seen: RenderedChunk[] = [];
  const bundle = await rolldown({
    input: 'main',
    plugins: [
      input,
      {
        name: 'first',
        augmentChunkHash(chunk) {
          seen.push(chunk);
          (chunk as RenderedChunk & { mark?: string }).mark = chunk.name;
        },
      },
      {
        name: 'second',
        augmentChunkHash(chunk) {
          seen.push(chunk);
          expect((chunk as RenderedChunk & { mark?: string }).mark).toBe(chunk.name);
        },
      },
    ],
  });
  await bundle.generate({
    format: 'esm',
    entryFileNames: '[name]-[hash].js',
    chunkFileNames: '[name]-[hash].js',
  });
  await bundle.close();
  expect(seen).toHaveLength(4);
  expect(new Set(seen).size).toBe(2);
});

test("each output gets its own chunk objects, with that output's data", async () => {
  const seen: Array<{ chunk: RenderedChunk; format: string }> = [];
  const bundle = await rolldown({
    input: 'main',
    plugins: [
      input,
      {
        name: 'reader',
        renderChunk(_code, chunk, options) {
          if (chunk.isEntry) seen.push({ chunk, format: options.format });
        },
      },
    ],
  });
  await bundle.generate({ format: 'esm' });
  await bundle.generate({ format: 'esm', entryFileNames: 'other-[name].js' });
  await bundle.generate({ format: 'esm' });
  await bundle.close();
  expect(seen).toHaveLength(3);
  expect(new Set(seen.map((s) => s.chunk)).size).toBe(3);
  expect(seen.map((s) => s.chunk.fileName)).toEqual(['main.js', 'other-main.js', 'main.js']);
});
