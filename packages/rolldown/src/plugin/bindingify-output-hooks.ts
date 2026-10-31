import type {
  BindingBundle,
  BindingHookFilter,
  BindingPluginContext,
  BindingPluginOptions,
  BindingRenderedChunk,
  BindingResult,
} from '../binding.cjs';
import { RolldownMagicString } from '../binding-magic-string';
import { bindingifySourcemap } from '../types/sourcemap';
import { aggregateBindingErrorsIntoJsError, unwrapBindingResult } from '../utils/error';
import { transformRenderedChunk } from '../utils/transform-rendered-chunk';
import {
  collectChangedBundle,
  transformToSharedOutputBundle,
} from '../utils/transform-to-rollup-output';
import { bindingifyRenderChunkFilter } from './bindingify-hook-filter';
import type { BindingifyPluginArgs } from './bindingify-plugin';
import { bindingifyHook, type PluginHookWithBindingExt } from './bindingify-plugin-hook-meta';
import type { RenderedChunkMeta } from './index';
import type { RenderedChunk } from '../types/rolldown-output';
import { createPluginContext } from './plugin-context';

export function bindingifyRenderStart(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['renderStart']> {
  return bindingifyHook(args.plugin.renderStart, ({ handler }) => ({
    plugin: async (ctx, opts) => {
      await handler.call(
        createPluginContext(args, ctx),
        args.pluginContextData.getOutputOptions(opts),
        args.pluginContextData.getInputOptions(opts),
      );
    },
  }));
}
// The chunk a `renderChunk` or `augmentChunkHash` hook gets is one object per chunk for every hook
// of the output, the one in `meta.chunks`, as in Rollup: what a hook sets on it is seen by the hooks
// after it, and each field is read from Rust once instead of once per hook. Objects are kept with the
// identity of their Rust data, so a chunk of another output gets its own. A context that is never
// cleared (a parallel plugin's worker) keeps none and gives each call its own object, as before.
function renderedChunkOf(
  args: BindingifyPluginArgs,
  chunk: BindingRenderedChunk,
  identity = chunk.getIdentity(),
): RenderedChunk {
  const cache = args.pluginContextData.renderedChunks;
  if (!cache) return transformRenderedChunk(chunk);
  const fileName = chunk.fileName;
  const cached = cache.get(fileName);
  if (cached?.identity === identity) return cached.chunk;
  const created = transformRenderedChunk(chunk);
  cache.set(fileName, { identity, chunk: created });
  return created;
}

export function bindingifyRenderChunk(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['renderChunk'], BindingHookFilter | undefined> {
  return bindingifyHook(args.plugin.renderChunk, ({ handler, options }) => ({
    plugin: async (ctx, code, chunk, opts, meta) => {
      const data = args.pluginContextData;
      const identity = chunk.getIdentity();
      // The chunks map is made once per output and shared by every call. One made for another
      // output (an earlier rebuild, in a context that is never cleared) doesn't have this call's
      // chunk; the kept chunk objects are then those of this output only.
      if (data.renderChunkMetaIdentities.get(chunk.fileName) !== identity) {
        const identities = new Map<string, number>();
        const chunks = Object.fromEntries(
          Object.entries(meta.chunks).map(([key, value]) => {
            const valueIdentity = value.getIdentity();
            identities.set(key, valueIdentity);
            return [key, renderedChunkOf(args, value, valueIdentity)];
          }),
        );
        data.setRenderChunkMeta({ chunks });
        data.renderChunkMetaIdentities = identities;
        if (data.renderedChunks) {
          for (const key of data.renderedChunks.keys()) {
            if (!identities.has(key)) data.renderedChunks.delete(key);
          }
        }
      }
      const renderedChunk = renderedChunkOf(args, chunk, identity);
      // Per-chunk calls of this hook run concurrently, so only the chunks map
      // is shared through the cache; each call gets its own meta object. A
      // getter defined on one shared object would be replaced by whichever
      // call started last, and a hook reading `meta.magicString` after an
      // `await` would get another chunk's code.
      const renderChunkMeta: RenderedChunkMeta = {
        chunks: args.pluginContextData.getRenderChunkMeta()!.chunks,
      };

      // Add lazy-loaded magicString if nativeMagicString is enabled
      let magicStringInstance: RolldownMagicString;
      if (args.options.experimental?.nativeMagicString) {
        Object.defineProperty(renderChunkMeta, 'magicString', {
          get() {
            if (magicStringInstance) {
              return magicStringInstance;
            }
            magicStringInstance = new RolldownMagicString(code);
            return magicStringInstance;
          },
          configurable: true,
        });
      }

      const ret = await handler.call(
        createPluginContext(args, ctx),
        code,
        renderedChunk,
        args.pluginContextData.getOutputOptions(opts),
        renderChunkMeta,
      );

      if (ret == null) {
        return;
      }

      // Handle MagicString return value directly
      if (ret instanceof RolldownMagicString) {
        const normalizedCode = ret.toString();
        const generatedMap = ret.generateMap();
        return {
          code: normalizedCode,
          map: bindingifySourcemap({
            file: generatedMap.file,
            mappings: generatedMap.mappings,
            names: generatedMap.names,
            sources: generatedMap.sources,
            sourcesContent: generatedMap.sourcesContent.map((s) => s ?? null),
          }),
        };
      }

      if (typeof ret === 'string') {
        return { code: ret };
      }

      // Handle object return with code as MagicString
      if (ret.code instanceof RolldownMagicString) {
        const magicString = ret.code as RolldownMagicString;
        const normalizedCode = magicString.toString();
        // If map is explicitly null, don't generate sourcemap (opt-out)
        // If map is undefined, auto-generate from MagicString
        if (ret.map === null) {
          return { code: normalizedCode, map: null };
        }
        if (ret.map === undefined) {
          const generatedMap = magicString.generateMap();
          return {
            code: normalizedCode,
            map: bindingifySourcemap({
              file: generatedMap.file,
              mappings: generatedMap.mappings,
              names: generatedMap.names,
              sources: generatedMap.sources,
              sourcesContent: generatedMap.sourcesContent.map((s) => s ?? null),
            }),
          };
        }
        return {
          code: normalizedCode,
          map: bindingifySourcemap(ret.map),
        };
      }

      if (ret.map === null) {
        return { code: ret.code, map: null };
      }

      return {
        code: ret.code,
        map: bindingifySourcemap(ret.map),
      };
    },
    filter: bindingifyRenderChunkFilter(options.filter),
  }));
}

export function bindingifyAugmentChunkHash(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['augmentChunkHash']> {
  return bindingifyHook(args.plugin.augmentChunkHash, ({ handler }) => ({
    plugin: async (ctx, chunk) => {
      return handler.call(createPluginContext(args, ctx), renderedChunkOf(args, chunk));
    },
  }));
}

export function bindingifyResolveFileUrl(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['resolveFileUrl']> {
  return bindingifyHook(args.plugin.resolveFileUrl, ({ handler }) => ({
    plugin: async (ctx, resolveFileUrlArgs) => {
      return handler.call(createPluginContext(args, ctx), resolveFileUrlArgs);
    },
  }));
}

export function bindingifyRenderError(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['renderError']> {
  return bindingifyHook(args.plugin.renderError, ({ handler }) => ({
    plugin: async (ctx, err) => {
      await handler.call(createPluginContext(args, ctx), aggregateBindingErrorsIntoJsError(err));
    },
  }));
}

function createOutputBundle(
  args: BindingifyPluginArgs,
  ctx: BindingPluginContext,
  bundle: BindingResult<BindingBundle>,
) {
  const context = createPluginContext(args, ctx);
  const cache = args.pluginContextData.outputBundleCache;
  const output = transformToSharedOutputBundle(
    context,
    unwrapBindingResult(bundle),
    cache,
    args.pluginContextData.outputCodeCache ?? undefined,
  );
  return { changed: cache.changed, context, output };
}

export function bindingifyGenerateBundle(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['generateBundle']> {
  return bindingifyHook(args.plugin.generateBundle, ({ handler }) => ({
    plugin: async (ctx, bundle, isWrite, opts) => {
      const { changed, context, output } = createOutputBundle(args, ctx, bundle);
      await handler.call(context, args.pluginContextData.getOutputOptions(opts), output, isWrite);
      return collectChangedBundle(changed, output);
    },
  }));
}

export function bindingifyWriteBundle(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['writeBundle']> {
  return bindingifyHook(args.plugin.writeBundle, ({ handler }) => ({
    plugin: async (ctx, bundle, opts) => {
      const { changed, context, output } = createOutputBundle(args, ctx, bundle);
      await handler.call(context, args.pluginContextData.getOutputOptions(opts), output);
      return collectChangedBundle(changed, output);
    },
  }));
}

export function bindingifyCloseBundle(
  args: BindingifyPluginArgs,
): PluginHookWithBindingExt<BindingPluginOptions['closeBundle']> {
  return bindingifyHook(args.plugin.closeBundle, ({ handler }) => ({
    plugin: async (ctx, err) => {
      await handler.call(
        createPluginContext(args, ctx),
        err ? aggregateBindingErrorsIntoJsError(err) : undefined,
      );
    },
  }));
}

export function bindingifyAddonHook<K extends 'banner' | 'footer' | 'intro' | 'outro'>(
  args: BindingifyPluginArgs,
  name: K,
): PluginHookWithBindingExt<BindingPluginOptions[K]> {
  return bindingifyHook(args.plugin[name], ({ handler }) => ({
    plugin: async (ctx, chunk) => {
      if (typeof handler === 'string') {
        return handler;
      }

      return handler.call(createPluginContext(args, ctx), transformRenderedChunk(chunk));
    },
  }));
}
