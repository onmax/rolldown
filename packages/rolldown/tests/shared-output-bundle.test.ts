import { rolldown, type OutputBundle, type Plugin } from 'rolldown';
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

test('generateBundle hooks of one build see one bundle, with earlier changes applied', async () => {
  const seen: OutputBundle[] = [];
  const output = await generate([
    {
      name: 'first',
      generateBundle(_, bundle) {
        seen.push(bundle);
        const main = Object.values(bundle).find((o) => o.type === 'chunk' && o.isEntry)!;
        if (main.type === 'chunk') main.code = `/* first */\n${main.code}`;
        this.emitFile({ type: 'asset', fileName: 'emitted.txt', source: 'from first' });
      },
    },
    {
      name: 'second',
      generateBundle(_, bundle) {
        seen.push(bundle);
        const main = Object.values(bundle).find((o) => o.type === 'chunk' && o.isEntry)!;
        expect(main.type === 'chunk' && main.code.startsWith('/* first */')).toBe(true);
        expect(bundle['emitted.txt']?.type).toBe('asset');
        const lazy = Object.keys(bundle).find((f) => f.startsWith('lazy'))!;
        delete bundle[lazy];
      },
    },
    {
      name: 'third',
      generateBundle(_, bundle) {
        seen.push(bundle);
        expect(Object.keys(bundle).some((f) => f.startsWith('lazy'))).toBe(false);
      },
    },
  ]);
  expect(seen).toHaveLength(3);
  const main = output.find((o) => o.type === 'chunk' && o.isEntry);
  expect(main?.type === 'chunk' && main.code.startsWith('/* first */')).toBe(true);
  expect(output.some((o) => o.fileName.startsWith('lazy'))).toBe(false);
  expect(output.some((o) => o.fileName === 'emitted.txt')).toBe(true);
});

test('outputs nobody changed keep their objects across generateBundle hooks', async () => {
  const objects: unknown[][] = [];
  const keep = (bundle: OutputBundle) =>
    objects.push(
      Object.keys(bundle)
        .sort()
        .map((k) => bundle[k]),
    );
  await generate([
    { name: 'a', generateBundle: (_, bundle) => void keep(bundle) },
    { name: 'b', generateBundle: (_, bundle) => void keep(bundle) },
  ]);
  expect(objects[0]).toHaveLength(objects[1].length);
  objects[0].forEach((item, i) => expect(item).toBe(objects[1][i]));
});
