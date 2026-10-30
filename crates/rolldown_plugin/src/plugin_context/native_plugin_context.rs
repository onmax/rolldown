use std::{
  borrow::Cow,
  path::{Path, PathBuf},
  sync::{Arc, Mutex, Weak},
};

use anyhow::Context;
use arcstr::ArcStr;
use derive_more::Debug;
use rolldown_common::{
  FilenameTemplate, LogLevel, LogWithoutPlugin, ModuleDefFormat, ModuleId, ModuleLoaderMsg,
  PackageJson, PluginIdx, ResolvedId, SharedFileEmitter, SharedModuleInfoDashMap,
  SharedNormalizedBundlerOptions, WatchPath, side_effects::HookSideEffects,
};
use rolldown_resolver::{ResolveError, Resolver};
use rolldown_utils::dashmap::FxDashSet;
use tracing::Instrument;

use crate::{
  PluginDriver,
  plugin_context::PluginContextMeta,
  types::{
    hook_resolve_id_skipped::HookResolveIdSkipped,
    plugin_context_resolve_options::PluginContextResolveOptions,
  },
  utils::resolve_id_check_external::resolve_id_check_external,
};

pub type SharedNativePluginContext = Arc<NativePluginContextImpl>;

#[derive(Debug)]
pub struct NativePluginContextImpl {
  pub(crate) plugin_name: Cow<'static, str>,
  pub(crate) skipped_resolve_calls: Vec<Arc<HookResolveIdSkipped>>,
  pub(crate) plugin_idx: PluginIdx,
  pub(crate) resolver: Arc<Resolver>,
  pub(crate) meta: Arc<PluginContextMeta>,
  pub(crate) plugin_driver: Weak<PluginDriver>,
  pub(crate) file_emitter: SharedFileEmitter,
  pub(crate) options: SharedNormalizedBundlerOptions,
  pub(crate) watch_files: Arc<FxDashSet<ArcStr>>,
  pub(crate) module_infos: SharedModuleInfoDashMap,
  pub(crate) tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ModuleLoaderMsg>>>>,
  pub(crate) session: rolldown_devtools::Session,
  pub(crate) bundle_span: tracing::Span,
  // `resolve_id` hook not only will be triggered by the rolldown's resolve process, but also could be triggered
  // by manual calls of `PluginContext.resolve()`. We use a dedicated span here to distinguish whether the call is
  // - automatic (by rolldown) or manual (by `PluginContext.resolve()`)
  pub(crate) manual_resolve_span: tracing::Span,
}

impl NativePluginContextImpl {
  pub async fn load(
    &self,
    specifier: &str,
    side_effects: Option<HookSideEffects>,
    module_def_format: ModuleDefFormat,
  ) -> anyhow::Result<()> {
    // `experimental.buildCache`: a result built from another module depends on more than the
    // calling module's entry tracks.
    if self.options.experimental.build_cache.is_some()
      && let (Some(module), Some(plugin_driver)) =
        (crate::hook_module(), self.plugin_driver.upgrade())
    {
      plugin_driver.mark_uncacheable(&module);
    }
    // Clone out the sender under the lock, then drop the lock before awaiting.
    let sender = {
      let guard = self.tx.lock().ok().context("Failed to acquire PluginDriver tx lock")?.clone();
      guard.context("The `PluginContext.load` only work at `resolveId/load/transform/moduleParsed` hooks. If you using it at resolveId hook, please make sure it could not load the entry module.")?
    };
    sender
      .send(ModuleLoaderMsg::FetchModule(Box::new(ResolvedId {
        id: ModuleId::new(specifier),
        side_effects,
        module_def_format,
        ..Default::default()
      })))
      .map_err(|e| anyhow::Error::new(e).context("PluginContext: failed to send FetchModule message - module loader shut down during plugin execution"))?;
    let plugin_driver = self
      .plugin_driver
      .upgrade()
      .ok_or_else(|| anyhow::anyhow!("Plugin driver is already dropped."))?;
    plugin_driver.wait_for_module_load_completion(specifier).await;
    Ok(())
  }

  pub fn try_get_package_json_or_create(&self, path: &Path) -> anyhow::Result<Arc<PackageJson>> {
    self.resolver.try_get_package_json_or_create(path)
  }

