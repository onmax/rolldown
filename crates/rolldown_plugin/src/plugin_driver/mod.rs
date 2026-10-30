mod build_hooks;
mod hook_orders;
mod output_hooks;
mod plugin_driver_factory;
mod watch_hooks;

pub use plugin_driver_factory::PluginDriverFactory;

use std::{
  ops::Deref,
  sync::{Arc, Mutex},
};

use anyhow::Context;
use arcstr::ArcStr;
use dashmap::DashMap;
use rolldown_common::{
  ModuleId, ModuleIdx, ModuleInfo, ModuleLoaderMsg, PluginIdx, SharedFileEmitter,
  SharedModuleInfoDashMap, WatchPath,
};
use rolldown_utils::dashmap::{FxDashMap, FxDashSet};
use tokio::sync::broadcast;

use crate::{
  __inner::SharedPluginable,
  PluginContext,
  plugin_driver::hook_orders::PluginHookOrders,
  type_aliases::{IndexPluginContext, IndexPluginable},
  types::build_timings::BuildTimings,
};

pub type SharedPluginDriver = Arc<PluginDriver>;

tokio::task_local! {
  /// `experimental.buildCache`: the module whose task is running, so the files native hooks emit
  /// or watch meanwhile (in its `load` and `transform`, and while its imports are resolved) count
  /// as its own. JS hooks run on another thread and report their module themselves.
  pub static HOOK_MODULE: ArcStr;
}

/// The module [`HOOK_MODULE`] names, if the caller runs inside a module's task.
pub fn hook_module() -> Option<ArcStr> {
  HOOK_MODULE.try_with(Clone::clone).ok()
}

/// `experimental.buildCache`: one `PluginContext::resolve` made for a module's hooks (`resolved`
/// is `None` when it failed).
#[derive(Debug, Clone)]
pub struct HookResolution {
  pub importer: Option<ArcStr>,
  pub specifier: ArcStr,
  pub resolved: Option<rolldown_common::ResolvedId>,
}

pub struct PluginDriver {
  plugins: IndexPluginable,
  contexts: IndexPluginContext,
  hook_orders: PluginHookOrders,
  pub(crate) should_skip_user_plugins_for_lazy_proxy_modules: bool,
  pub(crate) lazy_compilation_plugin_idx: Option<PluginIdx>,
  pub file_emitter: SharedFileEmitter,
  pub watch_files: Arc<FxDashSet<ArcStr>>,
  pub module_infos: SharedModuleInfoDashMap,
  /// Module dependencies tracked during load/transform hooks for HMR invalidation
  pub transform_dependencies: Arc<DashMap<ModuleIdx, Arc<FxDashSet<WatchPath>>>>,
  /// `experimental.buildCache`: modules whose results are not kept, since reusing them would
  /// change the output: their hooks (`load`, `transform`, or the resolution of their imports)
  /// emitted a file or a chunk, or did something else a reused entry can't replay.
  pub uncacheable_modules: FxDashSet<ArcStr>,
  /// `experimental.buildCache`: files a hook watched on behalf of a module outside its `load` and
  /// `transform` (the resolution of its imports), kept with the module's entry.
  pub module_hook_dependencies: FxDashMap<ArcStr, FxDashSet<String>>,
  /// `experimental.buildCache`: what `PluginContext::resolve` resolved for a module's hooks, whose
  /// results may depend on it; the module's entry depends on what decides those resolutions.
  pub module_hook_resolutions: FxDashMap<ArcStr, Vec<HookResolution>>,
  /// `experimental.buildCache`: the JS module options (JSON) kept in the entry of each module
  /// the scan reused (`{}` for none), `""` for a module it processed. Absent until the module's
  /// task decides.
  pub reused_module_meta: FxDashMap<ArcStr, ArcStr>,
  /// `experimental.buildCache`: the `meta` (JSON, `""` for none) of the modules the JS
  /// side knows, handed over at the end of the scan, to be kept in the entries this build writes.
  pub module_meta_for_cache: FxDashMap<ArcStr, ArcStr>,
  context_load_completion_manager: ContextLoadCompletionManager,
  pub(crate) tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ModuleLoaderMsg>>>>,
  /// Outlives the `Bundle` through `BundleHandle`, which is how the binding reads them
  /// once the build has finished.
  pub build_timings: BuildTimings,
}

impl PluginDriver {
  pub fn clear(&self) {
    self.watch_files.clear();
    self.module_infos.clear();
    // Note: transform_dependencies is NOT cleared here - it's preserved across incremental builds
    // by BundleFactory which manages its lifecycle (reset on full builds only)
    self.context_load_completion_manager.clear();
    self.file_emitter.clear();
    self.reused_module_meta.clear();
    self.module_meta_for_cache.clear();
    self.uncacheable_modules.clear();
    self.module_hook_dependencies.clear();
    self.module_hook_resolutions.clear();
  }

