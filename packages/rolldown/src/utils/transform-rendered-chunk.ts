import type { BindingRenderedChunk } from '../binding.cjs';
import type { RenderedChunk } from '../types/rolldown-output';
import { transformToRenderedModule } from './transform-rendered-module';

// Each field is read from Rust on first use and kept: a chunk doesn't change while hooks see it.
export function transformRenderedChunk(chunk: BindingRenderedChunk): RenderedChunk {
  let name: RenderedChunk['name'] | undefined;
  let isEntry: RenderedChunk['isEntry'] | undefined;
  let isDynamicEntry: RenderedChunk['isDynamicEntry'] | undefined;
  let facadeModuleId: RenderedChunk['facadeModuleId'] | undefined;
  let moduleIds: RenderedChunk['moduleIds'] | undefined;
  let exports: RenderedChunk['exports'] | undefined;
  let fileName: RenderedChunk['fileName'] | undefined;
  let imports: RenderedChunk['imports'] | undefined;
  let dynamicImports: RenderedChunk['dynamicImports'] | undefined;
  let modules: RenderedChunk['modules'] | undefined;
  return {
    type: 'chunk',
    get name() {
      return (name ??= chunk.name);
    },
    get isEntry() {
      return (isEntry ??= chunk.isEntry);
    },
    get isDynamicEntry() {
      return (isDynamicEntry ??= chunk.isDynamicEntry);
    },
    get facadeModuleId() {
      if (facadeModuleId === undefined) facadeModuleId = chunk.facadeModuleId;
      return facadeModuleId;
    },
    get moduleIds() {
      return (moduleIds ??= chunk.moduleIds);
    },
    get exports() {
      return (exports ??= chunk.exports);
    },
    get fileName() {
      return (fileName ??= chunk.fileName);
    },
    get imports() {
      return (imports ??= chunk.imports);
    },
    get dynamicImports() {
      return (dynamicImports ??= chunk.dynamicImports);
    },
    get modules() {
      return (modules ??= transformChunkModules(chunk.modules));
    },
  };
}

export function transformChunkModules(
  modules: BindingRenderedChunk['modules'],
): RenderedChunk['modules'] {
  const result: RenderedChunk['modules'] = {};
  for (let i = 0; i < modules.values.length; i++) {
    let key = modules.keys[i];
    const mod = modules.values[i];
    result[key] = transformToRenderedModule(mod);
  }
  return result;
}