  pub async fn resolve(
    &self,
    specifier: &str,
    importer: Option<&str>,
    extra_options: Option<PluginContextResolveOptions>,
  ) -> anyhow::Result<Result<ResolvedId, ResolveError>> {
    let plugin_driver = self
      .plugin_driver
      .upgrade()
      .ok_or_else(|| anyhow::anyhow!("Plugin driver is already dropped."))?;

    let normalized_extra_options = extra_options.unwrap_or_default();
    if normalized_extra_options.skip_self
      && self.skipped_resolve_calls.iter().any(|call| {
        call.plugin_idx == self.plugin_idx
          && call.specifier == specifier
          && call.importer.as_deref() == importer
      })
    {
      // This means that the plugin recursively called itself
      return Ok(Err(ResolveError::NotFound(specifier.to_string())));
    }

    let skipped_resolve_calls = if normalized_extra_options.skip_self {
      let mut skipped_resolve_calls = Vec::with_capacity(self.skipped_resolve_calls.len() + 1);
      skipped_resolve_calls.extend(self.skipped_resolve_calls.clone());
      skipped_resolve_calls.push(Arc::new(HookResolveIdSkipped {
        plugin_idx: self.plugin_idx,
        importer: importer.map(Into::into),
        specifier: specifier.into(),
      }));
      Some(skipped_resolve_calls)
    } else if !self.skipped_resolve_calls.is_empty() {
      Some(self.skipped_resolve_calls.clone())
    } else {
      None
    };

    // Use the pre-created manual resolve span.
    // When PluginContext.resolve() is called from JavaScript via NAPI, the tracing span
    // context is lost across the async boundary. By making the resolve span a child of
    // the bundle span (which is a child of the session span), we ensure that both
    // CONTEXT_session_id and CONTEXT_bundle_id are inherited automatically.
    // The plugin contexts are recreated at the start of each write/generate with the new bundle span.
    let resolve = async {
      resolve_id_check_external(
        &self.resolver,
        &plugin_driver,
        specifier,
        importer,
        normalized_extra_options.is_entry,
        normalized_extra_options.import_kind,
        skipped_resolve_calls,
        normalized_extra_options.custom,
        false,
        &self.options,
      )
      .await
    }
    .instrument(self.manual_resolve_span.clone());
    if self.options.experimental.build_cache.is_none() {
      return resolve.await;
    }
    // `experimental.buildCache`: what hooks emit or watch while resolving belongs to the module the
    // caller works for (the binding opens its scope for `this.resolve` from JS), else to the
    // importer; and the resolution itself is one of that module's dependencies, since its hooks
    // may use the result.
    let module = crate::hook_module().or_else(|| importer.map(ArcStr::from));
    let result = match &module {
      Some(module) if crate::hook_module().is_none() => {
        crate::HOOK_MODULE.scope(module.clone(), resolve).await
      }
      _ => resolve.await,
    };
    if let Some(module) = module {
      plugin_driver.add_module_hook_resolution(
        &module,
        crate::HookResolution {
          importer: importer.map(ArcStr::from),
          specifier: specifier.into(),
          resolved: result.as_ref().ok().and_then(|r| r.as_ref().ok()).cloned(),
        },
      );
    }
    result
  }

  /// See `PluginDriver::uncacheable_modules`.
  fn note_emitted_file(&self) {
    if let (Some(module), Some(plugin_driver)) =
      (crate::hook_module(), self.plugin_driver.upgrade())
    {
      plugin_driver.mark_uncacheable(&module);
    }
  }

  pub fn emit_chunk(&self, chunk: rolldown_common::EmittedChunk) -> anyhow::Result<ArcStr> {
    self.note_emitted_file();
    self.file_emitter.emit_chunk(Arc::new(chunk))
  }

  pub fn emit_prebuilt_chunk(&self, chunk: rolldown_common::EmittedPrebuiltChunk) -> ArcStr {
    self.file_emitter.emit_prebuilt_chunk(chunk)
  }

  pub fn emit_file(
    &self,
    file: rolldown_common::EmittedAsset,
    fn_asset_filename: Option<String>,
    fn_sanitized_file_name: Option<String>,
  ) -> anyhow::Result<ArcStr> {
    let file_name_is_none = file.file_name.is_none();
    let asset_filename_template = file_name_is_none.then(|| {
      FilenameTemplate::new(
        self.options.asset_filenames.value(fn_asset_filename),
        "output.assetFileNames",
      )
    });
    let sanitized_file_name = file_name_is_none.then(|| {
      self.options.sanitize_filename.value(file.name_for_sanitize(), fn_sanitized_file_name)
    });

    self.note_emitted_file();
    self.file_emitter.emit_file(file, asset_filename_template, sanitized_file_name)
  }

