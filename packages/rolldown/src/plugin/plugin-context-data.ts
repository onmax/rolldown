import type {
  ModuleOptions,
  NormalizedInputOptions,
  NormalizedOutputOptions,
  OutputOptions,
  RolldownPlugin,
} from '..';
import type { BindingOutputChunk, BindingPluginContext } from '../binding.cjs';
import { type BindingNormalizedOptions } from '../binding.cjs';
import type { LogHandler } from '../log/log-handler';
import { NormalizedInputOptionsImpl } from '../options/normalized-input-options';
import { NormalizedOutputOptionsImpl } from '../options/normalized-output-options';
import type { ModuleInfo } from '../types/module-info';
import type { KeptModuleOptions } from '../utils/build-cache-meta';
import { transformModuleInfo } from '../utils/transform-module-info';
import {
  createOutputBundleCache,
  type OutputBundleCache,
} from '../utils/transform-to-rollup-output';
import type { RenderedChunkMeta } from '.';
import type { PluginContextResolveOptions } from './plugin-context';

/**
 * This entry keeps the `chunk.code` string of one output chunk. All plugins of a build use this
 * one string. Each call of a `generateBundle` or `writeBundle` hook gets new `BindingOutputChunk`
 * objects. Without this cache, each plugin that reads `chunk.code` copies the code from Rust again.
 */
export interface OutputCodeCacheEntry {
  /**
   * This is the `BindingOutputChunk.getCodeIdentity()` value of the chunk data that `code` came
   * from. The identity is the address of that chunk data in Rust memory. The `chunk.code` getter in
   * `transformToMutableRollupOutputChunk` compares only the identity, never the code. When the
   * identity is the same, the getter returns `code`. Otherwise, the getter copies the code again
   * and replaces this entry.
   */
  identity: number;
  /**
   * This reference to the chunk data keeps `identity` correct while this entry exists:
   * - Rust does not free the chunk data, so new chunk data cannot get the same address.
   * - `update_output_chunk` (`crates/rolldown_binding/src/types/binding_output_chunk.rs:163`)
   *   cannot write a plugin change into this chunk data, because `Arc::get_mut` fails while this
   *   second reference exists. The function writes the change into new chunk data at a new
   *   address. So the getter sees a new identity and copies the new code. It does not return the
   *   old `code`.
   */
  holder: BindingOutputChunk;
  code: string;
}

export class PluginContextData {
  moduleOptionMap: Map<string, ModuleOptions> = new Map();
  /**
   * The cache is `null` in parallel plugin workers. In a worker, each plugin has its own
   * `PluginContextData`, so the cache never finds a match. See `parallel-plugin-worker.ts`.
   */
  outputCodeCache: Map<string, OutputCodeCacheEntry> | null;
  /** One bundle object shared by the output hooks of a build; see `OutputBundleCache`. */
  outputBundleCache: OutputBundleCache = createOutputBundleCache();
  resolveOptionsMap: Map<number, PluginContextResolveOptions> = new Map();
  loadModulePromiseMap: Map<string, Promise<void>> = new Map();
  renderedChunkMeta: RenderedChunkMeta | null = null;
  normalizedInputOptions: NormalizedInputOptions | null = null;
  normalizedOutputOptions: NormalizedOutputOptions | null = null;
  /**
   * `experimental.buildCache` is on: a reused module's `meta` comes from its entry.
   * `bindingContext` is any plugin context of the current build (they share one plugin driver),
   * for asking Rust; `keptMetaChecked` holds the ids already asked.
   */
  buildCache = false;
  bindingContext: BindingPluginContext | null = null;
  private keptMetaChecked = new Set<string>();

  constructor(
    private onLog: LogHandler,
    private outputOptions: OutputOptions,
    private normalizedInputPlugins: RolldownPlugin[],
    private normalizedOutputPlugins: RolldownPlugin[],
    cacheOutputCode = true,
  ) {
    this.outputCodeCache = cacheOutputCode ? new Map() : null;
  }

  updateModuleOption(id: string, option: ModuleOptions): ModuleOptions {
    // A reused module's kept `meta` goes under what is written once its task decided.
    const existing = this.buildCache ? this.getModuleOption(id) : this.moduleOptionMap.get(id);
    if (existing) {
      if (option.moduleSideEffects != null) {
        existing.moduleSideEffects = option.moduleSideEffects;
      }
      if (option.meta != null) {
        Object.assign(existing.meta, option.meta);
      }
      if (option.invalidate != null) {
        existing.invalidate = option.invalidate;
      }
    } else {
      this.moduleOptionMap.set(id, option);
      return option;
    }
    return existing;
  }

