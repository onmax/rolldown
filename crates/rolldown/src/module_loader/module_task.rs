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
use crate::types::module_record_store::{
  ModuleRecord, ModuleRecordStore, RecordedResolvedId, RecordedSourcemap,
  decode_hook_side_effects, encode_hook_side_effects,
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
    if let Err(errs) = self.run_inner().await {
      self.ctx.plugin_driver.mark_context_load_modules_loaded(self.resolved_id.id.clone());
      self
        .ctx
        .tx
        .send(ModuleLoaderMsg::BuildErrors(errs.into_vec().into_boxed_slice()))
        .expect("ModuleLoader: failed to send build errors - main thread terminated while processing module errors");
    }
  }

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
    // Native magic string fills the sourcemap chain after the scan, which a record would miss.
    let module_records =
      self.ctx.module_records.as_ref().filter(|_| self.magic_string_tx.is_none());
    let reused = module_records.and_then(|store| store.reusable(id.as_str())).and_then(|record| {
      let chain = record.sourcemap_chain.iter().map(RecordedSourcemap::decode).collect::<Option<Vec<_>>>()?;
      Some((record, chain))
    });
    if module_records.is_some() {
      self.ctx.plugin_driver.reused_module_meta.insert(
        id.as_arc_str().clone(),
        reused.as_ref().and_then(|(record, _)| record.meta.as_deref().map(ArcStr::from)),
      );
    }
    let (source, module_type) = if let Some((record, chain)) = &reused {
      sourcemap_chain.clone_from(chain);
      hook_side_effects = record.hook_side_effects.map(decode_hook_side_effects);
      self.ctx.plugin_driver.watch_files.insert(self.resolved_id.id.as_arc_str().clone());
      (StrOrBytes::Str(record.code.clone()), record.module_type())
    } else {
      self
        .load_source(&mut sourcemap_chain, &mut hook_side_effects, self.magic_string_tx.clone())
        .await?
    };
    let new_record = match (&reused, module_records, &source) {
      (None, Some(store), StrOrBytes::Str(code)) => self.start_record(store, code, &module_type, hook_side_effects, &sourcemap_chain),
      _ => None,
    };

    let stable_id = id.stabilize(&self.ctx.options.cwd);

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

    let recorded_deps = reused.as_ref().and_then(|(record, _)| {
      (record.resolved_deps.len() == raw_import_records.len()).then(|| {
        record.resolved_deps.iter().map(RecordedResolvedId::decode).collect::<IndexVec<_, _>>()
      })
    });
    let reused_deps = recorded_deps.is_some();
    let resolved_deps = if let Some(resolved_deps) = recorded_deps {
      resolved_deps
    } else {
      resolve_dependencies(
        &self.resolved_id,
        &self.ctx.options,
        &self.ctx.resolver,
        &self.ctx.plugin_driver,
        &raw_import_records,
        ecma_view.source.clone(),
        &mut warnings,
      )
      .await?
    };
    if let Some(store) = module_records {
      let resolved = |mut record: ModuleRecord| {
        record.resolved_deps = resolved_deps.iter().map(RecordedResolvedId::encode).collect();
        let package_json = self.resolved_id.package_json.as_ref().map(|p| p.realpath());
        let path_fallbacks = |file: &str| self.ctx.resolver.has_path_fallbacks(std::path::Path::new(file));
        let Some(dirs) = Self::resolution_dirs(
          id.as_str(),
          package_json,
          &raw_import_records,
          &resolved_deps,
          path_fallbacks,
        ) else {
          store.unrecordable(&record.files[0].0);
          return;
        };
        for dir in dirs {
          let Some(hash) = store.hash_file(&dir) else {
            store.unrecordable(&record.files[0].0);
            return;
          };
          if !record.files.iter().any(|(f, _)| *f == dir) {
            record.files.push((dir, hash));
          }
        }
        store.keep(record, false);
      };
      match (reused, new_record) {
        (Some((record, _)), _) if reused_deps => store.keep(record.clone(), true),
        (Some((record, _)), _) => resolved(record.clone()),
        (None, Some(record)) => resolved(record),
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

  /// A record of what the hooks produced for this module, or `None` when the module cannot be
  /// reused in a later build: it has no file behind it.
  fn start_record(
    &self,
    store: &ModuleRecordStore,
    code: &str,
    module_type: &ModuleType,
    hook_side_effects: Option<rolldown_common::side_effects::HookSideEffects>,
    sourcemap_chain: &[SourcemapChainElement],
  ) -> Option<ModuleRecord> {
    let id = self.resolved_id.id.as_str();
    let file = ModuleRecordStore::file_of(id)?;
    if self.ctx.plugin_driver.modules_with_transform_emitted_files.contains(&self.module_idx) {
      store.unrecordable(file);
      return None;
    }
    let Some(hash) = store.hash_file(file) else {
      store.unrecordable(file);
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
    if let Some(tsconfig) =
      self.ctx.options.transform_options.discover_tsconfig_file(std::path::Path::new(file))
    {
      extra.push(tsconfig.to_string_lossy().into_owned());
    }
    extra.sort_unstable();
    extra.dedup();
    for dep in extra {
      let Some(hash) = store.hash_file(&dep) else {
        store.unrecordable(file);
        return None;
      };
      files.push((dep, hash));
    }
    Some(ModuleRecord {
      id: id.to_string(),
      files,
      code: code.to_string(),
      module_type: module_type.to_string(),
      hook_side_effects: hook_side_effects.map(encode_hook_side_effects),
      // Without sourcemaps in the output, the chain is never read.
      sourcemap_chain: if self.ctx.options.sourcemap.is_some() {
        sourcemap_chain.iter().map(RecordedSourcemap::encode).collect()
      } else {
        vec![]
      },
      resolved_deps: vec![],
      meta: None,
    })
  }

  /// Directory entries that decide what this module's imports resolve to, as
  /// `ModuleRecordStore::entries_dep` paths: for each relative specifier, the entries of the
  /// directory it points into named like its last segment (`./dep` → `dep*`); for each resolved
  /// file outside `node_modules`, the entries next to it named like its stem (`Foo.vue` → `Foo*`),
  /// and for an `index` file the entries named like its directory (`dep.js` beats `dep/index.js`).
  /// An import that resolves into a local package other than the importer's (outside
  /// `node_modules`; `./lib` with its own `package.json` counts), or any bare import of a local
  /// package, also depends on that `package.json` (`exports`, `main`, `imports`); an `index` file
  /// also depends on the entries named `package.json` beside it, since one appearing there can
  /// redirect the directory. Packages in `node_modules` are covered by the caller's `key`
  /// (lockfile). `None`: a bare import resolved to a local file from a module whose tsconfig has
  /// `paths` fallbacks (several targets), whose other candidates aren't tracked yet; such a
  /// module is not recorded.
  fn resolution_dirs(
    importer: &str,
    importer_package_json: Option<&std::path::Path>,
    raw_import_records: &IndexVec<rolldown_common::ImportRecordIdx, rolldown_common::RawImportRecord>,
    resolved_deps: &IndexVec<rolldown_common::ImportRecordIdx, ResolvedId>,
    path_fallbacks: impl Fn(&str) -> bool,
  ) -> Option<Vec<String>> {
    use std::path::Path;
    let mut deps: Vec<String> = vec![];
    let mut push = |path: &Path| {
      let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return };
      let dep = ModuleRecordStore::entries_dep(&dir.to_string_lossy(), &name.to_string_lossy());
      if !dep.contains("/node_modules/") && !deps.contains(&dep) {
        deps.push(dep);
      }
    };
    let importer_dir = ModuleRecordStore::file_of(importer).and_then(|f| Path::new(f).parent());
    for record in raw_import_records {
      let specifier = record.module_request.as_str();
      if let (Some(base), true) = (importer_dir, specifier.starts_with('.')) {
        push(&normalize_path(&base.join(specifier)));
      }
    }
    let mut package_jsons: Vec<String> = vec![];
    let mut checked_fallbacks = false;
    for (record, dep) in raw_import_records.iter().zip(resolved_deps.iter()) {
      let specifier = record.module_request.as_str();
      let bare = !specifier.starts_with(['.', '/', '\0']);
      let local = ModuleRecordStore::file_of(dep.id.as_str()).is_some_and(|f| !f.contains("/node_modules/"));
      if bare && local && !checked_fallbacks {
        checked_fallbacks = true;
        if ModuleRecordStore::file_of(importer).is_some_and(&path_fallbacks) {
          return None;
        }
      }
      let Some(package_json) = dep.package_json.as_ref().map(|p| p.realpath()) else { continue };
      let other_package = importer_package_json != Some(package_json);
      let package_json = package_json.to_string_lossy();
      if (bare || other_package)
        && !package_json.contains("/node_modules/")
        && !package_jsons.iter().any(|d| *d == package_json)
      {
        package_jsons.push(package_json.into_owned());
      }
    }
    for dep in resolved_deps {
      let Some(file) = ModuleRecordStore::file_of(dep.id.as_str()).map(Path::new) else { continue };
      let (Some(dir), Some(stem)) = (file.parent(), file.file_stem()) else { continue };
      push(&dir.join(stem));
      if stem == "index" {
        push(dir);
        push(&dir.join("package.json"));
      }
    }
    deps.extend(package_jsons);
    deps.sort_unstable();
    Some(deps)
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
