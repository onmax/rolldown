//! Persistent (filesystem) cache for per-module build pipeline results:
//! `load` hooks, `transform` hooks and dependency resolution.
//!
//! An entry holds what the hooks produced for one module: its code after `transform`, the module
//! type, the hooks' side effects override, the sourcemap chain, the resolved ids of its imports,
//! the module's JS options (`meta`, `moduleSideEffects`), and the files the result depends on with the hash of their
//! content. Parsing and scanning re-run natively on the cached code, so a hit builds the module a
//! full scan would build. Like `experimental.incrementalBuild`, this assumes `load`, `transform`
//! and `resolveId` have no side effects.
//!
//! An entry is found by a key covering the cache salt, the module's stable id and the hash of the
//! file behind the id (without its query), and is used when every file it depends on hashes the
//! same and none is listed in `invalidate`. The dependencies are:
//!
//! - the file itself, and the files the hooks declared with `this.addWatchFile`, in `load` and
//!   `transform`, while the module's imports were resolved, or for `this.resolve` from its hooks;
//! - the tsconfig that governs the module, by the config it resolves to (`extends` and references
//!   included), and the directories up to it by their `tsconfig.json` entries, so that one added
//!   closer is seen;
//! - the `package.json` that governs the module and each local file its imports resolved to, and
//!   the directories up to it by their `package.json` entries, so that one added closer is seen;
//! - directories by their entry names (directories marked): the entries of the directories its
//!   imports, and what its hooks resolved through `this.resolve`, resolved into
//!   that are named like the import (`entries_dep`), so that a new file that would win the
//!   resolution (`dep.js` next to `dep/index.js`) re-runs the importer, the same at every tsconfig
//!   `paths` and `baseUrl` candidate of a bare import, and all entries of a directory passed to
//!   `addWatchFile`. A symlink counts with where its chain finally leads and a directory with
//!   where it really is, so pointing a symlink elsewhere re-runs the importer too.
//!
//! Ids of one file that differ by query (`X.vue?vue&type=script`) are made from state a plugin
//! built while processing another of them, so each of their entries lists the dependencies of all
//! of them. A file that is missing is recorded as missing; one that exists but can't be read makes
//! the module uncacheable. Not cached: virtual modules, binary modules, builds with native magic
//! string, modules whose resolution warned, and every id of a file one of whose ids did in a hook
//! a hit would skip (`load`, `transform`, the resolution of its imports) what a hit can't replay
//! (emitted a file or a chunk, loaded another module, resolved an external with `meta`), or has
//! options that JSON would not bring back the same. Inputs Rolldown can't see (plugin and
//! resolve options, files hooks read without declaring them) belong in `key` or `invalidate`.
//!
//! Entries are written at the end of a successful build, before it returns: the dependency union
//! and the `meta` are only known then, and the scan does not wait on writes. An entry ends with a
//! checksum, so a cut or mixed write reads as a miss.
//!
//! See `internal-docs/build-cache/design.md` for the rationale and
//! `internal-docs/build-cache/implementation.md` for the data flow.

use std::{
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
  },
};

use arcstr::ArcStr;
use rolldown_common::{
  ModuleDefFormat, ModuleId, ModuleType, PackageJson, ResolvedExternal, ResolvedId,
  SharedNormalizedBundlerOptions, SourcemapChainElement, side_effects::HookSideEffects,
};
use rolldown_plugin::{HookUsage, PluginDriver};
use rolldown_sourcemap::SourceMap;
use rolldown_utils::{
  dashmap::{FxDashMap, FxDashSet},
  rayon::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator},
  stabilize_id::stabilize_id,
  xxhash::xxhash_with_base,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use sugar_path::SugarPath;

/// Bump whenever the on-disk entry layout changes. Mixed into the cache salt,
/// so old entries simply stop matching instead of failing to decode.
const FORMAT_VERSION: u8 = 4;
const MAGIC: [u8; 4] = *b"RDBC";
const HEADER_LEN: usize = MAGIC.len() + 1 + 8;
/// An entry ends with the xxh3 of everything before it, so a cut or mixed write reads as a miss.
const CHECKSUM_LEN: usize = 8;
/// Marks a sourcemap source stored relative to `cwd` (no real source starts with it).
const PORTABLE_SOURCE: char = '\u{1}';
/// `<path>` + this: a tsconfig file, hashed by the config it resolves to (`extends` included).
/// An entries dependency is `<dir>\0<prefix>`, and no prefix starts with NUL.
pub const TSCONFIG_DEP: &str = "\0\0tsconfig";
/// See `BuildCache::save`.
pub const BUILD_DEPS: &str = "build-deps.json";

/// What saving does with one kept entry.
enum Saved {
  Write(PathBuf, CachedModule),
  Remove(PathBuf),
  Keep,
}

/// The entries of one file as saved, and the dependencies they list after the file itself.
type SavedFile = (Vec<Saved>, Vec<(String, u64)>);

/// What every entry of `file` lists after its own first item: the dependencies of all of them and
/// `extra`, without `file` itself, sorted, each once.
fn shared_rest(
  file: &str,
  entries: &[&[(String, u64)]],
  extra: Option<&[(String, u64)]>,
) -> Vec<(String, u64)> {
  if let ([only], None) = (entries, extra)
    && is_canonical(only)
  {
    return only[1..].to_vec();
  }
  let mut set: FxHashSet<&(String, u64)> = FxHashSet::default();
  for files in entries {
    set.extend(files.iter());
  }
  set.extend(extra.into_iter().flatten());
  let mut rest: Vec<(String, u64)> = set.into_iter().filter(|dep| dep.0 != file).cloned().collect();
  rest.sort_unstable();
  rest
}

/// `files` in the stored order: the module's file, then its other dependencies sorted, each once.
fn is_canonical(files: &[(String, u64)]) -> bool {
  let Some(((first, _), rest)) = files.split_first() else { return true };
  rest.iter().all(|(f, _)| f != first) && rest.windows(2).all(|w| w[0] < w[1])
}

/// Rolldown's always-registered inner plugins (see `apply_inner_plugins`):
/// their hooks are native early-return filters whose cost is far below the
/// cache's own per-module overhead, so they don't count as cacheable work.
const CHEAP_INNER_PLUGINS: [&str; 4] =
  ["builtin:copy-module", "builtin:asset-module", "builtin:data-url", "builtin:oxc-runtime"];

/// The hash recorded for a dependency that doesn't exist.
pub const MISSING: u64 = 0;
const DIR_SEED: u64 = 0x0d1e_c70e;