  getModuleOption(id: string): ModuleOptions {
    let option = this.moduleOptionMap.get(id);
    if (!option) {
      option = {
        moduleSideEffects: null,
        meta: {},
      };
      this.moduleOptionMap.set(id, option);
    }
    if (this.buildCache) this.restoreKeptMeta(id, option);
    return option;
  }

  /**
   * Restores the options kept in a reused module's entry (see `build-cache-meta.ts`). They are
   * what the module ended the last build with, so they replace what `resolveId` set before the
   * module's task decided (the same hooks ran then too), keys it removed staying removed, and are
   * overwritten by everything written after. `meta` keeps its identity.
   */
  private restoreKeptMeta(id: string, option: ModuleOptions): void {
    if (this.keptMetaChecked.has(id) || !this.bindingContext) return;
    const kept = this.bindingContext.getReusedModuleMeta(id);
    // The module's task hasn't decided yet; ask again on the next read.
    if (kept == null) return;
    this.keptMetaChecked.add(id);
    // Not reused: its hooks ran and set the options.
    if (!kept) return;
    const { meta = {}, moduleSideEffects = null } = JSON.parse(kept) as KeptModuleOptions;
    option.meta ??= {};
    for (const key of Reflect.ownKeys(option.meta)) {
      delete (option.meta as Record<PropertyKey, unknown>)[key];
    }
    Object.assign(option.meta, meta);
    option.moduleSideEffects = moduleSideEffects;
  }

  getModuleInfo(id: string, context: BindingPluginContext): ModuleInfo | null {
    const bindingInfo = context.getModuleInfo(id);
    if (bindingInfo) {
      const info = transformModuleInfo(bindingInfo, this.getModuleOption(id));
      return this.proxyModuleInfo(id, info);
    }
    return null;
  }

  proxyModuleInfo(id: string, info: ModuleInfo): ModuleInfo {
    let moduleSideEffects = info.moduleSideEffects;
    Object.defineProperty(info, 'moduleSideEffects', {
      get() {
        return moduleSideEffects;
      },
      set: (v: any) => {
        this.updateModuleOption(id, {
          moduleSideEffects: v,
          meta: info.meta,
          invalidate: true,
        });
        moduleSideEffects = v;
      },
    });
    return info;
  }

  getModuleIds(context: BindingPluginContext): ArrayIterator<string> {
    const moduleIds = context.getModuleIds();
    return moduleIds.values();
  }

  saveResolveOptions(options: PluginContextResolveOptions): number {
    const index = this.resolveOptionsMap.size;
    this.resolveOptionsMap.set(index, options);
    return index;
  }

  getSavedResolveOptions(receipt: number): PluginContextResolveOptions | undefined {
    return this.resolveOptionsMap.get(receipt);
  }

  removeSavedResolveOptions(receipt: number): void {
    this.resolveOptionsMap.delete(receipt);
  }

  setRenderChunkMeta(meta: RenderedChunkMeta): void {
    this.renderedChunkMeta = meta;
  }

  getRenderChunkMeta(): RenderedChunkMeta | null {
    return this.renderedChunkMeta;
  }

  getInputOptions(opts: BindingNormalizedOptions): NormalizedInputOptions {
    this.normalizedInputOptions ??= new NormalizedInputOptionsImpl(
      opts,
      this.onLog,
      this.normalizedInputPlugins,
    );
    return this.normalizedInputOptions;
  }

  getOutputOptions(opts: BindingNormalizedOptions): NormalizedOutputOptions {
    this.normalizedOutputOptions ??= new NormalizedOutputOptionsImpl(
      opts,
      this.outputOptions,
      this.normalizedOutputPlugins,
    );
    return this.normalizedOutputOptions;
  }

  clear(): void {
    this.renderedChunkMeta = null;
    this.loadModulePromiseMap.clear();
    // Watch mode reuses this object across rebuilds; entries hold the previous build's chunk data.
    this.outputCodeCache?.clear();
    this.outputBundleCache = createOutputBundleCache();
    this.bindingContext = null;
    this.keptMetaChecked.clear();
  }
}
