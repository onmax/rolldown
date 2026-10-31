import type {
  BindingBundle,
  BindingOutputAsset,
  BindingOutputChunk,
  BindingOutputs,
  JsChangedOutputs,
  JsOutputAsset,
  JsOutputChunk,
} from '../binding.cjs';
import type { MinimalPluginContext } from '../plugin/minimal-plugin-context';
import type { OutputCodeCacheEntry } from '../plugin/plugin-context-data';
import { OutputAssetImpl } from '../types/output-asset-impl';
import type { OutputBundle } from '../types/output-bundle';
import { OutputChunkImpl } from '../types/output-chunk-impl';
import type { OutputAsset, OutputChunk, RolldownOutput, SourceMap } from '../types/rolldown-output';
import { bindingifySourcemap } from '../types/sourcemap';
import { type AssetSource, bindingAssetSource, transformAssetSource } from './asset-source';
import { transformChunkModules } from './transform-rendered-chunk';

export function transformToRollupSourceMap(map: string): SourceMap {
  const parsed: Omit<SourceMap, 'toString' | 'toUrl'> = JSON.parse(map);
  const obj: SourceMap = {
    ...parsed,
    toString() {
      return JSON.stringify(obj);
    },
    toUrl() {
      return `data:application/json;charset=utf-8;base64,${Buffer.from(
        obj.toString(),
        'utf-8',
      ).toString('base64')}`;
    },
  };
  // Preserve Rollup's legacy property for plugins while serializing the standard `ignoreList`.
  Object.defineProperty(obj, 'x_google_ignoreList', {
    get: () => obj.ignoreList,
    set: (value: number[] | undefined) => {
      obj.ignoreList = value;
    },
  });
  return obj;
}

/**
 * The key a mutable output has in the shared bundle. A plugin can rename an output; until Rust
 * returns it under the new name, the bundle still holds it under the old one, and its later
 * changes must be reported under the key the bundle has now.
 */
const bundleKeys = new WeakMap<object, string>();

function transformToRollupOutputChunk(bindingChunk: BindingOutputChunk): OutputChunk {
  return new OutputChunkImpl(bindingChunk);
}

function transformToMutableRollupOutputChunk(
  bindingChunk: BindingOutputChunk,
  changed: ChangedOutputs,
  codeCache?: Map<string, OutputCodeCacheEntry>,
): OutputChunk {
  const fileName = bindingChunk.getFileName();
  const chunk = {
    type: 'chunk',
    get code() {
      if (!codeCache) return bindingChunk.getCode();
      const identity = bindingChunk.getCodeIdentity();
      const hit = codeCache.get(fileName);
      if (hit && hit.identity === identity) return hit.code;
      const code = bindingChunk.getCode();
      codeCache.set(fileName, { identity, holder: bindingChunk, code });
      return code;
    },
    fileName,
    // Fields are read lazily: a `generateBundle` or `writeBundle` call builds this wrapper for
    // every output, and most plugins read only a few fields of a few outputs. The Proxy below
    // caches each value on first read.
    get name() {
      return bindingChunk.getName();
    },
    get modules() {
      return transformChunkModules(bindingChunk.getModules());
    },
    get imports() {
      return bindingChunk.getImports();
    },
    get dynamicImports() {
      return bindingChunk.getDynamicImports();
    },
    get exports() {
      return bindingChunk.getExports();
    },
    get isEntry() {
      return bindingChunk.getIsEntry();
    },
    get facadeModuleId() {
      return bindingChunk.getFacadeModuleId() || null;
    },
    get isDynamicEntry() {
      return bindingChunk.getIsDynamicEntry();
    },
    get moduleIds() {
      return bindingChunk.getModuleIds();
    },
    get map() {
      const map = bindingChunk.getMap();
      return map ? transformToRollupSourceMap(map) : null;
    },
    get sourcemapFileName() {
      return bindingChunk.getSourcemapFileName() || null;
    },
    get preliminaryFileName() {
      return bindingChunk.getPreliminaryFileName();
    },
  } as OutputChunk;
  const cache: Record<string | symbol, any> = {};
  return new Proxy(chunk, {
    get(target, p) {
      if (p in cache) {
        return cache[p];
      }
      const value = target[p as keyof OutputChunk];
      cache[p] = value;
      return value;
    },
    set(_target, p, newValue, receiver): boolean {
      cache[p] = newValue;
      changed.updated.add(bundleKeys.get(receiver) ?? fileName);
      return true;
    },
    has(target, p): boolean {
      if (p in cache) return true;
      return p in target;
    },
  });
}

