use itertools::Itertools;
use napi::Env;
use napi_derive::napi;
use sugar_path::SugarPath;

use rolldown_plugin::__inner::infer_module_def_format;
use rolldown_plugin::{PluginContext, SharedNativePluginContext};

use super::types::{
  binding_emitted_asset::BindingEmittedAsset, binding_emitted_chunk::BindingEmittedChunk,
  binding_emitted_prebuilt_chunk::BindingEmittedPrebuiltChunk,
  binding_hook_side_effects::BindingHookSideEffects,
  binding_plugin_context_resolve_options::BindingPluginContextResolveOptions,
  binding_resolved_external::BindingResolvedExternal,
};

use crate::{types::binding_module_info::BindingModuleInfo, utils::napi_error};

#[napi]
pub struct BindingPluginContext {
  inner: SharedNativePluginContext,
}

#[napi]
impl BindingPluginContext {
  #[napi(
    ts_args_type = "specifier: string, sideEffects: boolean | 'no-treeshake' | undefined, packageJsonPath?: string"
  )]
  pub async fn load(
    &self,
    specifier: String,
    side_effects: Option<BindingHookSideEffects>,
    package_json_path: Option<String>,
  ) -> napi::Result<()> {
    let package_json = package_json_path
      .as_ref()
      .map(|p| self.inner.try_get_package_json_or_create(p.as_path()))
      .transpose()?;
    let module_def_format = infer_module_def_format(&specifier, package_json.as_ref());
    self
      .inner
      .load(&specifier, side_effects.map(TryInto::try_into).transpose()?, module_def_format)
      .await
      .map_err(|program_err| napi_error::load_error(&specifier, program_err))
  }

  #[napi]
  pub async fn resolve(
    &self,
    specifier: String,
    importer: Option<String>,
    mut extra_options: Option<BindingPluginContextResolveOptions>,
  ) -> napi::Result<Option<BindingPluginContextResolvedId>> {
    let cache_module = extra_options.as_mut().and_then(|options| options.cache_module.take());
    let resolve = self.inner.resolve(
      &specifier,
      importer.as_deref(),
      extra_options.map(TryInto::try_into).transpose().map_err(napi::Error::from_reason)?,
    );
    let ret = match cache_module {
      Some(module) => rolldown_plugin::HOOK_MODULE.scope(module.into(), resolve).await,
      None => resolve.await,
    }
    .map_err(|program_err| napi_error::resolve_error(&specifier, program_err))?
    .ok();

    Ok(ret.map(|info| BindingPluginContextResolvedId {
      // TODO: should use `&str` instead. (claude code) Attempt failed due to NAPI object field must be String type
      id: info.id.to_string(),
      external: info.external.into(),
      module_side_effects: info.side_effects.map(Into::into),
      // TODO: should use `&str` instead. (claude code) Attempt failed due to PathBuf conversion requires to_string_lossy()
      package_json_path:
        info.package_json.map(|item| item.realpath().to_string_lossy().to_string()),
    }))
  }

  #[napi]
  pub fn emit_file<'env>(
    &self,
    env: &'env Env,
    file: BindingEmittedAsset,
    asset_filename: Option<String>,
    fn_sanitized_file_name: Option<String>,
  ) -> napi::Result<napi::JsString<'env>> {
    let reference_id = self
      .inner
      .emit_file(file.into(), asset_filename, fn_sanitized_file_name)
      .map_err(|e| napi::Error::from_reason(e.to_string()))?;
    env.create_string(&reference_id)
  }

  #[napi]
  // Must stay sync — making this `async` would require `block_on` again
  // and reopen the emit-chunk deadlock. See `FileEmitter::emit_chunk`.
  pub fn emit_chunk<'env>(
    &self,
    env: &'env Env,
    file: BindingEmittedChunk,
  ) -> napi::Result<napi::JsString<'env>> {
    let arc_str = self.inner.emit_chunk(file.try_into()?)?;
    env.create_string(arc_str)
  }

  #[napi]
  pub fn emit_prebuilt_chunk<'env>(
    &self,
    env: &'env Env,
    file: BindingEmittedPrebuiltChunk,
  ) -> napi::Result<napi::JsString<'env>> {
    let arc_str = self.inner.emit_prebuilt_chunk(file.try_into()?);
    env.create_string(arc_str)
  }

  #[napi]
  pub fn get_file_name<'env>(
    &self,
    env: &'env Env,
    reference_id: String,
  ) -> napi::Result<napi::JsString<'env>> {
    let arc_str = self.inner.get_file_name(reference_id.as_str())?;
    env.create_string(arc_str)
  }

  #[napi]
  pub fn get_module_info(&self, module_id: String) -> Option<BindingModuleInfo> {
    self.inner.get_module_info(&module_id).map(BindingModuleInfo::new)
  }

  #[napi]
  pub fn get_module_ids<'env>(&self, env: &'env Env) -> napi::Result<Vec<napi::JsString<'env>>> {
    self.inner.get_module_ids().iter().map(|id| env.create_string(id)).try_collect()
  }

  #[napi]
  pub fn add_watch_file(&self, file: String) {
    self.inner.add_watch_file(&file);
  }

  /// `experimental.buildCache`: a JS hook working for `module_id` did something a reused entry
  /// can't replay (emitted a file, loaded another module, ...), so the module isn't kept.
  #[napi]
  pub fn mark_module_uncacheable(&self, module_id: String) {
    self.inner.mark_module_uncacheable(&module_id);
  }

  /// `experimental.buildCache`: `addWatchFile` from a hook working for `module_id` outside its
  /// `load` and `transform` (resolving one of its imports); the module's entry depends on `file`.
  #[napi]
  pub fn add_module_watch_file(&self, module_id: String, file: String) {
    self.inner.add_module_watch_file(&module_id, &file);
  }

  /// `experimental.buildCache`: the module options JSON kept for a module the scan reused, `""`
  /// when it wasn't reused, `null` while the module's task hasn't decided.
  #[napi]
  pub fn get_reused_module_meta(&self, module_id: String) -> Option<String> {
    self.inner.reused_module_meta(&module_id).map(|meta| meta.to_string())
  }

  /// `experimental.buildCache`: the `meta` JSON of modules, to keep in their records.
  #[napi]
  pub fn set_module_meta_for_cache(&self, ids: Vec<String>, metas: Vec<String>) {
    self.inner.set_module_meta_for_cache(
      ids.into_iter().zip(metas).map(|(id, meta)| (id.into(), meta.into())),
    );
  }
}

impl From<PluginContext> for BindingPluginContext {
  fn from(ctx: PluginContext) -> Self {
    match ctx {
      PluginContext::Napi(_) => unreachable!("Js plugins don't have PluginContext::Napi"),
      PluginContext::Native(inner) => Self { inner },
    }
  }
}
#[napi_derive::napi(object, object_from_js = false)]
pub struct BindingPluginContextResolvedId {
  pub id: String,
  pub package_json_path: Option<String>,
  #[napi(ts_type = "boolean | 'absolute' | 'relative'")]
  pub external: BindingResolvedExternal,
  #[napi(ts_type = "boolean | 'no-treeshake'")]
  pub module_side_effects: Option<BindingHookSideEffects>,
}