/// One module's cached result, with absolute paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedModule {
  /// Files this result depends on, with the hash of their content when it was cached.
  /// The first entry is the file behind the module id.
  pub files: Vec<(String, u64)>,
  pub code: String,
  pub module_type: String,
  /// `Some` only when a `load` or `transform` hook overrode the side effects, so a hit never
  /// clobbers side effects derived from resolution data outside the entry.
  pub side_effects: Option<u8>,
  pub sourcemap_chain: Vec<CachedSourcemap>,
  pub resolved_deps: Vec<CachedResolvedId>,
  /// The module's `meta` from JS plugins (JSON), restored on a hit.
  pub meta: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CachedSourcemap {
  Transform { plugin_idx: u32, map: String },
  Load { map: String },
  Omitted { plugin_idx: u32, plugin_name: String },
  Null { plugin_idx: u32, original_content: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedResolvedId {
  pub id: String,
  pub module_def_format: u8,
  pub external: u8,
  pub normalize_external_id: Option<bool>,
  pub package_json: Option<CachedPackageJson>,
  pub side_effects: Option<u8>,
  pub is_external_without_side_effects: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedPackageJson {
  pub name: Option<String>,
  pub version: Option<String>,
  pub r#type: Option<String>,
  pub side_effects: Option<CachedPackageSideEffects>,
  pub realpath: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CachedPackageSideEffects {
  Bool(bool),
  String(String),
  Array(Vec<String>),
}

/// The JSON part of an entry: everything but the code, with absolute paths stored relative to
/// `cwd` so entries stay portable across machines sharing the cache.
#[derive(Serialize, Deserialize)]
struct EntryMeta {
  files: Vec<(String, u64)>,
  module_type: String,
  side_effects: Option<u8>,
  sourcemap_chain: Vec<CachedSourcemap>,
  resolved_deps: Vec<CachedResolvedId>,
  meta: Option<String>,
}

/// What `BuildCache::lookup` found for a module.
pub enum Lookup {
  /// Not cacheable: no file behind the id, or the file can't be read.
  Bypass,
  /// No usable entry; the result can be kept under `key`.
  Miss {
    key: String,
  },
  Hit {
    key: String,
    module: CachedModule,
  },
}

#[derive(Debug, Default)]
pub struct BuildCacheStats {
  pub hit: AtomicU32,
  pub missing: AtomicU32,
  pub changed: AtomicU32,
  pub invalidated: AtomicU32,
  pub excluded: AtomicU32,
  pub kept: AtomicU32,
}

struct Kept {
  key: String,
  module: Arc<CachedModule>,
  /// A hit kept as it was read.
  unchanged: bool,
}

/// A content-addressed store of [`CachedModule`] entries: one file per entry
/// at `<dir>/build-v1/<key[0..2]>/<key>`. The layout is deliberately dumb so
/// external tooling can sync it to and from remote storage without
/// understanding the entry format. One `BuildCache` serves one build.
pub struct BuildCache {
  root: PathBuf,
  dir: PathBuf,
  salt: String,
  cwd: PathBuf,
  invalidate: FxHashSet<String>,
  // LAB ONLY: `ROLLDOWN_BUILD_CACHE_EXCLUDE`, comma-separated id substrings never reused.
  // Stands in for a plugin-declared opt-out until `shouldTransformCachedModule` exists.
  exclude: Vec<String>,
  kept: FxDashMap<ArcStr, Kept>,
  /// The key looked up for each id, so entries of ids that turn out uncacheable are removed.
  keys: FxDashMap<ArcStr, String>,
  /// Files with an id that can't be cached this build; none of their ids is kept, since the
  /// dependencies of the one left out would be missing from the others.
  uncacheable_files: FxDashSet<String>,
  file_hashes: FxDashMap<String, Option<u64>>,
  /// Each directory read this build (`Err`: missing or unreadable).
  dir_entries: FxDashMap<String, Arc<Result<DirEntries, std::io::ErrorKind>>>,
  options: SharedNormalizedBundlerOptions,
  /// `cwd` with symlinks resolved, to tell a directory reached through a symlink.
  real_cwd: Option<PathBuf>,
  pub stats: BuildCacheStats,
}

struct DirEntries {
  /// Sorted entry names, a directory's (or a symlink to one) followed by `/`, so that a file
  /// replaced by a directory of the same name is seen; a symlink's name is also followed by `\0`
  /// and where it finally leads (relative to the real `cwd`; its own target when that is missing),
  /// so that pointing it, or a link further along its chain, elsewhere changes the hash of a
  /// dependency on its name.
  names: Vec<String>,
  /// Where the directory is when reached through a symlink (relative to the real `cwd`), `None`
  /// when it is where its path says.
  real: Option<String>,
}

impl BuildCache {
  pub fn new(
    options: &SharedNormalizedBundlerOptions,
    plugin_driver: &PluginDriver,
  ) -> Option<Arc<Self>> {
    let cache_options = options.experimental.build_cache_options()?;

    // The cache can only save `resolveId`/`load`/`transform` hook work plus
    // the dependency resolution it replays. When no plugin (beyond the cheap
    // inner ones) registers any of those hooks there is nothing expensive to
    // skip and the cache would only add fs reads and entry decoding to every
    // module, so it disables itself.
    let cached_hooks = HookUsage::ResolveId
      | HookUsage::ResolveDynamicImport
      | HookUsage::Load
      | HookUsage::Transform;
    let has_cacheable_work = plugin_driver.plugins().iter().any(|plugin| {
      plugin.call_hook_usage().intersects(cached_hooks)
        && !CHEAP_INNER_PLUGINS.contains(&plugin.call_name().as_ref())
    });
    if !has_cacheable_work {
      return None;
    }

    let root =
      options.cwd.join(cache_options.dir.as_deref().unwrap_or("node_modules/.cache/rolldown"));
    let dir = root.join("build-v1");

    // Everything that invalidates the whole cache goes into the salt. Plugin
    // configurations and implementations are invisible here; callers fold
    // those into `key` (see `BuildCacheOptions::key`).
    let mut salt_input = vec![FORMAT_VERSION];
    salt_input.extend_from_slice(env!("CARGO_PKG_VERSION").as_bytes());
    salt_input.push(0);
    salt_input.extend_from_slice(cache_options.key.as_deref().unwrap_or_default().as_bytes());
    salt_input.push(0);
    salt_input.extend_from_slice(format!("{:?}", options.platform).as_bytes());
    // Without sourcemaps in the output the chain is not kept, so an entry written without one
    // can't serve a build that wants it.
    salt_input.push(u8::from(options.sourcemap.is_some()));
    // `moduleTypes` decides module types before the cached seam; sorted for a
    // deterministic hash across processes.
    let mut module_types: Vec<_> = options.module_types.iter().collect();
    module_types.sort_by_key(|(a, _)| *a);
    for (extension, module_type) in module_types {
      salt_input.push(0);
      salt_input.extend_from_slice(extension.as_bytes());
      salt_input.push(0);
      salt_input.extend_from_slice(module_type.to_string().as_bytes());
    }
    for plugin in plugin_driver.plugins() {
      salt_input.push(0);
      salt_input.extend_from_slice(plugin.call_name().as_bytes());
    }
    let salt = xxhash_with_base(&salt_input, 16);

    Some(Arc::new(Self {
      root,
      dir,
      salt,
      cwd: options.cwd.clone(),
      invalidate: cache_options.invalidate.iter().flatten().cloned().collect(),
      exclude: std::env::var("ROLLDOWN_BUILD_CACHE_EXCLUDE")
        .map(|v| v.split(',').filter(|s| !s.is_empty()).map(ToString::to_string).collect())
        .unwrap_or_default(),
      kept: FxDashMap::default(),
      keys: FxDashMap::default(),
      uncacheable_files: FxDashSet::default(),
      file_hashes: FxDashMap::default(),
      dir_entries: FxDashMap::default(),
      real_cwd: std::fs::canonicalize(&options.cwd).ok(),
      options: Arc::clone(options),
      stats: BuildCacheStats::default(),
    }))
  }

  fn cache_key(
    &self,
    stable_id: &str,
    asserted_module_type: Option<&ModuleType>,
    file_hash: u64,
  ) -> String {
    let asserted = asserted_module_type.map(ToString::to_string).unwrap_or_default();
    let mut input = Vec::with_capacity(self.salt.len() + stable_id.len() + asserted.len() + 16);
    input.extend_from_slice(self.salt.as_bytes());
    input.push(0);
    input.extend_from_slice(stable_id.as_bytes());
    input.push(0);
    input.extend_from_slice(asserted.as_bytes());
    input.push(0);
    input.extend_from_slice(&file_hash.to_le_bytes());
    xxhash_with_base(&input, 16)
  }

  /// The file a module id reads from: the id without its query, if that is an absolute path.
  pub fn file_of(id: &str) -> Option<&str> {
    let file = id.split_once('?').map_or(id, |(file, _)| file);
    (!file.starts_with('\0') && Path::new(file).is_absolute()).then_some(file)
  }

  /// The entry of `id`, if it is cached and every file it depends on is unchanged.
  pub fn lookup(
    &self,
    id: &ArcStr,
    stable_id: &str,
    asserted_module_type: Option<&ModuleType>,
  ) -> Lookup {
    let Some(file) = Self::file_of(id) else { return Lookup::Bypass };
    let Some(file_hash) = self.hash_file(file) else {
      self.uncacheable(file);
      return Lookup::Bypass;
    };
    let key = self.cache_key(stable_id, asserted_module_type, file_hash);
    self.keys.insert(id.clone(), key.clone());
    let module = self.find(id, &key);
    // LAB ONLY: `ROLLDOWN_BUILD_CACHE_DEBUG=<file>` appends every id that is not reused.
    if module.is_none()
      && let Some(path) = std::env::var_os("ROLLDOWN_BUILD_CACHE_DEBUG")
    {
      use std::io::Write;
      if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}\t{id}", self.root.display());
      }
    }
    match module {
      Some(module) => Lookup::Hit { key, module },
      None => Lookup::Miss { key },
    }
  }

  fn find(&self, id: &str, key: &str) -> Option<CachedModule> {
    if self.exclude.iter().any(|s| id.contains(s.as_str())) {
      self.stats.excluded.fetch_add(1, Ordering::Relaxed);
      return None;
    }
    // Any malformed or unreadable entry is a miss; the build then recomputes and rewrites it.
    let Some(module) = std::fs::read(self.entry_path(key)).ok().and_then(|b| self.decode(&b))
    else {
      self.stats.missing.fetch_add(1, Ordering::Relaxed);
      return None;
    };
    if module.files.iter().any(|(file, _)| self.invalidate.contains(file)) {
      self.stats.invalidated.fetch_add(1, Ordering::Relaxed);
      return None;
    }
    if module.files.iter().any(|(file, hash)| self.hash_file(file) != Some(*hash)) {
      self.stats.changed.fetch_add(1, Ordering::Relaxed);
      return None;
    }
    self.stats.hit.fetch_add(1, Ordering::Relaxed);
    Some(module)
  }

  /// The hash of a dependency: a file's content, a directory's sorted entry names, `MISSING` for a
  /// path that doesn't exist, `None` when it exists but can't be read.
  pub fn hash_file(&self, file: &str) -> Option<u64> {
    if let Some(hash) = self.file_hashes.get(file) {
      return *hash;
    }
    let entries = |dir: &str, prefix: &str| match self.dir_entries(dir).as_ref() {
      Ok(DirEntries { names, real }) => {
        let start = names.partition_point(|n| n.as_str() < prefix);
        let matching = names[start..].iter().take_while(|n| n.starts_with(prefix));
        let mut hasher = xxhash_rust::xxh3::Xxh3::with_seed(DIR_SEED);
        if let Some(real) = real {
          hasher.update(real.as_bytes());
          hasher.update(b"\0\0");
        }
        for name in matching {
          hasher.update(name.as_bytes());
          hasher.update(b"\0");
        }
        Some(hasher.digest())
      }
      // A file where a directory was expected has no entries either.
      Err(std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Some(MISSING),
      Err(_) => None,
    };
    let hash = if let Some(tsconfig) = file.strip_suffix(TSCONFIG_DEP) {
      self.tsconfig_hash(tsconfig)
    } else if let Some((dir, prefix)) = file.split_once('\0') {
      entries(dir, prefix)
    } else {
      match std::fs::read(file) {
        Ok(bytes) => Some(xxhash_rust::xxh3::xxh3_64(&bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(MISSING),
        Err(_) => entries(file, ""),
      }
    };
    self.file_hashes.insert(file.to_string(), hash);
    hash
  }

  fn dir_entries(&self, dir: &str) -> Arc<Result<DirEntries, std::io::ErrorKind>> {
    if let Some(entries) = self.dir_entries.get(dir) {
      return Arc::clone(entries.value());
    }
    let name = |entry: std::fs::DirEntry| -> std::io::Result<String> {
      let mut name = entry.file_name().to_string_lossy().into_owned();
      let file_type = entry.file_type()?;
      let path = entry.path();
      if file_type.is_dir() || (file_type.is_symlink() && path.is_dir()) {
        name.push('/');
      }
      if file_type.is_symlink() {
        name.push('\0');
        match (std::fs::canonicalize(&path), self.real_cwd.as_deref()) {
          (Ok(real), Some(real_cwd)) => name.push_str(&real.relative(real_cwd).to_string_lossy()),
          _ => name.push_str(&std::fs::read_link(&path)?.to_string_lossy()),
        }
      }
      Ok(name)
    };
    let entries = std::fs::read_dir(dir)
      .and_then(|entries| entries.map(|e| e.and_then(name)).collect::<Result<Vec<_>, _>>())
      .map(|mut names| {
        names.sort_unstable();
        DirEntries { names, real: self.real_dir(Path::new(dir)) }
      })
      .map_err(|err| err.kind());
    let entries = Arc::new(entries);
    self.dir_entries.insert(dir.to_string(), Arc::clone(&entries));
    entries
  }

  /// Where `dir` really is, relative to the real `cwd`, when a symlink on its path leads
  /// elsewhere than the path says; `None` when it doesn't.
  fn real_dir(&self, dir: &Path) -> Option<String> {
    let real = std::fs::canonicalize(dir).ok()?;
    let real_cwd = self.real_cwd.as_deref()?;
    let real_relative = real.strip_prefix(real_cwd).ok();
    if real_relative.is_some() && real_relative == dir.strip_prefix(&self.cwd).ok() {
      return None;
    }
    Some(real.relative(real_cwd).to_string_lossy().into_owned())
  }

  /// A dependency on the entries of `dir` whose names start with `prefix` (NUL can't be in a path).
  pub fn entries_dep(dir: &str, prefix: &str) -> String {
    format!("{dir}\0{prefix}")
  }

  /// The dependencies that decide which tsconfig governs `file` and what it resolves to, when
  /// Rolldown discovers tsconfig files: each directory from `file` up to the nearest one with a
  /// `tsconfig.json` (by that name, so one added closer is seen), that `tsconfig.json`, and
  /// `found`, the one discovery picked (a project reference can win over the nearest file).
  pub fn tsconfig_deps(&self, file: &Path, found: Option<&Path>) -> Vec<String> {
    let mut deps = vec![];
    if !self.options.transform_options.discovers_tsconfig()
      || file.components().any(|c| c.as_os_str() == "node_modules")
    {
      return deps;
    }
    let mut nearest = None;
    for dir in file.ancestors().skip(1) {
      let dir_str = dir.to_string_lossy();
      deps.push(Self::entries_dep(&dir_str, "tsconfig.json"));
      let has_tsconfig = self.dir_entries(&dir_str).as_ref().as_ref().is_ok_and(|entries| {
        entries.names.iter().any(|n| n == "tsconfig.json" || n.starts_with("tsconfig.json\0"))
      });
      if has_tsconfig {
        nearest = Some(dir.join("tsconfig.json"));
        break;
      }
    }
    for tsconfig in
      nearest.as_deref().into_iter().chain(found.filter(|f| Some(*f) != nearest.as_deref()))
    {
      deps.push(format!("{}{TSCONFIG_DEP}", tsconfig.to_string_lossy()));
    }
    deps
  }

  /// The hash of the config `tsconfig` resolves to, `extends` and references included; `None`
  /// when it can't be read. Its paths are absolute, so each directory from `cwd` up (deepest
  /// first, the file system root left out) is replaced by a marker of its depth: the hash stays
  /// the same for a checkout in another place.
  fn tsconfig_hash(&self, tsconfig: &str) -> Option<u64> {
    let mut resolved =
      self.options.transform_options.resolved_tsconfig_debug(Path::new(tsconfig))?;
    for (depth, dir) in self.cwd.ancestors().enumerate() {
      if dir.parent().is_none() {
        break;
      }
      let dir = dir.to_string_lossy();
      resolved = resolved
        .replace(&format!("{dir}/"), &format!("{PORTABLE_SOURCE}{depth}/"))
        .replace(&format!("{dir}\""), &format!("{PORTABLE_SOURCE}{depth}\""));
    }
    Some(xxhash_rust::xxh3::xxh3_64(resolved.as_bytes()))
  }

  /// Leaves every id of `file` out of the entries this build writes.
  pub fn uncacheable(&self, file: &str) {
    self.uncacheable_files.insert(file.to_string());
  }

  /// Keeps `module` for `id` under `key`. `unchanged`: a hit kept as it was read.
  pub fn keep(&self, id: ArcStr, key: &str, module: CachedModule, unchanged: bool) {
    self.stats.kept.fetch_add(1, Ordering::Relaxed);
    self.kept.insert(id, Kept { key: key.to_string(), module: Arc::new(module), unchanged });
  }

  /// Writes the entries of this build. A module's `meta` becomes the one the JS side handed over
  /// for its id (`""` for none); without one, it stays as it was. Each entry lists the
  /// dependencies of every id of its file. An entry that is already on disk as it would be
  /// written is not written again; entries of ids that turned out uncacheable are removed.
  ///
  /// Modules that are not cached (virtual ones, those that emitted files or depend on unreadable
  /// ones) have dependencies no entry lists, so the dependencies of every entry, the files this
  /// build watched (which include every module whose hooks ran) and the uncacheable files are
  /// also written to `<dir>/BUILD_DEPS` (rewritten only when it changes): a tool that reuses a
  /// whole build's output checks them. Returns how many entries were written.
  pub fn save(&self, plugin_driver: &PluginDriver) -> std::io::Result<usize> {
    let meta = &plugin_driver.module_meta_for_cache;
    // `!`: the module's `meta` can't be kept (not JSON), so neither can the module, nor the other
    // ids of its file, whose dependencies may be the ones it declared. The same goes for a module
    // whose hooks emitted a file.
    let not_kept = meta
      .iter()
      .filter(|m| m.value().as_str() == "!")
      .map(|m| m.key().clone())
      .chain(plugin_driver.uncacheable_modules.iter().map(|id| id.key().clone()));
    for id in not_kept {
      if let Some(file) = Self::file_of(&id) {
        self.uncacheable(file);
      }
    }
    let file_of_kept = |kept: &Kept| kept.module.files.first().map(|(file, _)| file.clone());
    // An import resolved to an external module with JS options: the external has no entry to bring
    // them back when the importer is reused, however it became external.
    for entry in &self.kept {
      let kept = entry.value();
      let external_with_options = kept
        .module
        .resolved_deps
        .iter()
        .any(|dep| dep.external != 0 && meta.get(dep.id.as_str()).is_some_and(|m| !m.is_empty()));
      if external_with_options && let Some(file) = file_of_kept(kept) {
        self.uncacheable(&file);
      }
    }
    // The kept entries grouped by the file they are keyed on (query siblings share one): every
    // entry of a file lists the dependencies of all of them.
    let kept: Vec<(ArcStr, String, Arc<CachedModule>, bool)> = self
      .kept
      .iter()
      .map(|e| {
        (e.key().clone(), e.value().key.clone(), Arc::clone(&e.value().module), e.value().unchanged)
      })
      .collect();
    let mut by_file: FxHashMap<&str, Vec<usize>> = FxHashMap::default();
    for (i, (_, _, module, _)) in kept.iter().enumerate() {
      if let Some((file, _)) = module.files.first() {
        by_file.entry(file.as_str()).or_default().push(i);
      }
    }
    // Files hooks watched while resolving a module's imports.
    let mut hook_deps: FxHashMap<&str, Vec<(String, u64)>> = FxHashMap::default();
    for entry in &plugin_driver.module_hook_dependencies {
      let Some(file) = Self::file_of(entry.key()) else { continue };
      let Some((&file, _)) = by_file.get_key_value(file) else { continue };
      for dep in entry.value().iter() {
        match self.hash_file(dep.key()) {
          Some(hash) => hook_deps.entry(file).or_default().push((dep.key().clone(), hash)),
          None => self.uncacheable(file),
        }
      }
    }
    let groups: Vec<(&str, &Vec<usize>)> = by_file.iter().map(|(f, ids)| (*f, ids)).collect();
    let per_file: Vec<SavedFile> = groups
      .par_iter()
      .map(|&(file, ids)| {
        let extra = hook_deps.get(file);
        // An entry stores its own file first (with the hash it read), then every other dependency
        // of the file's entries sorted, each once.
        let entries: Vec<&[(String, u64)]> =
          ids.iter().map(|&i| kept[i].2.files.as_slice()).collect();
        let rest = shared_rest(file, &entries, extra.map(Vec::as_slice));
        let uncacheable = self.uncacheable_files.contains(file);
        let saved = ids
          .iter()
          .map(|&i| {
            let (id, key, module, unchanged) = &kept[i];
            if uncacheable {
              return Saved::Remove(self.entry_path(key));
            }
            let next_meta = match meta.get(id) {
              Some(m) if m.is_empty() => None,
              Some(m) => Some(m.value().to_string()),
              None => module.meta.clone(),
            };
            let same_files = module.files.len() == rest.len() + 1 && module.files[1..] == rest[..];
            if *unchanged && same_files && module.meta == next_meta {
              return Saved::Keep;
            }
            let mut module = CachedModule::clone(module);
            module.files.truncate(1);
            module.files.extend(rest.iter().cloned());
            module.meta = next_meta;
            Saved::Write(self.entry_path(key), module)
          })
          .collect();
        (saved, rest)
      })
      .collect();
    let mut writes = vec![];
    let mut removes = vec![];
    let mut deps: Vec<&str> = vec![];
    for (&(file, _), (saved, rest)) in groups.iter().zip(&per_file) {
      deps.push(file);
      deps.extend(rest.iter().map(|(f, _)| f.as_str()));
      for s in saved {
        match s {
          Saved::Write(path, module) => writes.push((path, module)),
          Saved::Remove(path) => removes.push(path.clone()),
          Saved::Keep => {}
        }
      }
    }
    for entry in &self.keys {
      if Self::file_of(entry.key()).is_some_and(|f| self.uncacheable_files.contains(f))
        && !self.kept.contains_key(entry.key())
      {
        removes.push(self.entry_path(entry.value()));
      }
    }

    std::fs::create_dir_all(&self.root)?;
    let watch_files: Vec<String> =
      plugin_driver.watch_files.iter().map(|f| f.key().to_string()).collect();
    let uncacheable_files: Vec<String> =
      self.uncacheable_files.iter().map(|f| f.key().clone()).collect();
    deps.extend(watch_files.iter().map(String::as_str));
    deps.extend(uncacheable_files.iter().map(String::as_str));
    deps.sort_unstable();
    deps.dedup();
    let build_deps = self.root.join(BUILD_DEPS);
    let bytes = serde_json::to_vec(&deps)?;
    if std::fs::read(&build_deps).ok().as_deref() != Some(bytes.as_slice()) {
      write_atomically(&build_deps, &bytes)?;
    }
    for path in removes {
      let _ = std::fs::remove_file(path);
    }
    let written = writes.len();
    writes.into_par_iter().try_for_each(|(path, module)| {
      std::fs::create_dir_all(path.parent().expect("entry paths have a parent"))?;
      write_atomically(path, &self.encode(module)?)
    })?;
    Ok(written)
  }

  pub fn stats_line(&self) -> String {
    let s = &self.stats;
    format!(
      "hit {} missing {} changed {} invalidated {} excluded {} kept {}",
      s.hit.load(Ordering::Relaxed),
      s.missing.load(Ordering::Relaxed),
      s.changed.load(Ordering::Relaxed),
      s.invalidated.load(Ordering::Relaxed),
      s.excluded.load(Ordering::Relaxed),
      s.kept.load(Ordering::Relaxed),
    )
  }

  fn entry_path(&self, key: &str) -> PathBuf {
    self.dir.join(&key[0..2]).join(key)
  }

  /// `path` relative to `cwd` when absolute (`dir\0prefix` dependencies included).
  fn portable(&self, path: &str) -> String {
    match path.split_once('\0') {
      Some((dir, prefix)) => format!("{}\0{prefix}", self.portable(dir)),
      None if Path::new(path).is_absolute() => stabilize_id(path, &self.cwd),
      None => path.to_string(),
    }
  }

  fn local(&self, path: &str, absolute: bool) -> Option<String> {
    if let Some((dir, prefix)) = path.split_once('\0') {
      return Some(format!("{}\0{prefix}", self.local(dir, true)?));
    }
    if !absolute {
      return Some(path.to_string());
    }
    Some(path.as_path().absolutize_with(&self.cwd).to_str()?.to_string())
  }

  /// Entry layout: `MAGIC`, format version byte, `u64` LE metadata length, the
  /// metadata JSON, then the raw transformed code bytes. Keeping the code out of
  /// the JSON avoids escaping the biggest blob.
  fn encode(&self, module: &CachedModule) -> std::io::Result<Vec<u8>> {
    // Ids that aren't absolute paths (virtual, external bare) are kept as is, marked by a leading
    // `=`; a query stays as it is. A relative path that starts with `=` gets a `./` first.
    let id = |id: &str| {
      if !Path::new(id).is_absolute() {
        return format!("={id}");
      }
      let path = match id.split_once('?') {
        Some((file, query)) => format!("{}?{query}", self.portable(file)),
        None => self.portable(id),
      };
      if path.starts_with('=') { format!("./{path}") } else { path }
    };
    let meta = EntryMeta {
      files: module.files.iter().map(|(f, h)| (self.portable(f), *h)).collect(),
      module_type: module.module_type.clone(),
      side_effects: module.side_effects,
      sourcemap_chain: module
        .sourcemap_chain
        .iter()
        .map(|element| element.map_sources(|source| self.portable_source(source)))
        .collect(),
      resolved_deps: module
        .resolved_deps
        .iter()
        .map(|dep| CachedResolvedId {
          id: id(&dep.id),
          package_json: dep
            .package_json
            .clone()
            .map(|p| CachedPackageJson { realpath: self.portable(&p.realpath), ..p }),
          ..dep.clone()
        })
        .collect(),
      meta: module.meta.clone(),
    };
    let meta_bytes = serde_json::to_vec(&meta)?;
    let mut bytes = Vec::with_capacity(HEADER_LEN + meta_bytes.len() + module.code.len());
    bytes.extend_from_slice(&MAGIC);
    bytes.push(FORMAT_VERSION);
    bytes.extend_from_slice(&(meta_bytes.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&meta_bytes);
    bytes.extend_from_slice(module.code.as_bytes());
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    Ok(bytes)
  }

  /// A sourcemap source that is an absolute path, relative to `cwd` behind `PORTABLE_SOURCE`.
  fn portable_source(&self, source: &str) -> String {
    if Path::new(source).is_absolute() {
      format!("{PORTABLE_SOURCE}{}", self.portable(source))
    } else {
      source.to_string()
    }
  }

  fn local_source(&self, source: &str) -> String {
    match source.strip_prefix(PORTABLE_SOURCE) {
      Some(path) => self.local(path, true).unwrap_or_else(|| path.to_string()),
      None => source.to_string(),
    }
  }

  fn decode(&self, bytes: &[u8]) -> Option<CachedModule> {
    if bytes.len() < HEADER_LEN + CHECKSUM_LEN || bytes[0..4] != MAGIC || bytes[4] != FORMAT_VERSION
    {
      return None;
    }
    let (bytes, checksum) = bytes.split_at(bytes.len() - CHECKSUM_LEN);
    if xxhash_rust::xxh3::xxh3_64(bytes).to_le_bytes() != checksum {
      return None;
    }
    let meta_len = usize::try_from(u64::from_le_bytes(bytes[5..13].try_into().ok()?)).ok()?;
    let code_offset = HEADER_LEN.checked_add(meta_len)?;
    if bytes.len() < code_offset {
      return None;
    }
    let meta: EntryMeta = serde_json::from_slice(&bytes[HEADER_LEN..code_offset]).ok()?;
    let code = String::from_utf8(bytes[code_offset..].to_vec()).ok()?;
    let id = |id: &str| match (id.strip_prefix('='), id.split_once('?')) {
      (Some(id), _) => Some(id.to_string()),
      (None, Some((file, query))) => Some(format!("{}?{query}", self.local(file, true)?)),
      (None, None) => self.local(id, true),
    };
    let mut files = Vec::with_capacity(meta.files.len());
    for (file, hash) in meta.files {
      files.push((self.local(&file, true)?, hash));
    }
    if files.is_empty() {
      return None;
    }
    let mut resolved_deps = Vec::with_capacity(meta.resolved_deps.len());
    for dep in meta.resolved_deps {
      let package_json = match dep.package_json {
        Some(p) => Some(CachedPackageJson { realpath: self.local(&p.realpath, true)?, ..p }),
        None => None,
      };
      resolved_deps.push(CachedResolvedId { id: id(&dep.id)?, package_json, ..dep });
    }
    Some(CachedModule {
      files,
      code,
      module_type: meta.module_type,
      side_effects: meta.side_effects,
      sourcemap_chain: meta
        .sourcemap_chain
        .iter()
        .map(|element| element.map_sources(|source| self.local_source(source)))
        .collect(),
      resolved_deps,
      meta: meta.meta,
    })
  }
}

/// Writes go to a temp file of their own first (unique per process and write, created
/// exclusively) and are moved into place with a rename, so concurrent builds sharing a cache dir
/// never observe partial entries.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
  use std::io::Write;
  static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("entry");
  let write = WRITES.fetch_add(1, Ordering::Relaxed);
  let tmp = path.with_file_name(format!(".{file_name}.{}.{write}.tmp", std::process::id()));
  std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?.write_all(bytes)?;
  std::fs::rename(&tmp, path).inspect_err(|_| {
    let _ = std::fs::remove_file(&tmp);
  })
}

pub fn encode_hook_side_effects(value: HookSideEffects) -> u8 {
  match value {
    HookSideEffects::True => 0,
    HookSideEffects::False => 1,
    HookSideEffects::NoTreeshake => 2,
  }
}

pub fn decode_hook_side_effects(value: u8) -> HookSideEffects {
  match value {
    0 => HookSideEffects::True,
    1 => HookSideEffects::False,
    _ => HookSideEffects::NoTreeshake,
  }
}

impl CachedModule {
  pub fn module_type(&self) -> ModuleType {
    ModuleType::from_str_with_fallback(&self.module_type)
  }
}

impl CachedSourcemap {
  /// The element with each of its map's `sources` passed through `f`.
  fn map_sources(&self, f: impl Fn(&str) -> String) -> Self {
    let map = |json: &str| {
      let Ok(mut value) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
      };
      if let Some(sources) = value.get_mut("sources").and_then(|s| s.as_array_mut()) {
        for source in sources.iter_mut() {
          if let Some(path) = source.as_str() {
            *source = serde_json::Value::String(f(path));
          }
        }
      }
      value.to_string()
    };
    match self {
      Self::Transform { plugin_idx, map: json } => {
        Self::Transform { plugin_idx: *plugin_idx, map: map(json) }
      }
      Self::Load { map: json } => Self::Load { map: map(json) },
      other => other.clone(),
    }
  }

  pub fn encode(element: &SourcemapChainElement) -> Self {
    match element {
      SourcemapChainElement::Transform((plugin_idx, map)) => {
        Self::Transform { plugin_idx: plugin_idx.raw(), map: map.to_json_string() }
      }
      SourcemapChainElement::Load(map) => Self::Load { map: map.to_json_string() },
      SourcemapChainElement::Omitted { plugin_idx, plugin_name } => {
        Self::Omitted { plugin_idx: plugin_idx.raw(), plugin_name: plugin_name.to_string() }
      }
      SourcemapChainElement::Null { plugin_idx, original_content } => {
        Self::Null { plugin_idx: plugin_idx.raw(), original_content: original_content.to_string() }
      }
    }
  }

  pub fn decode(&self) -> Option<SourcemapChainElement> {
    use rolldown_common::PluginIdx;
    let map = |json: &str| {
      serde_json::from_str::<rolldown_sourcemap::JSONSourceMap>(json)
        .ok()
        .and_then(|json| SourceMap::from_json(json).ok())
    };
    Some(match self {
      Self::Transform { plugin_idx, map: json } => {
        SourcemapChainElement::Transform((PluginIdx::from_raw(*plugin_idx), map(json)?))
      }
      Self::Load { map: json } => SourcemapChainElement::Load(map(json)?),
      Self::Omitted { plugin_idx, plugin_name } => SourcemapChainElement::Omitted {
        plugin_idx: PluginIdx::from_raw(*plugin_idx),
        plugin_name: plugin_name.as_str().into(),
      },
      Self::Null { plugin_idx, original_content } => SourcemapChainElement::Null {
        plugin_idx: PluginIdx::from_raw(*plugin_idx),
        original_content: original_content.as_str().into(),
      },
    })
  }
}

impl CachedResolvedId {
  pub fn encode(resolved: &ResolvedId) -> Self {
    Self {
      id: resolved.id.to_string(),
      module_def_format: match resolved.module_def_format {
        ModuleDefFormat::Unknown => 0,
        ModuleDefFormat::Cjs => 1,
        ModuleDefFormat::Cts => 2,
        ModuleDefFormat::CjsPackageJson => 3,
        ModuleDefFormat::EsmMjs => 4,
        ModuleDefFormat::EsmMts => 5,
        ModuleDefFormat::EsmPackageJson => 6,
      },
      external: match resolved.external {
        ResolvedExternal::Bool(false) => 0,
        ResolvedExternal::Bool(true) => 1,
        ResolvedExternal::Absolute => 2,
        ResolvedExternal::Relative => 3,
      },
      normalize_external_id: resolved.normalize_external_id,
      package_json: resolved.package_json.as_deref().map(|p| CachedPackageJson {
        name: p.name().map(ToString::to_string),
        version: p.version().map(ToString::to_string),
        r#type: p.r#type().map(ToString::to_string),
        side_effects: p.side_effects.as_ref().map(|s| match s {
          rolldown_common::side_effects::SideEffects::Bool(v) => CachedPackageSideEffects::Bool(*v),
          rolldown_common::side_effects::SideEffects::String(v) => {
            CachedPackageSideEffects::String(v.clone())
          }
          rolldown_common::side_effects::SideEffects::Array(v) => {
            CachedPackageSideEffects::Array(v.clone())
          }
        }),
        realpath: p.realpath().to_string_lossy().into_owned(),
      }),
      side_effects: resolved.side_effects.map(encode_hook_side_effects),
      is_external_without_side_effects: resolved.is_external_without_side_effects,
    }
  }

  pub fn decode(&self) -> ResolvedId {
    ResolvedId {
      id: ModuleId::new(self.id.as_str()),
      module_def_format: match self.module_def_format {
        1 => ModuleDefFormat::Cjs,
        2 => ModuleDefFormat::Cts,
        3 => ModuleDefFormat::CjsPackageJson,
        4 => ModuleDefFormat::EsmMjs,
        5 => ModuleDefFormat::EsmMts,
        6 => ModuleDefFormat::EsmPackageJson,
        _ => ModuleDefFormat::Unknown,
      },
      external: match self.external {
        1 => ResolvedExternal::Bool(true),
        2 => ResolvedExternal::Absolute,
        3 => ResolvedExternal::Relative,
        _ => ResolvedExternal::Bool(false),
      },
      normalize_external_id: self.normalize_external_id,
      package_json: self.package_json.as_ref().map(|p| {
        Arc::new(PackageJson::from_parts(
          p.name.as_deref().map(Into::into),
          p.version.as_deref().map(Into::into),
          match p.r#type.as_deref() {
            Some("commonjs") => Some("commonjs"),
            Some("module") => Some("module"),
            _ => None,
          },
          p.side_effects.as_ref().map(|s| match s {
            CachedPackageSideEffects::Bool(v) => {
              rolldown_common::side_effects::SideEffects::Bool(*v)
            }
            CachedPackageSideEffects::String(v) => {
              rolldown_common::side_effects::SideEffects::String(v.clone())
            }
            CachedPackageSideEffects::Array(v) => {
              rolldown_common::side_effects::SideEffects::Array(v.clone())
            }
          }),
          PathBuf::from(&p.realpath),
        ))
      }),
      side_effects: self.side_effects.map(decode_hook_side_effects),
      is_external_without_side_effects: self.is_external_without_side_effects,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  type Files = Vec<(String, u64)>;

  /// `files` of each entry of one file as `save` wrote them before entries were grouped.
  fn files_before_grouping(
    file: &str,
    entries: &[Vec<(String, u64)>],
    extra: &[(String, u64)],
  ) -> Vec<Vec<(String, u64)>> {
    let mut deps: FxHashSet<(String, u64)> = FxHashSet::default();
    for files in entries {
      deps.extend(files.iter().cloned());
    }
    deps.extend(extra.iter().cloned());
    entries
      .iter()
      .map(|files| {
        let mut rest: Vec<_> = deps.iter().filter(|dep| dep.0 != file).cloned().collect();
        rest.sort_unstable();
        std::iter::once(files[0].clone()).chain(rest).collect()
      })
      .collect()
  }

  #[test]
  fn grouped_save_lists_what_it_listed_before() {
    let d = |p: &str, h: u64| (p.to_string(), h);
    let f = "/a/X.vue";
    let cases: Vec<(Vec<Files>, Files)> = vec![
      // One entry, already in stored order.
      (vec![vec![d(f, 1), d("/a/b.ts", 2), d("/a/c.ts", 3)]], vec![]),
      // One entry out of order, with a duplicate and its own file again.
      (vec![vec![d(f, 1), d("/a/c.ts", 3), d("/a/b.ts", 2), d("/a/b.ts", 2), d(f, 1)]], vec![]),
      // Query siblings with different dependencies and a different hash of the file itself.
      (
        vec![
          vec![d(f, 11), d("/a/b.ts", 2)],
          vec![d(f, 22), d("/a/style.css", 4), d("/a/b.ts", 2)],
        ],
        vec![],
      ),
      // Hook dependencies, one of them already listed.
      (vec![vec![d(f, 1), d("/a/b.ts", 2)]], vec![d("/a/dir\0pre", 5), d("/a/b.ts", 2)]),
      // A dependency on the same path with another hash.
      (vec![vec![d(f, 1), d("/a/b.ts", 2)], vec![d(f, 1), d("/a/b.ts", 9)]], vec![]),
    ];
    for (entries, extra) in cases {
      let slices: Vec<&[(String, u64)]> = entries.iter().map(Vec::as_slice).collect();
      let rest = shared_rest(f, &slices, (!extra.is_empty()).then_some(extra.as_slice()));
      let now: Vec<Vec<(String, u64)>> = entries
        .iter()
        .map(|files| std::iter::once(files[0].clone()).chain(rest.iter().cloned()).collect())
        .collect();
      assert_eq!(now, files_before_grouping(f, &entries, &extra), "{entries:?} {extra:?}");
    }
  }

  fn cache(cwd: &Path) -> BuildCache {
    BuildCache {
      root: cwd.join("cache"),
      dir: cwd.join("cache/build-v1"),
      salt: "salt".into(),
      cwd: cwd.to_path_buf(),
      invalidate: FxHashSet::default(),
      exclude: vec![],
      kept: FxDashMap::default(),
      keys: FxDashMap::default(),
      uncacheable_files: FxDashSet::default(),
      file_hashes: FxDashMap::default(),
      dir_entries: FxDashMap::default(),
      options: Arc::new(rolldown_common::NormalizedBundlerOptions {
        cwd: cwd.to_path_buf(),
        ..Default::default()
      }),
      real_cwd: None,
      stats: BuildCacheStats::default(),
    }
  }

  #[test]
  fn entry_roundtrip_keeps_paths_portable() {
    let cwd = std::env::current_dir().unwrap();
    let file = cwd.join("src/a.js").to_string_lossy().into_owned();
    let dir_dep = BuildCache::entries_dep(&cwd.join("src").to_string_lossy(), "dep");
    let module = CachedModule {
      files: vec![(file, 1), (dir_dep, 2)],
      code: "const a = 2;".into(),
      module_type: "tsx".into(),
      side_effects: Some(1),
      sourcemap_chain: vec![CachedSourcemap::Omitted { plugin_idx: 3, plugin_name: "p".into() }],
      resolved_deps: vec![
        CachedResolvedId {
          id: cwd.join("src/dep.js").to_string_lossy().into_owned(),
          module_def_format: 4,
          external: 0,
          normalize_external_id: None,
          package_json: Some(CachedPackageJson {
            name: Some("pkg".into()),
            version: None,
            r#type: Some("module".into()),
            side_effects: Some(CachedPackageSideEffects::Array(vec!["*.css".into()])),
            realpath: cwd.join("package.json").to_string_lossy().into_owned(),
          }),
          side_effects: None,
          is_external_without_side_effects: false,
        },
        CachedResolvedId {
          id: "react".into(),
          module_def_format: 0,
          external: 1,
          normalize_external_id: None,
          package_json: None,
          side_effects: None,
          is_external_without_side_effects: true,
        },
      ],
      meta: Some("{\"a\":1}".into()),
    };
    let bytes = cache(&cwd).encode(&module).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains(cwd.to_str().unwrap()), "entries hold no absolute paths");
    let other = cwd.join("elsewhere");
    let moved = cache(&other).decode(&bytes).unwrap();
    assert_eq!(moved.files[0].0, other.join("src/a.js").to_string_lossy());
    assert_eq!(moved.resolved_deps[1].id, "react");
    assert_eq!(cache(&cwd).decode(&bytes).unwrap(), module);
  }

  #[test]
  fn rejects_unknown_version_and_garbage() {
    let cwd = std::env::current_dir().unwrap();
    let module = CachedModule {
      files: vec![(cwd.join("a.js").to_string_lossy().into_owned(), 1)],
      code: "code".into(),
      module_type: "js".into(),
      side_effects: None,
      sourcemap_chain: vec![],
      resolved_deps: vec![],
      meta: None,
    };
    let c = cache(&cwd);
    let bytes = c.encode(&module).unwrap();
    let mut wrong_version = bytes.clone();
    wrong_version[4] = FORMAT_VERSION + 1;
    assert!(c.decode(&wrong_version).is_none());
    assert!(c.decode(&bytes[0..HEADER_LEN - 1]).is_none());
    assert!(c.decode(b"not a cache entry").is_none());
    // A write cut after the code, or one whose bytes changed, is a miss.
    assert!(c.decode(&bytes[0..bytes.len() - 1]).is_none());
    let mut flipped = bytes.clone();
    let code_byte = bytes.len() - CHECKSUM_LEN - 1;
    flipped[code_byte] ^= 1;
    assert!(c.decode(&flipped).is_none());
    assert_eq!(c.decode(&bytes).unwrap(), module);
  }

  #[test]
  fn ids_and_sourcemap_sources_stay_portable() {
    let cwd = std::env::current_dir().unwrap();
    let source = cwd.join("src/a.ts").to_string_lossy().into_owned();
    let map =
      format!(r#"{{"version":3,"sources":[{source:?},"rel.ts"],"names":[],"mappings":""}}"#);
    let module = CachedModule {
      files: vec![(cwd.join("src/a.ts").to_string_lossy().into_owned(), 1)],
      code: "code".into(),
      module_type: "js".into(),
      side_effects: None,
      sourcemap_chain: vec![CachedSourcemap::Load { map }],
      resolved_deps: vec![CachedResolvedId {
        id: cwd.join("=dep.js").to_string_lossy().into_owned(),
        module_def_format: 0,
        external: 0,
        normalize_external_id: None,
        package_json: None,
        side_effects: None,
        is_external_without_side_effects: false,
      }],
      meta: None,
    };
    let bytes = cache(&cwd).encode(&module).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains(cwd.to_str().unwrap()));
    let other = cwd.join("elsewhere");
    let moved = cache(&other).decode(&bytes).unwrap();
    assert_eq!(moved.resolved_deps[0].id, other.join("=dep.js").to_string_lossy());
    let CachedSourcemap::Load { map } = &moved.sourcemap_chain[0] else { unreachable!() };
    let map: serde_json::Value = serde_json::from_str(map).unwrap();
    assert_eq!(map["sources"][0], other.join("src/a.ts").to_string_lossy().as_ref());
    assert_eq!(map["sources"][1], "rel.ts");
  }
}
