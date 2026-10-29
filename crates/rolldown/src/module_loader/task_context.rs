use oxc::transformer_plugins::ReplaceGlobalDefinesConfig;
use rolldown_common::ModuleLoaderMsg;
use rolldown_fs::FileSystem;
use rolldown_plugin::SharedPluginDriver;

use crate::{SharedOptions, SharedResolver, types::module_record_store::SharedModuleRecordStore};

/// Used to store common data shared between all tasks.
pub struct TaskContext<Fs: FileSystem> {
  pub options: SharedOptions,
  pub tx: tokio::sync::mpsc::UnboundedSender<ModuleLoaderMsg>,
  pub resolver: SharedResolver<Fs>,
  pub fs: Fs,
  pub plugin_driver: SharedPluginDriver,
  pub meta: TaskContextMeta,
  pub module_records: Option<SharedModuleRecordStore>,
}

pub struct TaskContextMeta {
  pub replace_global_define_config: Option<ReplaceGlobalDefinesConfig>,
}
