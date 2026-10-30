import type { Plugin } from '../plugin';
import type { PluginContextData } from '../plugin/plugin-context-data';

// `experimental.incrementalBuildCache` keeps each module's `meta` in the module's record in Rust,
// so it is restored exactly when the record is reused, and a module whose hooks run again starts
// from no `meta`. `PluginContextData.getModuleOption` restores it the first time a module's options
// are read after its task decided; this plugin hands every module's `meta` back to Rust when the
// scan ends, before the records are written. `meta` set after `buildEnd` is not kept.

/** Last in the plugin list, so it runs after every other `buildEnd`. */
export function incrementalBuildCacheMetaPlugin(data: PluginContextData): Plugin {
  return {
    name: 'rolldown:incremental-build-cache-meta',
    buildEnd: {
      order: 'post',
      handler(error) {
        if (error || !data.bindingContext) return;
        const ids: string[] = [];
        const metas: string[] = [];
        for (const id of data.moduleOptionMap.keys()) {
          const { meta } = data.getModuleOption(id);
          ids.push(id);
          let json = '';
          try {
            if (meta && Object.keys(meta).length > 0) json = JSON.stringify(meta);
          } catch {
            // Not JSON: a reused record would come back without it, so the module isn't kept.
            json = '!';
          }
          metas.push(json);
        }
        data.bindingContext.setModuleMetaForCache(ids, metas);
      },
    },
  };
}
