import type { Program } from '@oxc-project/types';
import path from 'node:path';
import type {
  BindingHookFilter,
  BindingHookLoadOutput,
  BindingHookResolveIdOutput,
  BindingHookTransformOutput,
  BindingPluginOptions,
  BindingTransformHookExtraArgs,
  BindingTransformPluginContext,
} from '../binding.cjs';
import { RolldownMagicString } from '../binding-magic-string';
import { parseAst } from '../parse-ast-index';
import { bindingifySourcemap, type ExistingRawSourceMap } from '../types/sourcemap';
import { aggregateBindingErrorsIntoJsError } from '../utils/error';
import { discardResult, isPromiseLike } from '../utils/misc';
import { transformModuleInfo } from '../utils/transform-module-info';
import {
  copySourcemapObject,
  isEmptySourcemapFiled,
  normalizeTransformHookSourcemap,
} from '../utils/transform-sourcemap';
import {
  bindingifyLoadFilter,
  bindingifyResolveIdFilter,
  bindingifyTransformFilter,
} from './bindingify-hook-filter';
import type { BindingifyPluginArgs } from './bindingify-plugin';
import { bindingifyHook, type PluginHookWithBindingExt } from './bindingify-plugin-hook-meta';
import type {
  LoadResult,
  PluginHooks,
  ResolveIdResult,
  SourceDescription,
  TransformResult,
} from './index';
import { LoadPluginContextImpl } from './load-plugin-context';
import { createPluginContext } from './plugin-context';
import { TransformPluginContextImpl } from './transform-plugin-context';

