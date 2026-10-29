import type {
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
    set(_target, p, newValue): boolean {
      cache[p] = newValue;
      changed.updated.add(fileName);
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
    set(_target, p, newValue): boolean {
      cache[p] = newValue;
      changed.updated.add(fileName);
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
  bundle: OutputBundle | null;
}

export function createOutputBundleCache(): OutputBundleCache {
  return {
    changed: { updated: new Set(), deleted: new Set() },
    context: null,
    items: new Map(),
    bundle: null,
  };
}

export function transformToSharedOutputBundle(
  context: MinimalPluginContext,
  output: BindingOutputs,
  cache: OutputBundleCache,
  codeCache?: Map<string, OutputCodeCacheEntry>,
): OutputBundle {
  cache.changed.updated.clear();
  cache.changed.deleted.clear();
  cache.context = context;
  const items = new Map<number, OutputChunk | OutputAsset>();
  let reusedAll = cache.bundle !== null;
  for (const chunk of output.chunks) {
    const identity = chunk.getCodeIdentity();
    let item = cache.items.get(identity);
    if (!item) {
      item = transformToMutableRollupOutputChunk(chunk, cache.changed, codeCache);
      reusedAll = false;
    }
    items.set(identity, item);
  }
  for (const asset of output.assets) {
    const identity = asset.getIdentity();
    let item = cache.items.get(identity);
    if (!item) {
      item = transformToMutableRollupOutputAsset(asset, cache.changed);
      reusedAll = false;
    }
    items.set(identity, item);
  }
  if (reusedAll && items.size === cache.items.size) return cache.bundle!;
  cache.items = items;
  const bundle = Object.fromEntries([...items.values()].map((item) => [item.fileName, item]));
  cache.bundle = new Proxy(
    bundle,
    bundleHandler(() => cache.context!, cache.changed),
  );
  return cache.bundle;
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