function transformToRollupOutputAsset(bindingAsset: BindingOutputAsset): OutputAsset {
  return new OutputAssetImpl(bindingAsset);
}

function transformToMutableRollupOutputAsset(
  bindingAsset: BindingOutputAsset,
  changed: ChangedOutputs,
): OutputAsset {
  const fileName = bindingAsset.getFileName();
  const asset = {
    type: 'asset',
    fileName,
    get originalFileName() {
      return bindingAsset.getOriginalFileName() || null;
    },
    get originalFileNames() {
      return bindingAsset.getOriginalFileNames();
    },
    get source(): AssetSource {
      return transformAssetSource(bindingAsset.getSource());
    },
    get name() {
      return bindingAsset.getName() ?? undefined;
    },
    get names() {
      return bindingAsset.getNames();
    },
  } as OutputAsset;
  const cache: Record<string | symbol, any> = {};
  return new Proxy(asset, {
    get(target, p) {
      if (p in cache) {
        return cache[p];
      }
      const value = target[p as keyof OutputAsset];
      cache[p] = value;
      return value;
    },
    set(_target, p, newValue, receiver): boolean {
      cache[p] = newValue;
      changed.updated.add(bundleKeys.get(receiver) ?? fileName);
      return true;
    },
  });
}

export function transformToRollupOutput(output: BindingOutputs): RolldownOutput {
  const { chunks, assets } = output;
  const transformed = {
    output: [
      ...chunks.map((chunk) => transformToRollupOutputChunk(chunk)),
      ...assets.map((asset) => transformToRollupOutputAsset(asset)),
    ],
  } as RolldownOutput;
  if (output.mangleCache !== undefined) {
    transformed.mangleCache = output.mangleCache;
  }
  return transformed;
}

function bundleHandler(
  getContext: () => MinimalPluginContext,
  changed: ChangedOutputs,
): ProxyHandler<OutputBundle> {
  return {
    set(_target, _p, _newValue, _receiver) {
      const originalStackTraceLimit = Error.stackTraceLimit;
      Error.stackTraceLimit = 2;
      const message =
        'This plugin assigns to bundle variable. This is discouraged by Rollup and is not supported by Rolldown. This will be ignored. https://rollupjs.org/plugin-development/#generatebundle:~:text=DANGER,this.emitFile.';
      const stack = new Error(message).stack ?? message;
      Error.stackTraceLimit = originalStackTraceLimit;

      getContext().warn({
        message: stack,
        code: 'UNSUPPORTED_BUNDLE_ASSIGNMENT',
      });
      return true;
    },
    deleteProperty(target, property): boolean {
      if (typeof property === 'string') {
        changed.deleted.add(property);
      }
      delete target[property as keyof typeof target];
      return true;
    },
  };
}

export interface ChangedOutputs {
  updated: Set<string>;
  deleted: Set<string>;
}

// TODO find a way only transfer the changed part to Rust side.
/**
 * State kept across the `generateBundle` and `writeBundle` calls of one build, so consecutive
 * plugins share one bundle object, as in Rollup. Each output's wrapper is kept by the identity of
 * its native data: Rust replaces an output's data whenever it changes, so an unchanged identity
 * means an unchanged output.
 */
export interface OutputBundleCache {
  /** Cleared at the start of every call; wrappers record their mutations here. */
  changed: ChangedOutputs;
  context: MinimalPluginContext | null;
  items: Map<number, OutputChunk | OutputAsset>;
  /** The identities of the last call's chunks and assets, in Rust's order. */
  chunkIdentities: number[];
  assetIdentities: number[];
  /**
   * The last call's view. It holds every output's data, so while the identities above are compared
   * none of them can be freed and reused for other data, or changed in place by a native hook.
   */
  view: BindingBundle | null;
  /** The object behind `bundle`, updated in place so `bundle` stays one object for the build. */
  target: OutputBundle | null;
  bundle: OutputBundle | null;
}

export function createOutputBundleCache(): OutputBundleCache {
  return {
    changed: { updated: new Set(), deleted: new Set() },
    context: null,
    items: new Map(),
    chunkIdentities: [],
    assetIdentities: [],
    view: null,
    target: null,
    bundle: null,
  };
}

/**
 * The bundle for the next `generateBundle` or `writeBundle` call. Outputs whose native data is
 * unchanged keep their wrapper. An output a plugin changed through its wrapper comes back from Rust
 * as new data; it keeps its wrapper too, which already holds the change, along with any property
 * Rust doesn't know about. Outputs added or removed in Rust are added to or removed from the same
 * bundle object.
 */
