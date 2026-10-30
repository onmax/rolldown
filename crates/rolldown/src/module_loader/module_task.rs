use std::sync::Arc;

use arcstr::ArcStr;
use oxc::span::Span;

use rolldown_common::{
  ExportsKind, FlatOptions, ImportKind, ModuleIdx, ModuleInfo, ModuleLoaderMsg, ModuleType,
  NormalModule, NormalModuleTaskResult, ResolvedId, SourceMapGenMsg, SourcemapChainElement,
  StrOrBytes, try_extract_lazy_barrel_info,
};
use rolldown_error::{
  BuildDiagnostic, BuildResult, DiagnosticOptions, EventKindSwitcher, UnloadableDependencyContext,
  downcast_napi_error_diagnostics,
};
use rolldown_utils::{ecmascript::legitimize_identifier_name, indexmap::FxIndexSet};

use rolldown_fs::FileSystem;

use crate::{
  ecmascript::ecma_module_view_factory::{CreateEcmaViewReturn, create_ecma_view},
  types::module_factory::{CreateModuleContext, CreateModuleViewArgs},
  utils::{load_source::load_source, transform_source::transform_source},
};

use super::{resolve_utils::resolve_dependencies, task_context::TaskContext};
use crate::utils::build_cache::{
  BuildCache, CachedModule, CachedResolvedId, CachedSourcemap, Lookup, decode_hook_side_effects,
  encode_hook_side_effects,
};
use oxc_index::IndexVec;

pub struct ModuleTaskOwner {
  source: ArcStr,
  importer_id: ArcStr,
  importee_span: Span,
}

impl ModuleTaskOwner {
  pub fn new(normal_module: &NormalModule, importee_span: Span) -> Self {
    Self {
      source: normal_module.source.clone(),
      importer_id: normal_module.stable_id.as_arc_str().clone(),
      importee_span,
    }
  }
}

pub struct ModuleTask<Fs: FileSystem + Clone + 'static> {
  ctx: Arc<TaskContext<Fs>>,
  module_idx: ModuleIdx,
  resolved_id: ResolvedId,
  owner: Option<ModuleTaskOwner>,
  is_user_defined_entry: bool,
  /// The module is asserted to be this specific module type.
  asserted_module_type: Option<ModuleType>,
  flat_options: FlatOptions,
  magic_string_tx: Option<std::sync::mpsc::Sender<SourceMapGenMsg>>,
}