export function bindingifyBuildStart(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['buildStart']> {
  return bindingifyHook(args.plugin.buildStart, ({ handler }) => ({
    plugin: (ctx, opts) => {
      return discardResult(
        handler.call(createPluginContext(args, ctx), args.pluginContextData.getInputOptions(opts)),
      );
    },
  }));
}
export function bindingifyBuildEnd(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['buildEnd']> {
  return bindingifyHook(args.plugin.buildEnd, ({ handler }) => ({
    plugin: (ctx, err) => {
      return discardResult(
        handler.call(
          createPluginContext(args, ctx),
          err ? aggregateBindingErrorsIntoJsError(err) : undefined,
        ),
      );
    },
  }));
}

export function bindingifyResolveId(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['resolveId'], BindingHookFilter | undefined> {
  const hook = args.plugin.resolveId as unknown as PluginHooks['resolveId'];
  return bindingifyHook(hook, ({ handler, options }) => ({
    plugin: (ctx, specifier, importer, extraOptions) => {
      const contextResolveOptions =
        extraOptions.custom != null
          ? args.pluginContextData.getSavedResolveOptions(extraOptions.custom)
          : undefined;

      const ret = handler.call(createPluginContext(args, ctx), specifier, importer ?? undefined, {
        ...extraOptions,
        custom: contextResolveOptions?.custom,
        attributes: extraOptions.attributes ?? {},
        importerRawId: extraOptions.importerRawId ?? importer ?? undefined,
        importerAttributes: extraOptions.importerAttributes ?? {},
      });
      if (isPromiseLike(ret)) {
        return Promise.resolve(ret).then((ret) =>
          bindingifyResolveIdResult(args, ret, specifier, extraOptions.attributes),
        );
      }
      return bindingifyResolveIdResult(args, ret, specifier, extraOptions.attributes);
    },
    filter: bindingifyResolveIdFilter(options.filter),
  }));
}

function bindingifyResolveIdResult(
  args: BindingifyPluginArgs,
  ret: ResolveIdResult,
  specifier: string,
  attributes: Record<string, string> | undefined,
): BindingHookResolveIdOutput | undefined {
  if (ret == null) {
    return;
  }
  if (ret === false) {
    return {
      id: specifier,
      external: true,
      normalizeExternalId: true,
    };
  }
  if (typeof ret === 'string') {
    return { id: ret, normalizeExternalId: false };
  }

  // Make sure the `moduleSideEffects` is update to date
  let exist = args.pluginContextData.updateModuleOption(
    args.pluginContextData.toModuleId(ret.id, ret.attributes ?? attributes),
    {
      meta: ret.meta || {},
      moduleSideEffects: ret.moduleSideEffects ?? null,
      invalidate: false,
    },
  );

  return {
    id: ret.id,
    external: ret.external,
    normalizeExternalId: false,
    moduleSideEffects: exist.moduleSideEffects ?? undefined,
    packageJsonPath: ret.packageJsonPath,
    attributes: ret.attributes,
  };
}

export function bindingifyResolveDynamicImport(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['resolveDynamicImport']> {
  return bindingifyHook(args.plugin.resolveDynamicImport, ({ handler }) => ({
    plugin: (ctx, specifier, importer, extraOptions) => {
      const attributes = extraOptions.attributes ?? {};
      const ret = handler.call(createPluginContext(args, ctx), specifier, importer ?? undefined, {
        attributes,
        importerRawId: extraOptions.importerRawId ?? importer ?? undefined,
        importerAttributes: extraOptions.importerAttributes ?? {},
      });
      if (isPromiseLike(ret)) {
        return Promise.resolve(ret).then((ret) =>
          bindingifyResolveDynamicImportResult(args, ret, specifier, attributes),
        );
      }
      return bindingifyResolveDynamicImportResult(args, ret, specifier, attributes);
    },
  }));
}

function bindingifyResolveDynamicImportResult(
  args: BindingifyPluginArgs,
  ret: ResolveIdResult,
  specifier: string,
  attributes: Record<string, string>,
): BindingHookResolveIdOutput | undefined {
  if (ret == null) {
    return;
  }
  if (ret === false) {
    return {
      id: specifier,
      external: true,
    };
  }
  if (typeof ret === 'string') {
    return {
      id: ret,
    };
  }

  const result: BindingHookResolveIdOutput = {
    id: ret.id,
    external: ret.external,
    packageJsonPath: ret.packageJsonPath,
    attributes: ret.attributes,
  };

  if (ret.moduleSideEffects !== null) {
    result.moduleSideEffects = ret.moduleSideEffects;
  }

  args.pluginContextData.updateModuleOption(
    args.pluginContextData.toModuleId(ret.id, ret.attributes ?? attributes),
    {
      meta: ret.meta || {},
      moduleSideEffects: ret.moduleSideEffects || null,
      invalidate: false,
    },
  );

  return result;
}

/**
 * The `meta` argument of the `transform` hook. A class rather than an object literal with
 * `Object.defineProperties`, because the hook runs once per module per plugin: defining the lazy
 * getters on every call cost more than the rest of the wrapper.
 */
class TransformHookMeta {
  declare moduleType: string;
  rawId: string;
  attributes: Record<string, string>;
  #code: string;
  #magicString?: RolldownMagicString;
  #ast?: Program;

  constructor(bindingMeta: BindingTransformHookExtraArgs, id: string, code: string) {
    Object.assign(this, bindingMeta);
    this.rawId = bindingMeta.rawId ?? id;
    this.attributes = bindingMeta.attributes ?? {};
    this.#code = code;
  }

  get magicString(): RolldownMagicString {
    return (this.#magicString ??= new RolldownMagicString(this.#code));
  }

  get ast(): Program {
    if (this.#ast) {
      return this.#ast;
    }
    let lang: 'js' | 'jsx' | 'tsx' | 'ts' = 'js';
    switch (this.moduleType) {
      case 'js':
      case 'jsx':
      case 'ts':
      case 'tsx':
        lang = this.moduleType;
        break;
      default:
        break;
    }
    this.#ast = parseAst(this.#code, {
      astType: this.moduleType.includes('ts') ? 'ts' : 'js',
      lang,
    });
    return this.#ast;
  }
}

export function bindingifyTransform(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['transform'], BindingHookFilter | undefined> {
  return bindingifyHook(args.plugin.transform, ({ handler, options }) => ({
    plugin: (ctx, code, id, bindingMeta) => {
      const meta = new TransformHookMeta(bindingMeta, id, code);
      const transformCtx = new TransformPluginContextImpl(
        args.outputOptions,
        ctx.inner(),
        args.plugin,
        args.pluginContextData,
        ctx,
        id,
        code,
        args.onLog,
        args.logLevel,
        args.watchMode,
      );
      const ret = handler.call(transformCtx, code, id, meta);

      if (isPromiseLike(ret)) {
        return Promise.resolve(ret).then((ret) =>
          bindingifyTransformResult(args, ret, ctx, code, id),
        );
      }
      return bindingifyTransformResult(args, ret, ctx, code, id);
    },
    filter: bindingifyTransformFilter(options.filter),
  }));
}

function bindingifyTransformResult(
  args: BindingifyPluginArgs,
  ret: TransformResult,
  ctx: BindingTransformPluginContext,
  code: string,
  id: string,
): BindingHookTransformOutput | undefined {
  if (ret == null) {
    return undefined;
  }

  if (typeof ret === 'string') {
    return { code: ret };
  }

  let moduleOption = args.pluginContextData.updateModuleOption(id, {
    meta: ret.meta ?? {},
    moduleSideEffects: ret.moduleSideEffects ?? null,
    invalidate: false,
  });

  let normalizedCode: string | undefined = undefined;
  let map = ret.map;
  let mapHandledByNativeChannel = false;
  if (typeof ret.code === 'string') {
    normalizedCode = ret.code;
  } else if (ret.code instanceof RolldownMagicString) {
    let magicString = ret.code as RolldownMagicString;
    normalizedCode = magicString.toString();
    // Rolldown generates the map from the MagicString only when the hook returns no `map`,
    // the same as in `renderChunk`. An own `map` or `map: null` is used as it is. A
    // generated map in addition to an own map would apply the edits of this step two times.
    if (ret.map === undefined) {
      // If the option is not enable we should just return soucemapJsonString
      let fallbackSourcemap = ctx.sendMagicString(magicString);
      if (fallbackSourcemap != undefined) {
        map = fallbackSourcemap;
      } else {
        // `experimental.nativeMagicString` is enabled: the sourcemap is
        // generated natively and delivered out-of-band via the magic-string
        // channel. Signal `null` (an explicit "no map on this output object")
        // rather than `undefined`, otherwise the Rust side treats this
        // transform as a missing/broken sourcemap (`Omitted`) and the empty
        // sentinel wipes out the real map produced by the channel.
        mapHandledByNativeChannel = true;
      }
    }
  }

  return {
    code: normalizedCode,
    // Preserve the `map: null` (intentional opt-out) vs `map: undefined`
    map:
      bindingifySourcemap(normalizeTransformHookSourcemap(id, code, map)) ??
      (mapHandledByNativeChannel || ret.map === null ? null : undefined),
    moduleSideEffects: moduleOption.moduleSideEffects ?? undefined,
    moduleType: ret.moduleType,
  };
}

export function bindingifyLoad(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['load'], BindingHookFilter | undefined> {
  return bindingifyHook(args.plugin.load, ({ handler, options }) => ({
    plugin: (ctx, id, bindingExtraOptions) => {
      const extraOptions = bindingExtraOptions ?? { rawId: id, attributes: {} };
      const ret = handler.call(
        new LoadPluginContextImpl(
          args.outputOptions,
          ctx.inner(),
          args.plugin,
          args.pluginContextData,
          ctx,
          id,
          args.onLog,
          args.logLevel,
          args.watchMode,
        ),
        id,
        extraOptions,
      );

      if (isPromiseLike(ret)) {
        return Promise.resolve(ret).then((ret) =>
          bindingifyLoadResult(args, ret, id, extraOptions.rawId),
        );
      }
      return bindingifyLoadResult(args, ret, id, extraOptions.rawId);
    },
    filter: bindingifyLoadFilter(options.filter),
  }));
}

function bindingifyLoadResult(
  args: BindingifyPluginArgs,
  ret: LoadResult,
  id: string,
  rawId: string,
): BindingHookLoadOutput | undefined {
  if (ret == null) {
    return;
  }

  if (typeof ret === 'string') {
    return { code: ret };
  }

  let moduleOption = args.pluginContextData.updateModuleOption(id, {
    meta: ret.meta || {},
    moduleSideEffects: ret.moduleSideEffects ?? null,
    invalidate: false,
  });

  let map = preProcessSourceMap(ret, rawId);

  return {
    code: ret.code,
    map: bindingifySourcemap(map),
    moduleType: ret.moduleType,
    moduleSideEffects: moduleOption.moduleSideEffects ?? undefined,
  };
}

function preProcessSourceMap(
  ret: SourceDescription,
  id: string,
): ExistingRawSourceMap | null | undefined {
  if (!ret.map) {
    return;
  }
  // Work on a copy, so the write to `sources` below does not change the plugin's object.
  let map =
    typeof ret.map === 'object'
      ? copySourcemapObject(ret.map)
      : (JSON.parse(ret.map) as ExistingRawSourceMap);
  if (!isEmptySourcemapFiled(map.sources)) {
    // normalize original sourcemap sources
    // Port form https://github.com/rollup/rollup/blob/master/src/utils/collapseSourcemaps.ts#L180-L188.
    const directory = path.dirname(id) || '.';
    const sourceRoot = map.sourceRoot || '.';
    map.sources = map.sources!.map((source) => path.resolve(directory, sourceRoot, source!));
  }
  return map;
}

export function bindingifyModuleParsed(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['moduleParsed']> {
  return bindingifyHook(args.plugin.moduleParsed, ({ handler }) => ({
    plugin: (ctx, moduleInfo) => {
      return discardResult(
        handler.call(
          createPluginContext(args, ctx),
          transformModuleInfo(moduleInfo, args.pluginContextData.getModuleOption(moduleInfo.id)),
        ),
      );
    },
  }));
}
