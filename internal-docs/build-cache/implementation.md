# Persistent Build Cache - Implementation

> Rationale, trade-offs and limitations live in [design.md](./design.md).

## Summary

`experimental.buildCache` persists the result of a module task's plugin
pipeline per module: the post-transform code, module type, sourcemap chain,
side-effect override, the resolved dependencies, the module's `meta`, and
the files the result depends on with their content hashes. On a hit, `ModuleTask`
restores all of it from disk and never calls a `resolveId`, `load` or
`transform` hook, nor the native resolver, for that module. Parsing and
scanning re-run natively on the cached code; everything downstream (link,
codegen) is unchanged.

## Components

| Piece               | Location                                                                                        |
| ------------------- | ----------------------------------------------------------------------------------------------- |
| Option types        | `crates/rolldown_common/src/inner_bundler_options/types/experimental_options.rs`                |
| Store + entry codec | `crates/rolldown/src/utils/build_cache.rs`                                                      |
| Construction        | `BundleFactory::create_bundle` (`crates/rolldown/src/bundle/bundle_factory.rs`)                 |
| Read/keep seam      | `ModuleTask::run_inner` (`crates/rolldown/src/module_loader/module_task.rs`)                    |
| Write               | `Bundle::save_build_cache` (`crates/rolldown/src/bundle/bundle.rs`)                             |
| `meta` handover     | `buildCacheMetaPlugin` (`packages/rolldown/src/utils/build-cache-meta.ts`)                      |
| Binding             | `BindingBuildCacheOptions` (`crates/rolldown_binding/.../binding_experimental_options.rs`)      |
| JS options          | `packages/rolldown/src/options/input-options.ts`, `validator.ts`, `bindingify-input-options.ts` |

## Data flow

1. `BundleFactory::create_bundle` builds one `BuildCache` per build when the
   option is enabled; `ModuleLoader::new` leaves it out when the native
   magic-string sourcemap channel is active (those sourcemaps are generated
   outside the module task and could not be captured; see
   `create_sourcemap_channel` in `scan_stage.rs`).
   `BuildCache::new` also returns `None` when no plugin beyond rolldown's
   cheap inner plugins registers a `resolveId`/`load`/`transform` hook; with
   nothing to skip, the cache would be pure overhead.
2. `ModuleTask::run_inner` calls `BuildCache::lookup` before running any
   hook. It hashes the file behind the id (the id without its query); ids
   that are not absolute filesystem paths (virtual modules, data URLs,
   `rolldown:`) bypass the cache, and a file that exists but can't be read
   makes every id of it uncacheable. File and directory hashes are memoized
   for the build.
3. The key is
   `xxh3(salt, stable_id, asserted_module_type, xxh3(file_bytes))`, hex
   encoded, where
   `salt = xxh3(format_version, rolldown_version, options.key, platform, sourcemap on/off, sorted moduleTypes, ordered plugin names)`.
   `stable_id` (cwd-relative, forward slashes) keeps keys portable across
   machines, which remote cache layers rely on.
4. Hit (the entry decoded, none of its files is in `invalidate`, and every
   file it lists hashes as recorded):
   - the module id is registered as a watch file, mirroring the
     read-from-disk path in `load_source`;
   - the stored sourcemap chain (including `Load` elements) replaces the
     empty chain, the side-effects override is applied if one was stored, and
     the cached code + module type feed straight into `create_ecma_view`;
   - the stored JS module options (`meta`, `moduleSideEffects`) are put in
     `PluginDriver::reused_module_meta`, which the JS side reads the first
     time the module's options are read, replacing what it has;
   - after the native scan, the cached `ResolvedId`s are used instead of
     calling `resolve_dependencies`. They are positionally aligned with the
     scanned import records; a length mismatch falls back to fresh
     resolution.
5. Miss: the normal pipeline runs (`load_source` → `transform_source` →
   `create_ecma_view` → `resolve_dependencies`). If the source was a string
   and resolution emitted no warnings, the entry is kept in memory
   (`BuildCache::keep`): code, final module type, side-effects delta (only if
   the load/transform pipeline changed it), the sourcemap chain (empty
   without output sourcemaps), the resolved dependencies, and its files: the
   module's file, the `addWatchFile` paths of `load` and `transform`, the
   tsconfig dependencies (`BuildCache::tsconfig_deps`), local `package.json`s
   and directory-entry dependencies (`ModuleTask::resolution_dirs`).
   While the cache is on, the task runs inside `HOOK_MODULE` (a tokio
   task-local naming the module), so native hooks that emit a file or watch
   one meanwhile, the resolution of its imports included, record it in
   `PluginDriver::uncacheable_modules` and `module_hook_dependencies`. JS
   plugin contexts know the module their hook works for (`cacheModule`):
   `load` and `transform` their own, `resolveId` the one Rust passes in
   (`HOOK_MODULE`). They report through `markModuleUncacheable` and
   `addModuleWatchFile`, and pass it to `this.resolve`, which runs the
   resolution in that scope; `PluginContext::resolve` from native code
   opens it for the importer when none is set. Each such resolution is
   recorded in `PluginDriver::module_hook_resolutions`, and the module's
   entry gets its directory dependencies with those of its imports.
6. After the output is written (or generated), `Bundle::save_build_cache`
   calls `BuildCache::save`: uncacheable ids and ids whose options are not
   JSON make their file uncacheable, each kept entry gets the union of
   the files of every id of its file (the files hooks watched while
   resolving its imports included) and the `meta` the JS side handed over,
   and is written unless it is a hit that would be written unchanged.
   Entries of ids that turned out uncacheable are removed, and
   `<dir>/build-deps.json` is rewritten if it changed.

## Entry format

`<dir>/build-v1/<key[0..2]>/<key>`, written to a temp file unique to the
process and the write (created exclusively) and renamed into place. Layout:

```text
b"RDBC"  format_version:u8 (4)  meta_len:u64le  meta_json  raw code bytes  xxh3(everything before):u64le
```

`meta_json` holds `files` (path and content hash; a directory-entry
dependency is `dir\0prefix`, a tsconfig one `path\0\0tsconfig`), `module_type`, the optional `side_effects`
override (0/1/2), the serialized chain elements (`Load`/`Transform` maps as
sourcemap JSON, `Omitted`/`Null` with plugin index and payload),
`resolved_deps`, one object per import record: the resolved id,
`ModuleDefFormat`, external kind, side-effect data and the dependency's
`package.json` fields (name, version, type, `sideEffects`, realpath), and
`meta`. Absolute paths inside entries are stored cwd-relative and
re-absolutized against the reader's cwd, sourcemap `sources` included
(marked by a leading `\u{1}`); ids that are not absolute paths are marked
with a leading `=` (a relative path starting with `=` gets `./` first), and
a query stays as it is. Any decode failure, size or checksum mismatch, or
unknown version is treated as a miss and the entry is rewritten.

## Invariants

- Cache failures never fail the build: reads degrade to misses, and a write
  that fails is reported as a warning with the cache stats.
- `PluginIdx` values inside cached chains are only meaningful because the
  ordered plugin name list is part of the salt; changing the plugin list
  invalidates every entry.
- Cached `resolved_deps` line up with the import records scanned from the
  cached code because scanning is deterministic; the length check is the
  corruption guard.
- The runtime module task and emitted-entry virtual modules never touch the
  cache (their ids are not absolute paths).
