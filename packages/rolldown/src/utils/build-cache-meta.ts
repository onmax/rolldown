import type { ModuleOptions, Plugin } from '../plugin';
import type { PluginContextData } from '../plugin/plugin-context-data';

// `experimental.buildCache` keeps each module's JS options (`meta` and `moduleSideEffects`) in the
// module's entry in Rust, so they are restored exactly when the entry is reused, and a module
// whose hooks run again starts from none. `PluginContextData.getModuleOption` restores them the
// first time a module's options are read after its task decided; this plugin hands every
// module's options back to Rust when the scan ends, before the entries are written.
//
// What is kept is what the build left, as Rollup's `cache` keeps `meta`: hooks that run on
// every build (`moduleParsed`, `buildEnd`) see a reused module's options with their own changes
// of the build before, so they should set values rather than add to them.

/** Last in the plugin list, so it runs after every other `buildEnd`. */
export function buildCacheMetaPlugin(data: PluginContextData): Plugin {
  return {
    name: 'rolldown:build-cache-meta',
    buildEnd: {
      order: 'post',
      handler(error) {
        if (error || !data.bindingContext) return;
        const ids: string[] = [];
        const metas: string[] = [];
        // External modules' options too: an importer resolved to one with options isn't kept.
        for (const id of data.moduleOptionMap.keys()) {
          ids.push(id);
          metas.push(encodeModuleOptions(data.getModuleOption(id)));
        }
        data.bindingContext.setModuleMetaForCache(ids, metas);
      },
    },
  };
}

/** The options a reused module gets back, as `encodeModuleOptions` wrote them. */
export interface KeptModuleOptions {
  meta?: Record<string, unknown>;
  moduleSideEffects?: ModuleOptions['moduleSideEffects'];
}

/**
 * `option` as `KeptModuleOptions` JSON, `''` for none, or `'!'` when it wouldn't come back the
 * same from JSON (a reused entry would bring back something else, so the module isn't kept).
 */
function encodeModuleOptions({ meta, moduleSideEffects }: ModuleOptions): string {
  const kept: KeptModuleOptions = {};
  if (meta && Reflect.ownKeys(meta).length > 0) kept.meta = meta;
  if (moduleSideEffects != null) kept.moduleSideEffects = moduleSideEffects;
  if (kept.meta == null && kept.moduleSideEffects == null) return '';
  if (!isPlainJson(kept, new Set())) return '!';
  try {
    return JSON.stringify(kept);
  } catch {
    return '!';
  }
}

/**
 * Whether `JSON.parse(JSON.stringify(value))` gives back an equal value: no `undefined`,
 * functions, symbols, bigints, non-finite numbers or `-0`, array holes, objects other than plain
 * ones (`Date`, `Map`, class instances, null prototypes), non-enumerable, symbol or accessor
 * properties, and no object reached twice (identity doesn't survive).
 */
function isPlainJson(value: unknown, seen: Set<object>): boolean {
  switch (typeof value) {
    case 'string':
    case 'boolean':
      return true;
    case 'number':
      return Number.isFinite(value) && !Object.is(value, -0);
    case 'object':
      break;
    default:
      return false;
  }
  if (value === null) return true;
  if (seen.has(value)) return false;
  seen.add(value);
  const isArray = Array.isArray(value);
  if (!isArray && Object.getPrototypeOf(value) !== Object.prototype) return false;
  const descriptors = Object.getOwnPropertyDescriptors(value);
  const keys = Reflect.ownKeys(descriptors);
  if (isArray) {
    // Every index and `length`, nothing else: no holes, no extra or symbol properties.
    const length = (value as unknown[]).length;
    if (keys.length !== length + 1) return false;
    for (let i = 0; i < length; i++) if (!(String(i) in descriptors)) return false;
  }
  for (const key of keys) {
    if (typeof key === 'symbol') return false;
    const descriptor = descriptors[key];
    if (isArray && key === 'length') continue;
    if (!descriptor.enumerable || !('value' in descriptor)) return false;
    if (!isPlainJson(descriptor.value, seen)) return false;
  }
  return true;
}
