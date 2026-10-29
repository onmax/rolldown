//! On-disk records of the scan stage, so a build in a new process can skip the `load`,
//! `transform` and `resolveId` hooks of modules whose inputs did not change.
//!
//! A record holds what the plugin hooks produced for one module: its code after `transform`,
//! the module type, hook side effects, the sourcemap chain and the resolved ids of its imports.
//! The scan re-parses the recorded code in Rust, so the `ScanStageCache` snapshot it builds is the
//! one a full scan would build. Like `experimental.incrementalBuild`, this assumes `load`,
//! `transform` and `resolveId` have no side effects.
//!
//! A record is reused when the file behind the module id (without its query) and every file the
//! hooks depended on (`this.addWatchFile` in `transform`, the module's tsconfig) hash the same as
//! when it was recorded, and the caller did not list the file in `invalidate`. Records of virtual
//! modules, of binary modules and of builds with native magic string are not kept.
//! See internal-docs/cache/implementation.md ("Module record store").

use std::{
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
  },
};

use arcstr::ArcStr;
use rolldown_common::{
  IncrementalBuildCacheOptions, ModuleDefFormat, ModuleId, ModuleType, PackageJson,
  ResolvedExternal, ResolvedId, SourcemapChainElement, side_effects::HookSideEffects,
};
use rolldown_sourcemap::SourceMap;
use rolldown_utils::{
  dashmap::FxDashMap,
  rayon::{IntoParallelIterator, ParallelIterator},
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

const FORMAT_VERSION: u32 = 2;
/// Records are split by id into shards, so a build that changed a few modules rewrites a few
/// shards instead of every record.
const SHARDS: usize = 32;

fn shard_of(id: &str) -> usize {
  (xxhash_rust::xxh3::xxh3_64(id.as_bytes()) % SHARDS as u64) as usize
}

fn shard_file(dir: &Path, shard: usize) -> PathBuf {
  dir.join(format!("module-records-{shard:02}.json"))
}

#[derive(Serialize, Deserialize)]
struct RecordFile {
  version: u32,
  rolldown_version: String,
  key: Option<String>,
  records: Vec<ModuleRecord>,
}

#[derive(Serialize)]
struct RecordFileRef<'a> {
  version: u32,
  rolldown_version: &'a str,
  key: Option<&'a str>,
  records: Vec<&'a ModuleRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleRecord {
  pub id: String,
  /// Files this record depends on, with the hash of their content when it was recorded.
  /// The first entry is the file behind the module id.
  pub files: Vec<(String, u64)>,
  pub code: String,
  pub module_type: String,
  pub hook_side_effects: Option<u8>,
  pub sourcemap_chain: Vec<RecordedSourcemap>,
  pub resolved_deps: Vec<RecordedResolvedId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RecordedSourcemap {
  Transform { plugin_idx: u32, map: String },
  Load { map: String },
  Omitted { plugin_idx: u32, plugin_name: String },
  Null { plugin_idx: u32, original_content: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedResolvedId {
  pub id: String,
  pub module_def_format: u8,
  pub external: u8,
  pub normalize_external_id: Option<bool>,
  pub package_json: Option<RecordedPackageJson>,
  pub side_effects: Option<u8>,
  pub is_external_without_side_effects: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedPackageJson {
  pub name: Option<String>,
  pub version: Option<String>,
  pub r#type: Option<String>,
  pub side_effects: Option<RecordedPackageSideEffects>,
  pub realpath: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RecordedPackageSideEffects {
  Bool(bool),
  String(String),
  Array(Vec<String>),
}

#[derive(Debug, Default)]
pub struct ModuleRecordStats {
  pub reused: AtomicU32,
  pub missing: AtomicU32,
  pub changed: AtomicU32,
  pub invalidated: AtomicU32,
  pub excluded: AtomicU32,
  pub recorded: AtomicU32,
}

#[derive(Debug)]
pub struct ModuleRecordStore {
  dir: PathBuf,
  key: Option<String>,
  previous: FxHashMap<ArcStr, ModuleRecord>,
  /// For each file behind a module id, the files any of its ids depended on. Ids of one file
  /// that differ by query (`X.vue?vue&type=script`) are made from state a plugin built while
  /// processing another of them, so a dependency of one counts for all.
  deps_by_file: FxHashMap<String, Vec<(String, u64)>>,
  previous_per_shard: [usize; SHARDS],
  /// Shards that got a record this build did not reuse unchanged.
  dirty: [AtomicBool; SHARDS],
  invalidate: FxHashSet<String>,
  // LAB ONLY: `ROLLDOWN_INCREMENTAL_CACHE_EXCLUDE`, comma-separated id substrings never reused.
  // Stands in for a plugin-declared opt-out until `shouldTransformCachedModule` exists.
  exclude: Vec<String>,
  next: FxDashMap<ArcStr, Arc<ModuleRecord>>,
  file_hashes: FxDashMap<String, Option<u64>>,
  pub stats: ModuleRecordStats,
}

pub type SharedModuleRecordStore = Arc<ModuleRecordStore>;

impl ModuleRecordStore {
  /// Loads the records in `options.dir`. Records written under another key, format or Rolldown
  /// version are dropped.
  pub fn load(options: &IncrementalBuildCacheOptions) -> Self {
    let dir = PathBuf::from(&options.dir);
    let shards: Vec<Vec<ModuleRecord>> = (0..SHARDS)
      .into_par_iter()
      .map(|shard| {
        std::fs::read(shard_file(&dir, shard))
          .ok()
          .and_then(|bytes| serde_json::from_slice::<RecordFile>(&bytes).ok())
          .filter(|file| {
            file.version == FORMAT_VERSION
              && file.rolldown_version == env!("CARGO_PKG_VERSION")
              && file.key == options.key
          })
          .map(|file| file.records)
          .unwrap_or_default()
      })
      .collect();
    let mut previous_per_shard = [0; SHARDS];
    for (shard, records) in shards.iter().enumerate() {
      previous_per_shard[shard] = records.len();
    }
    let previous: FxHashMap<ArcStr, ModuleRecord> =
      shards.into_iter().flatten().map(|r| (ArcStr::from(r.id.as_str()), r)).collect();
    let mut deps_by_file: FxHashMap<String, Vec<(String, u64)>> = FxHashMap::default();
    for record in previous.values() {
      let Some((file, _)) = record.files.first() else { continue };
      let deps = deps_by_file.entry(file.clone()).or_default();
      for dep in &record.files {
        if !deps.contains(dep) {
          deps.push(dep.clone());
        }
      }
    }
    Self {
      dir,
      key: options.key.clone(),
      previous,
      deps_by_file,
      previous_per_shard,
      dirty: std::array::from_fn(|_| AtomicBool::new(false)),
      invalidate: options.invalidate.iter().flatten().cloned().collect(),
      exclude: std::env::var("ROLLDOWN_INCREMENTAL_CACHE_EXCLUDE")
        .map(|v| v.split(',').filter(|s| !s.is_empty()).map(ToString::to_string).collect())
        .unwrap_or_default(),
      next: FxDashMap::default(),
      file_hashes: FxDashMap::default(),
      stats: ModuleRecordStats::default(),
    }
  }

  /// The file a module id reads from: the id without its query, if that is an absolute path.
  pub fn file_of(id: &str) -> Option<&str> {
    let file = id.split_once('?').map_or(id, |(file, _)| file);
    (!file.starts_with('\0') && Path::new(file).is_absolute()).then_some(file)
  }

  pub fn hash_file(&self, file: &str) -> Option<u64> {
    if let Some(hash) = self.file_hashes.get(file) {
      return *hash;
    }
    let hash = std::fs::read(file).ok().map(|bytes| xxhash_rust::xxh3::xxh3_64(&bytes));
    self.file_hashes.insert(file.to_string(), hash);
    hash
  }

  /// The record of `id` when all the files it depends on are unchanged.
  pub fn reusable(&self, id: &str) -> Option<&ModuleRecord> {
    let found = self.reusable_inner(id);
    // LAB ONLY: `ROLLDOWN_RECORDS_DEBUG=<file>` appends every id that is not reused.
    if found.is_none() {
      if let Some(path) = std::env::var_os("ROLLDOWN_RECORDS_DEBUG") {
        use std::io::Write;
        let reason = match self.previous.get(id) {
          None => "missing",
          Some(_) => "changed-or-invalidated",
        };
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
          let _ = writeln!(f, "{}\t{reason}\t{id}", self.dir.display());
        }
      }
    }
    found
  }

  fn reusable_inner(&self, id: &str) -> Option<&ModuleRecord> {
    if self.exclude.iter().any(|s| id.contains(s.as_str())) {
      self.stats.excluded.fetch_add(1, Ordering::Relaxed);
      return None;
    }
    let Some(record) = self.previous.get(id) else {
      self.stats.missing.fetch_add(1, Ordering::Relaxed);
      return None;
    };
    let files = record
      .files
      .first()
      .and_then(|(file, _)| self.deps_by_file.get(file))
      .unwrap_or(&record.files);
    if files.iter().any(|(file, _)| self.invalidate.contains(file)) {
      self.stats.invalidated.fetch_add(1, Ordering::Relaxed);
      return None;
    }
    if files.iter().any(|(file, hash)| self.hash_file(file).unwrap_or(0) != *hash) {
      self.stats.changed.fetch_add(1, Ordering::Relaxed);
      return None;
    }
    self.stats.reused.fetch_add(1, Ordering::Relaxed);
    Some(record)
  }

  /// Keeps `record` for the next build. `unchanged` says it is a previous record reused as is.
  pub fn keep(&self, record: ModuleRecord, unchanged: bool) {
    self.stats.recorded.fetch_add(1, Ordering::Relaxed);
    if !unchanged {
      self.dirty[shard_of(&record.id)].store(true, Ordering::Relaxed);
    }
    self.next.insert(ArcStr::from(record.id.as_str()), Arc::new(record));
  }

  /// Writes the records of this build, replacing the previous ones. A shard whose records are
  /// the previous ones, unchanged, is not written again.
  pub fn save(&self) -> std::io::Result<usize> {
    let mut shards: Vec<Vec<Arc<ModuleRecord>>> = vec![Vec::new(); SHARDS];
    for entry in &self.next {
      shards[shard_of(entry.key())].push(Arc::clone(entry.value()));
    }
    std::fs::create_dir_all(&self.dir)?;
    let written = shards
      .into_iter()
      .enumerate()
      .collect::<Vec<_>>()
      .into_par_iter()
      .filter(|(shard, records)| {
        self.dirty[*shard].load(Ordering::Relaxed) || records.len() != self.previous_per_shard[*shard]
      })
      .map(|(shard, mut records)| {
        records.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        let file = RecordFileRef {
          version: FORMAT_VERSION,
          rolldown_version: env!("CARGO_PKG_VERSION"),
          key: self.key.as_deref(),
          records: records.iter().map(AsRef::as_ref).collect(),
        };
        let path = shard_file(&self.dir, shard);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&file)?)?;
        std::fs::rename(tmp, path)
      })
      .collect::<std::io::Result<Vec<()>>>()?;
    Ok(written.len())
  }

  pub fn stats_line(&self) -> String {
    let s = &self.stats;
    format!(
      "reused {} missing {} changed {} invalidated {} excluded {} recorded {}",
      s.reused.load(Ordering::Relaxed),
      s.missing.load(Ordering::Relaxed),
      s.changed.load(Ordering::Relaxed),
      s.invalidated.load(Ordering::Relaxed),
      s.excluded.load(Ordering::Relaxed),
      s.recorded.load(Ordering::Relaxed),
    )
  }
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

impl RecordedSourcemap {
  pub fn encode(element: &SourcemapChainElement) -> Self {
    match element {
      SourcemapChainElement::Transform((plugin_idx, map)) => {
        Self::Transform { plugin_idx: plugin_idx.raw(), map: map.to_json_string() }
      }
      SourcemapChainElement::Load(map) => Self::Load { map: map.to_json_string() },
      SourcemapChainElement::Omitted { plugin_idx, plugin_name } => {
        Self::Omitted { plugin_idx: plugin_idx.raw(), plugin_name: plugin_name.to_string() }
      }
      SourcemapChainElement::Null { plugin_idx, original_content } => Self::Null {
        plugin_idx: plugin_idx.raw(),
        original_content: original_content.to_string(),
      },
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

impl RecordedResolvedId {
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
      package_json: resolved.package_json.as_deref().map(|p| RecordedPackageJson {
        name: p.name().map(ToString::to_string),
        version: p.version().map(ToString::to_string),
        r#type: p.r#type().map(ToString::to_string),
        side_effects: p.side_effects.as_ref().map(|s| match s {
          rolldown_common::side_effects::SideEffects::Bool(v) => {
            RecordedPackageSideEffects::Bool(*v)
          }
          rolldown_common::side_effects::SideEffects::String(v) => {
            RecordedPackageSideEffects::String(v.clone())
          }
          rolldown_common::side_effects::SideEffects::Array(v) => {
            RecordedPackageSideEffects::Array(v.clone())
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
        Arc::new(PackageJson::new(
          p.name.as_deref().map(Into::into),
          p.version.as_deref().map(Into::into),
          match p.r#type.as_deref() {
            Some("commonjs") => Some("commonjs"),
            Some("module") => Some("module"),
            _ => None,
          },
          p.side_effects.as_ref().map(|s| match s {
            RecordedPackageSideEffects::Bool(v) => {
              rolldown_common::side_effects::SideEffects::Bool(*v)
            }
            RecordedPackageSideEffects::String(v) => {
              rolldown_common::side_effects::SideEffects::String(v.clone())
            }
            RecordedPackageSideEffects::Array(v) => {
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

impl ModuleRecord {
  pub fn module_type(&self) -> ModuleType {
    ModuleType::from_str_with_fallback(&self.module_type)
  }
}
