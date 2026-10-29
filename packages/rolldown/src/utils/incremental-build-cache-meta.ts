import fs from 'node:fs';
import path from 'node:path';
import type { InputOptions } from '../options/input-options';
import type { PluginContextData } from '../plugin/plugin-context-data';

// `experimental.incrementalBuildCache` keeps module records in Rust; the `meta` plugins return
// from hooks lives here on the JS side, so it is kept next to them. A reused module's hooks do
// not run, and its `meta` must still be there for `this.getModuleInfo(id).meta`.
const META_FILE = 'module-meta.json';

type CacheOptions = NonNullable<NonNullable<InputOptions['experimental']>['incrementalBuildCache']>;

interface MetaFile {
  key: string | null;
  meta: Record<string, Record<string, any>>;
}

/** The file as loaded, so a build that changed no `meta` doesn't write it again. */
const loaded = new WeakMap<PluginContextData, string>();

export function loadModuleMeta(cache: CacheOptions, data: PluginContextData): void {
  let text: string;
  let file: MetaFile;
  try {
    text = fs.readFileSync(path.join(cache.dir, META_FILE), 'utf8');
    file = JSON.parse(text);
  } catch {
    return;
  }
  loaded.set(data, text);
  if (file.key !== (cache.key ?? null)) return;
  for (const [id, meta] of Object.entries(file.meta)) {
    data.updateModuleOption(id, { meta, moduleSideEffects: null, invalidate: false });
  }
}

/** Writes the `meta` of every module that has some. Call after a successful build. */
export function saveModuleMeta(cache: CacheOptions, data: PluginContextData): void {
  const meta: MetaFile['meta'] = {};
  for (const [id, option] of data.moduleOptionMap) {
    if (option.meta && Object.keys(option.meta).length > 0) meta[id] = option.meta;
  }
  const text = JSON.stringify({ key: cache.key ?? null, meta });
  if (loaded.get(data) === text) return;
  fs.mkdirSync(cache.dir, { recursive: true });
  const target = path.join(cache.dir, META_FILE);
  fs.writeFileSync(`${target}.tmp`, text);
  fs.renameSync(`${target}.tmp`, target);
}
