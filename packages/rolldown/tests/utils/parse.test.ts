import { parse, parseSync, Visitor } from 'rolldown/utils';
import { expect, test } from 'vitest';

test('parse non json value', async () => {
  const result = await parse('foo.js', '1n');
  expect(result.program.body[0]).toMatchInlineSnapshot(`
    {
      "end": 2,
      "expression": {
        "bigint": "1",
        "end": 2,
        "raw": "1n",
        "start": 0,
        "type": "Literal",
        "value": 1n,
      },
      "start": 0,
      "type": "ExpressionStatement",
    }
  `);
});

test('parseSync non json value', () => {
  const result = parseSync('foo.js', '1n');
  expect(result.program.body[0]).toMatchInlineSnapshot(`
    {
      "end": 2,
      "expression": {
        "bigint": "1",
        "end": 2,
        "raw": "1n",
        "start": 0,
        "type": "Literal",
        "value": 1n,
      },
      "start": 0,
      "type": "ExpressionStatement",
    }
  `);
});

test('Visitor is supported', () => {
  const result = parseSync('foo.js', 'function greet() { return 1; }');
  const order: string[] = [];
  const visitor = new Visitor({
    FunctionDeclaration() {
      order.push('enter');
    },
    'FunctionDeclaration:exit'() {
      order.push('exit');
    },
  });
  visitor.visit(result.program);
  expect(order).toEqual(['enter', 'exit']);
});

test('parseSync matches the JSON transfer of oxc-parser', async () => {
  // `rolldown/utils` reads the AST through raw transfer where it can, `oxc-parser`'s
  // default `parseSync` through JSON. Both must give the same result.
  const { parseSync: parseSyncJson } = await import('oxc-parser');
  const cases: [string, string, object?][] = [
    ['a.js', '#!/usr/bin/env node\nconst a = 1n + 2n; /* c */ export default /x\\d+/giu'],
    ['a.js', 'import x, { y as z } from "m"; export * as ns from "n"; export { z }'],
    ['a.ts', 'enum E { A = 1 } export type T<U> = U extends string ? `x${U}` : never; let v = <T>w!;'],
    ['a.tsx', 'const el = <div className="a">{list.map((i) => <Item key={i} />)}</div>'],
    ['a.js', 'const s = "héllo ✓ 𝒳"; const t = `${s}🙂`; // ünïcode'],
    ['a.js', 'let = ;'],
    ['a.js', 'a = ((b))', { preserveParens: true }],
    ['a.js', 'function f() { return 1 }', { range: true }],
    ['a.vue', 'const x: number = 1', { lang: 'ts', astType: 'ts' }],
  ];
  for (const [filename, code, options] of cases) {
    const raw = parseSync(filename, code, options);
    const json = parseSyncJson(filename, code, options);
    for (const field of ['program', 'module', 'comments', 'errors'] as const) {
      expect(raw[field], `${field} of ${code}`).toEqual(json[field]);
    }
  }
});