  pub async fn emit_file_async(
    &self,
    file: rolldown_common::EmittedAsset,
  ) -> anyhow::Result<ArcStr> {
    let asset_filename = self.options.asset_filename_with_file(&file).await?;
    let sanitized_file_name = self.options.sanitize_file_name_with_file(&file).await?;
    self.note_emitted_file();
    self.file_emitter.emit_file(file, asset_filename, sanitized_file_name)
  }

  pub fn get_file_name(&self, reference_id: &str) -> anyhow::Result<ArcStr> {
    self.file_emitter.get_file_name(reference_id)
  }

  pub fn associate_module_with_file_ref(&self, module_id: &str, reference_id: &str) {
    self.file_emitter.associate_module_with_file_ref(module_id, reference_id);
  }

  pub fn get_module_info(&self, module_id: &str) -> Option<Arc<rolldown_common::ModuleInfo>> {
    self.module_infos.get(module_id).map(|v| Arc::<rolldown_common::ModuleInfo>::clone(v.value()))
  }

  pub fn get_module_ids(&self) -> Vec<ArcStr> {
    self.module_infos.iter().map(|v| v.key().clone()).collect()
  }

  /// See `PluginDriver::reused_module_meta`: `None` while the module's task hasn't decided,
  /// `""` when the module wasn't reused.
  pub fn reused_module_meta(&self, module_id: &str) -> Option<ArcStr> {
    self.plugin_driver.upgrade()?.reused_module_meta.get(module_id).map(|v| v.value().clone())
  }

  /// See `PluginDriver::module_meta_for_cache`.
  pub fn set_module_meta_for_cache(&self, entries: impl IntoIterator<Item = (ArcStr, ArcStr)>) {
    if let Some(plugin_driver) = self.plugin_driver.upgrade() {
      for (id, meta) in entries {
        plugin_driver.module_meta_for_cache.insert(id, meta);
      }
    }
  }

  pub fn cwd(&self) -> &PathBuf {
    self.resolver.cwd()
  }

  /// The path is resolved against `cwd`, so the watch files are absolute and normalized.
  pub fn add_watch_file(&self, file: &str) {
    let path = WatchPath::new(file, &self.options.cwd).to_string();
    if let (Some(module), Some(plugin_driver)) =
      (crate::hook_module(), self.plugin_driver.upgrade())
    {
      plugin_driver.add_module_hook_dependency(&module, path.clone());
    }
    self.watch_files.insert(path.into());
  }

  /// `experimental.buildCache`: see `PluginDriver::uncacheable_modules`. For JS hooks, which run
  /// outside the module's task; native ones are seen through `HOOK_MODULE`.
  pub fn mark_module_uncacheable(&self, module_id: &str) {
    if let Some(plugin_driver) = self.plugin_driver.upgrade() {
      plugin_driver.mark_uncacheable(module_id);
    }
  }

  /// `experimental.buildCache`: `addWatchFile` from a JS hook working for `module_id`, which runs
  /// outside the module's task; also watches `file` as `add_watch_file` does.
  pub fn add_module_watch_file(&self, module_id: &str, file: &str) {
    let path = WatchPath::new(file, &self.options.cwd).to_string();
    if let Some(plugin_driver) = self.plugin_driver.upgrade() {
      plugin_driver.add_module_hook_dependency(module_id, path.clone());
    }
    self.watch_files.insert(path.into());
  }

  fn log(&self, level: LogLevel, log: LogWithoutPlugin) {
    if let Some(on_log) = &self.options.on_log {
      let on_log = on_log.clone();
      let log = log.into_log(Some(self.plugin_name.to_string()));
      rolldown_utils::futures::spawn(async move {
        // FIXME: should collect error happened here and cause the build to fail later
        let _ = on_log.call(level, log).await;
      });
    }
  }

  #[inline]
  pub fn info(&self, log: LogWithoutPlugin) {
    self.log(LogLevel::Info, log);
  }

  #[inline]
  pub fn warn(&self, log: LogWithoutPlugin) {
    self.log(LogLevel::Warn, log);
  }

  #[inline]
  pub fn debug(&self, log: LogWithoutPlugin) {
    self.log(LogLevel::Debug, log);
  }
}
