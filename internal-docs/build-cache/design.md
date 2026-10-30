# Persistent Build Cache - Design

> The data flow and file pointers live in [implementation.md](./implementation.md).

## Problem

For most projects rolldown is fast enough that persistent caching is not worth
its complexity (see the discussion in
[#802](https://github.com/rolldown/rolldown/discussions/802)). Two situations
break that assumption (tracked in
[#6995](https://github.com/rolldown/rolldown/issues/6995)):

- Slow JavaScript plugin hooks (framework compilers, legacy transpilers,
  custom resolvers). Every call crosses the NAPI boundary and runs
  single-threaded JS, so the `resolveId`/`load`/`transform` pipeline dominates
  cold-build time.
- Very large module graphs (100k+ modules in monorepos), where even cheap
  per-module work adds up and CI machines rebuild the same unchanged modules
  thousands of times a day.

The existing `ScanStageCache` only helps rebuilds within one process (watch
mode / HMR). Nothing survives a process restart, and nothing can be shared
between machines.

An earlier iteration ([#10137](https://github.com/rolldown/rolldown/pull/10137))
cached only the `transform` stage. Review feedback was that rolldown targets
caching for the whole bundler pipeline, not a single hook, so this design
covers everything a module task does before the native parse: the `load`
pipeline, the `transform` pipeline and dependency resolution (`resolveId`
hooks plus the native resolver).

## Goals

- Opt-in, experimental persistent cache that skips every plugin hook and the
  resolver for unchanged modules, across processes.
- Never fail or corrupt a build: every cache error degrades to a miss, every
  write is atomic.
- Warm builds must produce byte-identical output to cold builds, including
  sourcemaps and side-effect-driven tree shaking.
- On-disk layout simple enough that external tooling can implement **remote
  caching** on top (e.g. read-through from an HTTP mirror, CI writers
  populating an object store) by syncing files, without understanding the
  entry format. Cache keys and entry contents must therefore be
  machine-portable (no absolute paths).
- Smallest possible surface area inside rolldown; the cache touches exactly
  one seam (`ModuleTask::run_inner`).

## Non-goals

- Caching parse, link or codegen results. The oxc AST is arena-allocated and
  self-referential, so it cannot be serialized without a dedicated
  representation; parsing and scanning are native and fast relative to JS
  plugin hooks. Re-parsing the cached post-transform source reproduces the
  scan state, including the import records the cached resolutions align with.
- Automatic invalidation on plugin configuration or implementation changes.
  Rolldown cannot observe either, so callers fold a config/lockfile hash into
  `experimental.buildCache.key` (the same contract Metro's `getCacheKey` and
  webpack's `cache.version` use).
- Replacing `ScanStageCache`. The two compose: within a watch session the
  in-memory cache avoids re-scanning entirely; the persistent cache
  accelerates the scans that do happen.

## Decisions and trade-offs

- **Key on the module's raw on-disk content, not the post-`load` source.**
  Keying on post-`load` source (as #10137 did) forces `load` hooks to run on
  every build. Reading the file directly makes the key available before any
  hook runs, so hits skip the whole pipeline. The cost: modules that do not
  map to a real file (virtual modules, data URLs) bypass the cache entirely,
  and `load`/`transform` hooks are assumed to be deterministic functions of
  the module's id and content, the same determinism contract Metro and
  webpack's filesystem cache rely on.
- **Cache resolved dependencies positionally, not by specifier.** Scanning the
  cached code is deterministic, so the import records of a warm build are
  identical to the cold build's and a plain array lines up with them. A length
  mismatch (corrupt entry) falls back to fresh resolution.
- **An entry lists the files its result depends on, with their content
  hashes, and is used only while they all match.** The module's own file,
  every path a hook passed to `this.addWatchFile` for it (in `load` and
  `transform`, or while its imports were resolved; a directory counts by its
  entry names), the tsconfig that governs it by the config it resolves to
  (`extends` and references included, paths made relative so a checkout
  elsewhere hashes the same) and the directories up to it by their
  `tsconfig.json` entries, and the `package.json` that governs the module
  and each local file its imports resolve to, with the directories up to it
  by their `package.json` entries, so that one added closer starts a new
  package scope (`sideEffects` and `type` are kept with each resolution).
  What a hook watches through `this.resolve` belongs to the module whose
  hook called it, with or without an importer, and what `this.resolve`
  resolved counts like one of the module's imports (below), since the hook
  may use the result. A hook that
  reads a file it declares is then safe to cache, which is the common case
  for framework compilers (Vue SFC sub-blocks, component registries,
  generated types).
- **Validate cached resolutions by directory entries, not by stat-ing the
  resolved files.** For each relative import the entry records the entry
  names of the directory the import points into that start with the
  import's last segment without its extension (`./dep.js` → `dep*`, as an
  extension alias may find `dep.ts`), and for each resolved file the names
  next to it that start with its stem. Names mark directories, so a file
  replaced by a directory of the same name is seen. A deleted or moved dependency
  changes these names, and so does a new file that would win the resolution
  (`dep.js` next to `dep/index.js`, `Foo.ts` next to `Foo.vue`), which a
  stat of the old target can't see. A symlink counts with where its chain
  finally leads, and a directory reached through a symlink with where it
  really is, so pointing a symlink, or one further along its chain,
  elsewhere changes them too. A bare import also depends on the names at
  every candidate the governing tsconfig's `paths` and `baseUrl` give it,
  existing or not, and on the `index` and `package.json` names inside it:
  resolution tries them before `node_modules`, so a file or a directory
  main appearing at one wins.
- **Ids of one file share their dependencies.** `X.vue?vue&type=script` and
  `X.vue?vue&type=style&index=0` are built from state a plugin made while
  processing `X.vue`, so every entry of a file lists the dependencies of all
  of its ids. Ids with a query are keyed by the file behind them, without
  the query.
- **Module `meta` and `moduleSideEffects` are kept as the build left them**,
  as Rollup's `cache` keeps `meta`. JS plugins' options for each module are
  handed to Rust after every other `buildEnd` and stored in the module's
  entry; a hit restores them before any later hook reads them, replacing
  what `resolveId` set again (keys a hook removed stay removed).
  `moduleParsed` and `buildEnd` run on hits too and see their own changes of
  the build before, so they should set values rather than add to them (a
  plugin can cache work in `meta` this way). A module whose `meta` JSON
  would not bring back the same (`undefined`, functions, `Date`, `Map`,
  class instances, symbol or accessor properties, shared or cyclic
  objects, ...) is not cached, nor are the other ids of its file.
- **Modules whose hooks do what a hit can't replay are not cached**, nor are
  the other ids of their file: emitting a file or a chunk (a hit would drop
  it), `this.load` of another module from a JS or native hook (the result
  depends on that module), and resolving an import to an external module
  with `meta` (an external has no entry to bring it back; checked when the
  entries are written, so it holds however the module became external); any
  JS options count, `moduleSideEffects` too, which Vite sets on SSR
  externals: measured on a Nuxt app, 44 of 1,205 SSR modules, under 0.1 s). That covers `load` (Rolldown's own asset
  module types included), `transform`, the resolution of the module's
  imports (`builtin:copy-module`, a `resolveId` that emits), and what
  resolvers do for `this.resolve` from those hooks. Native hooks are seen
  through a task-local naming the module whose task runs, only set while
  the cache is on; JS hooks report the module themselves.
- **Entries are written when the build succeeds**, in parallel, after the
  output is written and before the build returns: the dependency union and
  `meta` are only known then, the scan never waits on a write, and a failed
  build writes nothing. An entry already on disk as it would be written is
  not rewritten. Writing is 20-110 ms per environment in the Nuxt apps we
  measured (5-12k modules). Each write goes to a temp file of its own,
  created exclusively, and is renamed into place; an entry ends with a
  checksum of the rest, so a cut or mixed file reads as a miss.
- **`build-deps.json` lists every file the build depended on**: the
  dependencies of all entries, every watched file, and the files of modules
  that were not cached. A tool that reuses a whole build's output (skips the
  build) checks these.
- **Serialize the full `ResolvedId`, including `package.json` data.** Side
  effects derived from `package.json` `sideEffects` globs are evaluated in the
  dependency's own task via `resolved_id.package_json`, so dropping it would
  change tree shaking between cold and warm builds. `PackageJson` is five
  small fields; paths inside entries are stored cwd-relative for portability.
- **Modules whose resolution produced warnings are never stored.** Warnings
  (e.g. "treating missing import as external") are emitted during resolution
  and would silently disappear on hits; re-running the pipeline keeps them.
- **One file per entry, content-addressed, atomic rename.** Rejected a single
  store file / database: per-entry files need no locking across concurrent
  builds, tolerate partial syncs, and are trivially mirrored to remote
  storage. The 256-way `key[0..2]` sharding keeps directories small at
  monorepo scale.
- **Format: JSON metadata header + raw code bytes.** Rejected new
  serialization dependencies (bincode/rkyv); `serde_json` is already a
  dependency, and keeping the code blob out of JSON avoids escaping the
  largest field. A format version byte plus the salt guard both decode
  compatibility and semantic compatibility.
- **Side effects are stored as a delta.** The pre-`load` value comes from
  resolution data outside the cache key, so only a load/transform-hook
  override is stored and replayed.
- **The cache disables itself when no plugin registers a cacheable hook.**
  With nothing to skip, the cache would only add per-module fs reads and
  entry decoding (measured 1.8x slower warm, 5.7x slower cold on a 10k-module
  plugin-free build). Rolldown's always-registered inner plugins
  (`copy-module`, `asset-module`, `data-url`, `oxc-runtime`) are exempt from
  the check: their hooks are native early-return filters cheaper than the
  cache itself.

## Known limitations

- Files hooks read without `this.addWatchFile` are invisible. Callers fold
  them into `key`, or list them in `invalidate`, which only matches files
  an entry lists as a dependency (to re-run a module because of a file it
  doesn't list, list the module's own file). Plugin options and Rolldown's
  `resolve` options are not in the salt either: they belong in `key`.
- Hooks that change other modules' `meta` or plugin state while a module is
  processed lose those changes when the module is reused, like any other
  side effect of a skipped hook. A hook that reads another module through
  `this.getModuleInfo` (its code, its imports) has the same problem;
  `this.load` makes the caller uncacheable, `getModuleInfo` doesn't.
  Vite's `vite:build-html` calls `this.load` on an HTML entry's scripts to
  set their `moduleSideEffects`, so `index.html` is never reused (measured:
  ~0.2 s more per build of a plain Vite app with one HTML entry).
- `compilerOptions.rootDirs` candidates of relative imports are not
  tracked. The tracked candidates are a superset worked out from the
  specifier, not the resolver's own list of what it tried
  (`oxc_resolver`'s missing dependencies would be exact).
- Inherited from #10159: a `load` or `transform` hook that sets
  `moduleSideEffects` to the value resolution gave stores no override, so
  if resolution later gives another value, the hit takes it.
- Installed packages are not hashed (`node_modules` paths are left out of
  the directory dependencies); callers fold the lockfile into `key`.
- Incompatible with `experimental.nativeMagicString` + sourcemaps: that path
  generates transform sourcemaps on a side channel outside the module task, so
  the cache silently disables itself there.
- No eviction. Entries are small and content-addressed; callers can wipe or
  age out the directory externally. Revisit if this graduates from
  experimental.

## Open questions

- Should the store become a trait (get/set) so embedders can plug remote
  backends directly instead of syncing the directory? The current layout was
  chosen so that this can be added later without changing the entry format.
- Whether Vite should derive `key` automatically from its resolved config.
- Whether entry-point resolution (`resolve_user_defined_entries`) is worth
  caching too; it is a handful of resolutions per build.
