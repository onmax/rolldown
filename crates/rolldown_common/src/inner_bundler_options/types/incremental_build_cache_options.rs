#[cfg(feature = "deserialize_bundler_options")]
use schemars::JsonSchema;
#[cfg(feature = "deserialize_bundler_options")]
use serde::Deserialize;

/// Keeps the scan stage's module records on disk, so a build in a new process reuses the
/// `load`, `transform` and `resolveId` results of modules whose files did not change.
#[derive(Debug, Default, Clone)]
#[cfg_attr(
  feature = "deserialize_bundler_options",
  derive(Deserialize, JsonSchema),
  serde(rename_all = "camelCase", deny_unknown_fields)
)]
pub struct IncrementalBuildCacheOptions {
  /// Directory the records are read from and written to.
  pub dir: String,
  /// Records written under a different key are not reused. Put everything the hooks read that
  /// Rolldown cannot see in it, such as plugin options and dependency versions.
  pub key: Option<String>,
  /// Absolute paths of files to treat as changed, for inputs the hooks read without
  /// `this.addWatchFile`.
  pub invalidate: Option<Vec<String>>,
}