impl<Fs: FileSystem + Clone + 'static> ModuleTask<Fs> {
  #[expect(clippy::too_many_arguments)]
  pub fn new(
    ctx: Arc<TaskContext<Fs>>,
    idx: ModuleIdx,
    resolved_id: ResolvedId,
    owner: Option<ModuleTaskOwner>,
    is_user_defined_entry: bool,
    assert_module_type: Option<ModuleType>,
    flat_options: FlatOptions,
    magic_string_tx: Option<std::sync::mpsc::Sender<SourceMapGenMsg>>,
  ) -> Self {
    Self {
      ctx,
      module_idx: idx,
      resolved_id,
      owner,
      is_user_defined_entry,
      asserted_module_type: assert_module_type,
      flat_options,
      magic_string_tx,
    }
  }

  #[tracing::instrument(name="NormalModuleTask::run", level = "trace", skip_all, fields(module_id = %self.resolved_id.id))]
  pub async fn run(mut self) {
    // `experimental.buildCache`: what hooks emit or watch while this task runs belongs to the module.
    let result = if self.ctx.build_cache.is_some() {
      let id = self.resolved_id.id.as_arc_str().clone();
      rolldown_plugin::HOOK_MODULE.scope(id, self.run_inner()).await
    } else {
      self.run_inner().await
    };
    if let Err(errs) = result {
      self.ctx.plugin_driver.mark_context_load_modules_loaded(self.resolved_id.id.clone());
      self
        .ctx
        .tx
        .send(ModuleLoaderMsg::BuildErrors(errs.into_vec().into_boxed_slice()))
        .expect("ModuleLoader: failed to send build errors - main thread terminated while processing module errors");
    }
  }

  #[expect(clippy::too_many_lines)] // The build cache's lookup and keep steps wrap the pipeline stages inline.
  async fn run_inner(&mut self) -> BuildResult<()> {
    let id = self.resolved_id.id.clone();

    self.ctx.plugin_driver.set_module_info(
      &id,
      Arc::new(ModuleInfo {
        code: None,
        id: id.clone(),
        is_entry: self.is_user_defined_entry,
        importers: FxIndexSet::default(),
        dynamic_importers: FxIndexSet::default(),
        imported_ids: FxIndexSet::default(),
        dynamically_imported_ids: FxIndexSet::default(),
        exports: vec![],
        input_format: ExportsKind::None,
      }),
    );

    let mut sourcemap_chain = vec![];
    let mut hook_side_effects = self.resolved_id.side_effects.take();
    let side_effects_from_resolve = hook_side_effects;
    let stable_id = id.stabilize(&self.ctx.options.cwd);

    // Persistent build cache: a hit replays the plugin `load`/`transform` pipeline results plus
    // the resolved dependencies without calling any hook or the resolver; parsing and scanning
    // re-run natively on the cached code below. See internal-docs/build-cache/implementation.md.
    let build_cache = self.ctx.build_cache.as_ref();
    let lookup = build_cache.map_or(Lookup::Bypass, |cache| {
      cache.lookup(id.as_arc_str(), stable_id.as_arc_str(), self.asserted_module_type.as_ref())
    });
    let (key, hit) = match lookup {
      Lookup::Bypass => (None, None),
      Lookup::Miss { key } => (Some(key), None),
      Lookup::Hit { key, module } => {
        let chain =
          module.sourcemap_chain.iter().map(CachedSourcemap::decode).collect::<Option<Vec<_>>>();
        (Some(key), chain.map(|chain| (module, chain)))
      }
    };
    if build_cache.is_some() {
      self.ctx.plugin_driver.reused_module_meta.insert(
        id.as_arc_str().clone(),
        hit.as_ref().map_or_else(ArcStr::new, |(module, _)| {
          module.meta.as_deref().map_or_else(|| arcstr::literal!("{}"), ArcStr::from)
        }),
      );
    }

    let (source, module_type) = if let Some((module, chain)) = &hit {
      sourcemap_chain.clone_from(chain);
      if let Some(side_effects) = module.side_effects {
        hook_side_effects = Some(decode_hook_side_effects(side_effects));
      }
      // Watch registration mirrors the read-from-disk path in `load_source`.
      self.ctx.plugin_driver.watch_files.insert(id.as_arc_str().clone());
      (StrOrBytes::Str(module.code.clone()), module.module_type())
    } else {
      self
        .load_source(&mut sourcemap_chain, &mut hook_side_effects, self.magic_string_tx.clone())
        .await?
    };
    // Only string sources are cacheable; the pipeline never transforms bytes.
    let new_entry = match (&hit, build_cache, &key, &source) {
      (None, Some(cache), Some(_), StrOrBytes::Str(code)) => {
        // Side effects may already be set by resolution data outside the entry, so only the
        // load/transform pipeline's own override is kept; keeping the pre-existing value would
        // replay stale resolution data on later hits.
        let side_effects_override =
          if hook_side_effects == side_effects_from_resolve { None } else { hook_side_effects };
        self.start_entry(cache, code, &module_type, side_effects_override, &sourcemap_chain)
      }
      _ => None,
    };

    if matches!(module_type, ModuleType::Css) {
      Err(BuildDiagnostic::unsupported_feature(
        match &source { StrOrBytes::Bytes(_) => ArcStr::new(), StrOrBytes::Str(s) => s.into() },
        id.as_arc_str().clone(),
        Span::empty(0),
        "Bundling CSS is no longer supported (experimental support has been removed). See https://github.com/rolldown/rolldown/issues/4271 for details.".to_string())
      )?;
    }

    let mut warnings = vec![];

    let CreateEcmaViewReturn { mut ecma_view, ecma_related, raw_import_records, tla_keyword_span } =
      create_ecma_view(
        &mut CreateModuleContext {
          stable_id: &stable_id,
          module_idx: self.module_idx,
          plugin_driver: &self.ctx.plugin_driver,
          resolved_id: &self.resolved_id,
          options: &self.ctx.options,
          warnings: &mut warnings,
          module_type: module_type.clone(),
          replace_global_define_config: self.ctx.meta.replace_global_define_config.clone(),
          is_user_defined_entry: self.is_user_defined_entry,
          flat_options: self.flat_options,
        },
        CreateModuleViewArgs { source, sourcemap_chain, hook_side_effects },
      )
      .await?;

    // Scanning the cached code deterministically reproduces the import records the cached
    // resolutions are positionally aligned with; a length mismatch means a corrupt entry, so fall
    // back to a fresh resolution.
    let cached_deps = hit.as_ref().and_then(|(module, _)| {
      (module.resolved_deps.len() == raw_import_records.len()).then(|| {
        module.resolved_deps.iter().map(CachedResolvedId::decode).collect::<IndexVec<_, _>>()
      })
    });
    let hit_deps = cached_deps.is_some();
    let mut resolution_warned = false;
    let resolved_deps = if let Some(resolved_deps) = cached_deps {
      resolved_deps
    } else {
      let warnings_len_before_resolve = warnings.len();
      let resolved_deps = resolve_dependencies(
        &self.resolved_id,
        &self.ctx.options,
        &self.ctx.resolver,
        &self.ctx.plugin_driver,
        &raw_import_records,
        ecma_view.source.clone(),
        &mut warnings,
      )
      .await?;
      resolution_warned = warnings.len() != warnings_len_before_resolve;
      resolved_deps
    };
    if let (Some(cache), Some(key)) = (build_cache, &key) {
      let resolved = |mut entry: CachedModule| {
        let file = entry.files[0].0.clone();
        // Resolution warnings (e.g. unresolved imports treated as external) would not replay on
        // a hit, so such modules are never cached.
        if resolution_warned {
          cache.uncacheable(&file);
          return;
        }
        entry.resolved_deps = resolved_deps.iter().map(CachedResolvedId::encode).collect();
        let module_file = std::path::Path::new(&file);
        let hook_resolutions = self
          .ctx
          .plugin_driver
          .module_hook_resolutions
          .remove(id.as_str())
          .map(|(_, resolutions)| resolutions)
          .unwrap_or_default();
        let resolutions = raw_import_records
          .iter()
          .zip(resolved_deps.iter())
          .map(|(record, dep)| (Some(module_file), record.module_request.as_str(), Some(dep)))
          .chain(hook_resolutions.iter().map(|r| {
            let importer = r.importer.as_deref().and_then(BuildCache::file_of);
            (importer.map(std::path::Path::new), r.specifier.as_str(), r.resolved.as_ref())
          }));
        let importer_package_json =
          self.resolved_id.package_json.as_ref().map(|p| p.realpath().to_path_buf());
        let dirs = Self::resolution_dirs(
          module_file,
          importer_package_json.as_deref(),
          &self.ctx.options.cwd,
          resolutions,
          |importer, specifier| self.ctx.resolver.tsconfig_candidates(importer, specifier),
        );
        for dir in dirs {
          let Some(hash) = cache.hash_file(&dir) else {
            cache.uncacheable(&file);
            return;
          };
          if !entry.files.iter().any(|(f, _)| *f == dir) {
            entry.files.push((dir, hash));
          }
        }
        cache.keep(id.as_arc_str().clone(), key, entry, false);
      };
      match (hit, new_entry) {
        (Some((module, _)), _) if hit_deps => {
          cache.keep(id.as_arc_str().clone(), key, module, true);
        }
        (Some((module, _)), _) => resolved(module),
        (None, Some(entry)) => resolved(entry),
        (None, None) => {}
      }
    }

    for (record, info) in raw_import_records.iter().zip(&resolved_deps) {
      match record.kind {
        ImportKind::Import | ImportKind::Require | ImportKind::NewUrl => {
          ecma_view.imported_ids.insert(info.id.clone());
        }
        ImportKind::DynamicImport => {
          ecma_view.dynamically_imported_ids.insert(info.id.clone());
        }
        ImportKind::HotAccept => {
          ecma_view.hmr_info.deps.insert(info.id.clone());
        }
        // for a none css module, we should not have `at-import` or `url-import`
        ImportKind::AtImport | ImportKind::UrlImport => unreachable!(),
      }
    }

    let repr_name = self.resolved_id.id.representative_name();
    let repr_name = legitimize_identifier_name(&repr_name).into_owned();

    // Build lazy barrel info if the experimental flag is enabled
    let barrel_info = if self.flat_options.is_lazy_barrel_enabled() {
      try_extract_lazy_barrel_info(&ecma_view, &raw_import_records)
    } else {
      None
    };

    // Eagerly resolving every import in a giant barrel is a known bottleneck.
    // The threshold targets only the real outliers (large icon packs); normal
    // component and utility barrels stay well below it.
    if barrel_info.is_some()
      && self.ctx.options.checks.contains(EventKindSwitcher::LargeBarrelModules)
    {
      const LARGE_BARREL_IMPORT_THRESHOLD: usize = 5000;
      let import_record_count = raw_import_records.len();
      if import_record_count > LARGE_BARREL_IMPORT_THRESHOLD {
        if let Some(on_log) = self.ctx.options.on_log.as_ref() {
          let event = BuildDiagnostic::large_barrel_modules(id.to_string(), import_record_count)
            .with_severity(rolldown_error::Severity::Info)
            .to_diagnostic_with(&DiagnosticOptions { cwd: self.ctx.options.cwd.clone() });
          on_log
            .call(
              rolldown_common::LogLevel::Info,
              rolldown_common::Log {
                message: event.to_color_string(),
                id: Some(id.to_string()),
                code: Some(event.kind()),
                ..Default::default()
              },
            )
            .await?;
        }
      }
    }

    let module = NormalModule {
      repr_name,
      stable_id,
      id,
      debug_id: self.resolved_id.debug_id(&self.ctx.options.cwd),
      idx: self.module_idx,
      exec_order: u32::MAX,
      module_type: module_type.clone(),
      ecma_view,
      originative_resolved_id: self.resolved_id.clone(),
    };

    let module_info =
      Arc::new(module.to_module_info(Some(&raw_import_records), self.is_user_defined_entry));
    self.ctx.plugin_driver.set_module_info(&module.id, Arc::clone(&module_info));
    self.ctx.plugin_driver.module_parsed(Arc::clone(&module_info), &module).await?;
    self.ctx.plugin_driver.mark_context_load_modules_loaded(module.id.clone());

    let result = ModuleLoaderMsg::NormalModuleDone(Box::new(NormalModuleTaskResult {
      module: module.into(),
      ecma_related,
      resolved_deps,
      raw_import_records,
      warnings,
      barrel_info,
      tla_keyword_span,
    }));

    self.ctx.tx.send(result).expect(
      "ModuleLoader channel closed while sending module completion - main thread terminated unexpectedly"
    );

    Ok(())
  }

  /// An entry of what the hooks produced for this module, without its resolved dependencies, or
  /// `None` when it can't be cached.
  fn start_entry(
    &self,
    cache: &BuildCache,
    code: &str,
    module_type: &ModuleType,
    side_effects_override: Option<rolldown_common::side_effects::HookSideEffects>,
    sourcemap_chain: &[SourcemapChainElement],
  ) -> Option<CachedModule> {
    let file = BuildCache::file_of(self.resolved_id.id.as_str())?;
    let Some(hash) = cache.hash_file(file) else {
      cache.uncacheable(file);
      return None;
    };
    let mut files = vec![(file.to_string(), hash)];
    let mut extra: Vec<String> = self
      .ctx
      .plugin_driver
      .transform_dependencies
      .get(&self.module_idx)
      .map(|deps| deps.iter().map(|dep| dep.as_path().to_string_lossy().into_owned()).collect())
      .unwrap_or_default();
    let tsconfig =
      self.ctx.options.transform_options.discover_tsconfig_file(std::path::Path::new(file));
    extra.extend(cache.tsconfig_deps(std::path::Path::new(file), tsconfig.as_deref()));
    extra.sort_unstable();
    extra.dedup();
    for dep in extra {
      let Some(hash) = cache.hash_file(&dep) else {
        cache.uncacheable(file);
        return None;
      };
      files.push((dep, hash));
    }
    Some(CachedModule {
      files,
      code: code.to_string(),
      module_type: module_type.to_string(),
      side_effects: side_effects_override.map(encode_hook_side_effects),
      // Without sourcemaps in the output, the chain is never read.
      sourcemap_chain: if self.ctx.options.sourcemap.is_some() {
        sourcemap_chain.iter().map(CachedSourcemap::encode).collect()
      } else {
        vec![]
      },
      resolved_deps: vec![],
      meta: None,
    })
  }

  /// Directory entries that decide what this module's imports, and what its hooks resolved
  /// through `PluginContext::resolve`, resolve to, as `BuildCache::entries_dep` paths:
  /// - for each relative specifier, the entries of the directory it points into named like its
  ///   last segment without its extension (`./dep.js` → `dep*`, which an extension alias may
  ///   find as `dep.ts`);
  /// - for each bare specifier, the same for every candidate the governing tsconfig's `paths` and
  ///   `baseUrl` give it, and the entries named `index` and `package.json` inside it (a directory
  ///   there wins once it has them), since resolution tries them before `node_modules`;
  /// - for each resolved file outside `node_modules`, the entries next to it named like its stem
  ///   (`Foo.vue` → `Foo*`), and for an `index` file the entries named like its directory
  ///   (`dep.js` beats `dep/index.js`) and the entries named `package.json` beside it;
  /// - for that file and the module, the entries named `package.json` of each directory from
  ///   theirs up to the `package.json` that governs them (to the root when none does), since one
  ///   appearing closer starts a new package scope, and that `package.json` itself: `exports`,
  ///   `main` and `imports` decide where imports resolve, and `type` and `sideEffects` are kept
  ///   with the resolution.
  ///
  /// `resolutions`: importer (the cwd for none), specifier and result (`None`: it failed).
  /// Packages in `node_modules` are covered by the caller's `key` (lockfile).
  fn resolution_dirs<'a>(
    module_file: &std::path::Path,
    module_package_json: Option<&std::path::Path>,
    cwd: &std::path::Path,
    resolutions: impl Iterator<Item = (Option<&'a std::path::Path>, &'a str, Option<&'a ResolvedId>)>,
    tsconfig_candidates: impl Fn(&std::path::Path, &str) -> Vec<std::path::PathBuf>,
  ) -> Vec<String> {
    use std::path::Path;
    fn push_named(deps: &mut Vec<String>, dir: &Path, prefix: &str) {
      let dep = BuildCache::entries_dep(&dir.to_string_lossy(), prefix);
      if !dep.contains("/node_modules/") && !deps.contains(&dep) {
        deps.push(dep);
      }
    }
    fn push(deps: &mut Vec<String>, path: &Path) {
      if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
        push_named(deps, dir, &name.to_string_lossy());
      }
    }
    fn push_stem(deps: &mut Vec<String>, path: &Path) {
      if let (Some(dir), Some(stem)) = (path.parent(), path.file_stem()) {
        push_named(deps, dir, &stem.to_string_lossy());
      }
    }
    fn package_scope(deps: &mut Vec<String>, file: &Path, package_json: Option<&Path>) {
      let scope = package_json.and_then(Path::parent);
      for dir in file.ancestors().skip(1) {
        push(deps, &dir.join("package.json"));
        if Some(dir) == scope {
          break;
        }
      }
      if let Some(package_json) = package_json {
        let package_json = package_json.to_string_lossy();
        if !package_json.contains("/node_modules/") && !deps.iter().any(|d| *d == package_json) {
          deps.push(package_json.into_owned());
        }
      }
    }
    let mut deps: Vec<String> = vec![];
    package_scope(&mut deps, module_file, module_package_json);
    for (importer, specifier, resolved) in resolutions {
      if specifier.starts_with('.') {
        let base = importer.and_then(Path::parent).unwrap_or(cwd);
        push_stem(&mut deps, &normalize_path(&base.join(specifier)));
      } else if !specifier.starts_with(['/', '\0']) {
        for candidate in tsconfig_candidates(importer.unwrap_or(module_file), specifier) {
          push_stem(&mut deps, &candidate);
          push_named(&mut deps, &candidate, "index");
          push_named(&mut deps, &candidate, "package.json");
        }
      }
      let Some(dep) = resolved else { continue };
      let Some(file) = BuildCache::file_of(dep.id.as_str()).map(Path::new) else { continue };
      package_scope(&mut deps, file, dep.package_json.as_ref().map(|p| p.realpath()));
      let (Some(dir), Some(stem)) = (file.parent(), file.file_stem()) else { continue };
      push_named(&mut deps, dir, &stem.to_string_lossy());
      if stem == "index" {
        push(&mut deps, dir);
        push(&mut deps, &dir.join("package.json"));
      }
    }
    deps.sort_unstable();
    deps
  }

  #[tracing::instrument(level = "debug", skip_all)]
  async fn load_source(
    &self,
    sourcemap_chain: &mut Vec<SourcemapChainElement>,
    hook_side_effects: &mut Option<rolldown_common::side_effects::HookSideEffects>,
    magic_string_tx: Option<std::sync::mpsc::Sender<SourceMapGenMsg>>,
  ) -> BuildResult<(StrOrBytes, ModuleType)> {
    let mut is_read_from_disk = true;
    let result = load_source(
      &self.ctx.plugin_driver,
      &self.resolved_id,
      self.ctx.fs.clone(),
      sourcemap_chain,
      hook_side_effects,
      &self.ctx.options,
      self.asserted_module_type.as_ref(),
      &mut is_read_from_disk,
      self.module_idx,
    )
    .await;
    if is_read_from_disk {
      // - Only add watch files for files read from disk.
      // - Add watch files as early as possible for we might be able to recover from build errors.
      self.ctx.plugin_driver.watch_files.insert(self.resolved_id.id.as_arc_str().clone());
      // The tsconfig governing this module affects its transform and
      // resolution results, so watch it as well (#9598).
      if let Some(tsconfig_path) = self
        .ctx
        .options
        .transform_options
        .discover_tsconfig_file(std::path::Path::new(self.resolved_id.id.as_str()))
      {
        self.ctx.plugin_driver.watch_files.insert(tsconfig_path.to_string_lossy().as_ref().into());
      }
    }
    let (source, mut module_type) = result.map_err(|err| {
      downcast_napi_error_diagnostics(err).unwrap_or_else(|e| {
        BuildDiagnostic::unloadable_dependency(
          self.resolved_id.debug_id(self.ctx.options.cwd.as_path()).into(),
          self.owner.as_ref().map(|owner| UnloadableDependencyContext {
            source: owner.source.clone(),
            importer_id: owner.importer_id.clone(),
            importee_span: owner.importee_span,
          }),
          e.to_string().into(),
        )
      })
    })?;
    let source = match source {
      _ if self.resolved_id.id.starts_with("rolldown:") => source,
      StrOrBytes::Str(source) => {
        // Run plugin transform.
        let source = transform_source(
          &self.ctx.plugin_driver,
          &self.resolved_id,
          self.module_idx,
          source,
          sourcemap_chain,
          hook_side_effects,
          &mut module_type,
          magic_string_tx,
        )
        .await?;
        source.into()
      }
      StrOrBytes::Bytes(_) => source,
    };
    if let ModuleType::Custom(_) = module_type {
      // TODO: should provide some diagnostics for user how they should handle the module type.
      // e.g.
      // sass -> recommended npm install `sass` etc
      Err(anyhow::anyhow!(
        "`{}` is not specified module type,  rolldown can't handle this asset correctly. Please use the load/transform hook to transform the resource",
        self.resolved_id.id
      ))?;
    }
    Ok((source, module_type))
  }
}

/// `a/./b/../c` → `a/c`, without touching the file system.
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
  use std::path::Component;
  let mut out = std::path::PathBuf::new();
  for c in path.components() {
    match c {
      Component::CurDir => {}
      Component::ParentDir => {
        out.pop();
      }
      c => out.push(c.as_os_str()),
    }
  }
  out
}
