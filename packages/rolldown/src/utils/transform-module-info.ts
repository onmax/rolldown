import type { ModuleOptions } from '..';
import type { BindingModuleInfo } from '../binding.cjs';
import type { ModuleInfo } from '../types/module-info';
import { unsupported } from './misc';

// Fields are read from Rust on first use (most callers read only `meta`); the binding object is a
// snapshot taken when the info was asked for, so a later read gives what an eager copy would have.
export function transformModuleInfo(info: BindingModuleInfo, option: ModuleOptions): ModuleInfo {
  let id: string | undefined;
  let importers: string[] | undefined;
  let dynamicImporters: string[] | undefined;
  let importedIds: string[] | undefined;
  let dynamicallyImportedIds: string[] | undefined;
  let exports: string[] | undefined;
  let isEntry: boolean | undefined;
  let inputFormat: ModuleInfo['inputFormat'] | undefined;
  return {
    get ast() {
      return unsupported('ModuleInfo#ast');
    },
    get code() {
      return info.code;
    },
    get id() {
      return (id ??= info.id);
    },
    get importers() {
      return (importers ??= info.importers);
    },
    get dynamicImporters() {
      return (dynamicImporters ??= info.dynamicImporters);
    },
    get importedIds() {
      return (importedIds ??= info.importedIds);
    },
    get dynamicallyImportedIds() {
      return (dynamicallyImportedIds ??= info.dynamicallyImportedIds);
    },
    get exports() {
      return (exports ??= info.exports);
    },
    get isEntry() {
      return (isEntry ??= info.isEntry);
    },
    get inputFormat() {
      return (inputFormat ??= info.inputFormat as ModuleInfo['inputFormat']);
    },
    ...option,
  };
}