export function transformToSharedOutputBundle(
  context: MinimalPluginContext,
  output: BindingBundle,
  cache: OutputBundleCache,
  codeCache?: Map<string, OutputCodeCacheEntry>,
): OutputBundle {
  const chunkIdentities = output.chunkIdentities();
  const assetIdentities = output.assetIdentities();
  const previous = cache.view;
  cache.view = output;
  // Nothing changed since the last hook: no wrapper recorded a change, and Rust has the same data in
  // the same order (a change made in Rust, an emitted or deleted file, gives other identities).
  if (
    cache.bundle &&
    previous &&
    cache.changed.updated.size === 0 &&
    cache.changed.deleted.size === 0 &&
    sameNumbers(chunkIdentities, cache.chunkIdentities) &&
    sameNumbers(assetIdentities, cache.assetIdentities)
  ) {
    cache.context = context;
    return cache.bundle;
  }
  cache.chunkIdentities = chunkIdentities;
  cache.assetIdentities = assetIdentities;
  const changedByPlugin = new Map<string, OutputChunk | OutputAsset>();
  for (const fileName of cache.changed.updated) {
    const item = cache.target?.[fileName];
    // Rust returns it under the file name the wrapper holds now (a plugin may have renamed it).
    if (item && !cache.changed.deleted.has(fileName)) changedByPlugin.set(item.fileName, item);
  }
  cache.changed.updated.clear();
  cache.changed.deleted.clear();
  cache.context = context;
  const items = new Map<number, OutputChunk | OutputAsset>();
  const wrap = (
    identity: number,
    fileName: () => string,
    create: () => OutputChunk | OutputAsset,
  ) => {
    let item = cache.items.get(identity);
    if (!item) {
      const name = changedByPlugin.size > 0 ? fileName() : undefined;
      item = (name !== undefined && changedByPlugin.get(name)) || create();
    }
    items.set(identity, item);
  };
  chunkIdentities.forEach((identity, i) => {
    let chunk: BindingOutputChunk | undefined;
    const get = () => (chunk ??= output.getChunk(i)!);
    wrap(
      identity,
      () => get().getFileName(),
      () => transformToMutableRollupOutputChunk(get(), cache.changed, codeCache),
    );
  });
  assetIdentities.forEach((identity, i) => {
    let asset: BindingOutputAsset | undefined;
    const get = () => (asset ??= output.getAsset(i)!);
    wrap(
      identity,
      () => get().getFileName(),
      () => transformToMutableRollupOutputAsset(get(), cache.changed),
    );
  });
  cache.items = items;
  if (!cache.target) {
    cache.target = {};
    cache.bundle = new Proxy(
      cache.target,
      bundleHandler(() => cache.context!, cache.changed),
    );
  }
  // Same object, same key order as Rust's outputs (plugins iterate it, e.g. for manifests).
  const target = cache.target;
  const next = [...items.values()];
  const keys = Object.keys(target);
  if (keys.length !== next.length || next.some((item, i) => keys[i] !== item.fileName)) {
    for (const key of keys) delete target[key];
  }
  for (const item of next) {
    if (target[item.fileName] !== item) target[item.fileName] = item;
    bundleKeys.set(item, item.fileName);
  }
  return cache.bundle!;
}

function sameNumbers(a: number[], b: number[]) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

export function collectChangedBundle(
  changed: ChangedOutputs,
  bundle: OutputBundle,
): JsChangedOutputs {
  const changes: Record<string, JsOutputChunk | JsOutputAsset> = {};
  for (const key in bundle) {
    if (changed.deleted.has(key) || !changed.updated.has(key)) {
      continue;
    }
    const item = bundle[key];
    if (item.type === 'asset') {
      changes[key] = {
        filename: item.fileName,
        originalFileNames: item.originalFileNames,
        source: bindingAssetSource(item.source),
        names: item.names,
      };
    } else {
      // not all properties modifications are reflected to Rust side
      changes[key] = {
        code: item.code,
        filename: item.fileName,
        name: item.name,
        isEntry: item.isEntry,
        exports: item.exports,
        modules: {},
        imports: item.imports,
        dynamicImports: item.dynamicImports,
        facadeModuleId: item.facadeModuleId || undefined,
        isDynamicEntry: item.isDynamicEntry,
        moduleIds: item.moduleIds,
        map: bindingifySourcemap(item.map),
        sourcemapFilename: item.sourcemapFileName || undefined,
        preliminaryFilename: item.preliminaryFileName,
      };
    }
  }
  return {
    changes,
    deleted: changed.deleted,
  };
}