  pub fn set_module_info(&self, module_id: &ModuleId, module_info: Arc<ModuleInfo>) {
    self.module_infos.insert(module_id.as_arc_str().into(), module_info);
  }

  pub fn set_context_load_modules_tx(
    &self,
    tx: Option<tokio::sync::mpsc::UnboundedSender<ModuleLoaderMsg>>,
  ) -> anyhow::Result<()> {
    *self.tx.lock().ok().context("Failed to acquire PluginDriver tx lock")? = tx;
    Ok(())
  }

  pub fn mark_context_load_modules_loaded(&self, module_id: ModuleId) {
    self.context_load_completion_manager.mark_completion(module_id);
  }

  pub fn invalidate_context_load_module(&self, module_id: &ModuleId) {
    self.context_load_completion_manager.invalidate(module_id);
  }

  pub async fn wait_for_module_load_completion(&self, specifier: &str) {
    self.context_load_completion_manager.wait_for_completion(specifier.into()).await;
  }

  pub fn iter_plugin_with_context_by_order<'me>(
    &'me self,
    ordered_plugins: &'me [PluginIdx],
  ) -> impl ExactSizeIterator<Item = (PluginIdx, &'me SharedPluginable, &'me PluginContext)> + 'me
  {
    ordered_plugins.iter().copied().map(move |idx| {
      let plugin = &self.plugins[idx];
      let context = &self.contexts[idx];
      (idx, plugin, context)
    })
  }

  pub fn plugins(&self) -> &IndexPluginable {
    &self.plugins
  }

  /// See `uncacheable_modules`.
  pub fn mark_uncacheable(&self, module_id: &str) {
    if !self.uncacheable_modules.contains(module_id) {
      self.uncacheable_modules.insert(module_id.into());
    }
  }

  /// `file`: absolute and normalized, as in `watch_files`.
  pub fn add_module_hook_resolution(&self, module_id: &str, resolution: HookResolution) {
    self.module_hook_resolutions.entry(module_id.into()).or_default().push(resolution);
  }

  pub fn add_module_hook_dependency(&self, module_id: &str, file: String) {
    if let Some(deps) = self.module_hook_dependencies.get(module_id) {
      deps.insert(file);
      return;
    }
    self.module_hook_dependencies.entry(module_id.into()).or_default().insert(file);
  }

  pub fn add_transform_dependency(&self, module_idx: ModuleIdx, dependency: WatchPath) {
    self
      .transform_dependencies
      .entry(module_idx)
      .or_insert_with(|| Arc::new(FxDashSet::default()))
      .insert(dependency);
  }
}

impl Deref for PluginDriver {
  type Target = PluginHookOrders;
  fn deref(&self) -> &Self::Target {
    &self.hook_orders
  }
}

#[derive(Default)]
struct ContextLoadCompletionManager {
  notifiers: DashMap<ModuleId, ContextLoadCompletionState>,
}

enum ContextLoadCompletionState {
  Pending(broadcast::Sender<()>),
  Completed,
}

impl ContextLoadCompletionManager {
  pub async fn wait_for_completion(&self, module_id: ModuleId) {
    let mut rx = match self.notifiers.entry(module_id) {
      dashmap::Entry::Vacant(guard) => {
        let (tx, rx) = broadcast::channel(1);
        guard.insert(ContextLoadCompletionState::Pending(tx));
        rx
      }
      dashmap::Entry::Occupied(mut guard) => match guard.get_mut() {
        ContextLoadCompletionState::Pending(sender) => sender.subscribe(),
        ContextLoadCompletionState::Completed => {
          /* no need to wait */
          return;
        }
      },
    };

    if let Err(err) = rx.recv().await {
      // This happens when `.invalidate` is called before `.mark_completion` is called, which is not expected
      debug_assert!(
        false,
        "The sender was dropped while waiting for module load completion: {err}"
      );
      tracing::warn!("The sender was dropped while waiting for module load completion");
    }
  }

  pub fn mark_completion(&self, module_id: ModuleId) {
    match self.notifiers.entry(module_id) {
      dashmap::Entry::Vacant(guard) => {
        guard.insert(ContextLoadCompletionState::Completed);
      }
      dashmap::Entry::Occupied(mut guard) => match guard.get_mut() {
        ContextLoadCompletionState::Pending(sender) => {
          sender.send(()).expect(
            "PluginDriver: failed to send completion notification - receiver was dropped before wait_for_completion was called, indicating a race condition in module loading"
          );
          *guard.get_mut() = ContextLoadCompletionState::Completed;
        }
        ContextLoadCompletionState::Completed => {
          // This happens if `.mark_completion` is called multiple times, which is not expected
          debug_assert!(false, "mark_completion was called even though it was already completed");
          tracing::warn!("mark_completion was called even though it was already completed");
        }
      },
    }
  }

  pub fn invalidate(&self, module_id: &ModuleId) {
    self.notifiers.remove(module_id);
  }

  pub fn clear(&self) {
    self.notifiers.clear();
  }
}
