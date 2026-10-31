use std::{
  collections::{BTreeMap, HashMap, HashSet},
  sync::Arc,
};

use super::{
  binding_output_asset::{BindingOutputAsset, JsOutputAsset},
  binding_output_chunk::{BindingOutputChunk, JsOutputChunk, update_output_chunk},
  error::BindingError,
};
use napi::Either;
use napi_derive::napi;
use rolldown_error::{BuildDiagnostic, DiagnosticOptions};
use rustc_hash::FxBuildHasher;

#[napi(object, object_from_js = false)]
pub struct BindingOutputs {
  pub chunks: Vec<BindingOutputChunk>,
  pub assets: Vec<BindingOutputAsset>,
  #[napi(ts_type = "Record<string, string | false>")]
  pub mangle_cache: Option<BTreeMap<String, Either<String, bool>>>,
}

impl BindingOutputs {
  fn new(
    outputs: Vec<rolldown_common::Output>,
    mangle_cache: Option<BTreeMap<String, Option<String>>>,
  ) -> Self {
    let mut chunks = vec![];
    let mut assets = vec![];
    outputs.into_iter().for_each(|o| match o {
      rolldown_common::Output::Chunk(chunk) => {
        chunks.push(BindingOutputChunk::new(chunk));
      }
      rolldown_common::Output::Asset(asset) => {
        assets.push(BindingOutputAsset::new(asset));
      }
    });
    let mangle_cache = mangle_cache.map(|cache| {
      cache
        .into_iter()
        .map(|(original, target)| {
          let target = target.map_or(Either::B(false), Either::A);
          (original, target)
        })
        .collect()
    });
    Self { chunks, assets, mangle_cache }
  }
}

impl From<Vec<rolldown_common::Output>> for BindingOutputs {
  fn from(outputs: Vec<rolldown_common::Output>) -> Self {
    Self::new(outputs, None)
  }
}

impl From<rolldown::BundleOutput> for BindingOutputs {
  fn from(output: rolldown::BundleOutput) -> Self {
    Self::new(output.assets, output.mangle_cache)
  }
}

/// The bundle a `generateBundle` or `writeBundle` hook gets, read on demand. A build with thousands
/// of outputs and a dozen such hooks made a JS object per output per hook as `BindingOutputs`;
/// with this, JS asks for the identities once per hook and only for the outputs it hasn't seen.
#[napi]
pub struct BindingBundle {
  chunks: Vec<Arc<rolldown_common::OutputChunk>>,
  assets: Vec<Arc<rolldown_common::OutputAsset>>,
}

#[napi]
impl BindingBundle {
  /// `BindingOutputChunk::get_code_identity` of every chunk, in output order.
  #[napi]
  pub fn chunk_identities(&self) -> Vec<i64> {
    self.chunks.iter().map(|chunk| identity(Arc::as_ptr(chunk).addr())).collect()
  }

  /// `BindingOutputAsset::get_identity` of every asset, in output order.
  #[napi]
  pub fn asset_identities(&self) -> Vec<i64> {
    self.assets.iter().map(|asset| identity(Arc::as_ptr(asset).addr())).collect()
  }

  #[napi]
  pub fn get_chunk(&self, index: u32) -> Option<BindingOutputChunk> {
    self.chunks.get(index as usize).map(|chunk| BindingOutputChunk::new(Arc::clone(chunk)))
  }

  #[napi]
  pub fn get_asset(&self, index: u32) -> Option<BindingOutputAsset> {
    self.assets.get(index as usize).map(|asset| BindingOutputAsset::new(Arc::clone(asset)))
  }
}

fn identity(address: usize) -> i64 {
  i64::try_from(address).unwrap_or(i64::MAX)
}

impl From<Vec<rolldown_common::Output>> for BindingBundle {
  fn from(outputs: Vec<rolldown_common::Output>) -> Self {
    let mut chunks = vec![];
    let mut assets = vec![];
    for output in outputs {
      match output {
        rolldown_common::Output::Chunk(chunk) => chunks.push(chunk),
        rolldown_common::Output::Asset(asset) => assets.push(asset),
      }
    }
    Self { chunks, assets }
  }
}

#[napi_derive::napi(object, object_to_js = false)]
pub struct JsChangedOutputs {
  pub deleted: HashSet<String, FxBuildHasher>,
  pub changes: HashMap<String, Either<JsOutputChunk, JsOutputAsset>, FxBuildHasher>,
}

impl JsChangedOutputs {
  pub fn apply_changes(
    &mut self,
    outputs: &mut Vec<rolldown_common::Output>,
  ) -> anyhow::Result<()> {
    let mut result = Ok(());
    if !self.deleted.is_empty() || !self.changes.is_empty() {
      outputs.retain_mut(|output| {
        if result.is_err() {
          return true;
        }
        let filename = output.filename();
        if self.deleted.contains(filename) {
          return false;
        }
        if let Some(change) = self.changes.remove(filename) {
          match (output, change) {
            (rolldown_common::Output::Chunk(old_chunk), Either::A(chunk)) => {
              if let Err(err) = update_output_chunk(old_chunk, chunk) {
                result = Err(err);
              }
            }
            (v @ rolldown_common::Output::Asset(_), Either::B(asset)) => {
              *v = rolldown_common::Output::Asset(Arc::new(asset.into()));
            }
            _ => {}
          }
        }
        true
      });
    }
    result
  }
}

pub fn to_binding_error(diagnostic: &BuildDiagnostic, cwd: std::path::PathBuf) -> BindingError {
  match diagnostic.downcast_napi_error() {
    Ok(napi_error) => {
      // Note: In WASM workers, napi::Error objects with maybe_raw/maybe_env references cannot be
      // safely shared across threads, which would cause try_clone() to fail. Currently, we don't
      // guarantee full JS error consistency in WASM environments. In the future, we could enhance
      // the BindingError fields to preserve all custom error properties and achieve complete JS
      // error consistency across all environments.
      #[cfg(not(target_family = "wasm"))]
      {
        let error = napi_error.try_clone().unwrap_or_else(|e| e);
        BindingError::JsError(napi::JsError::from(error))
      }
      #[cfg(target_family = "wasm")]
      {
        let error = napi::Error::new(napi_error.status, napi_error.reason.clone());
        BindingError::JsError(napi::JsError::from(error))
      }
    }
    Err(error) => {
      let diag = error.to_diagnostic_with(&DiagnosticOptions { cwd });

      // Extract location information from the diagnostic if available
      // Note: Line numbers, columns, and byte positions are cast to u32.
      // This is safe for practical use cases as files with >4 billion lines or bytes are extremely rare.
      #[expect(
        clippy::cast_possible_truncation,
        reason = "line/column/position values are unlikely to exceed u32::MAX in practical use"
      )]
      let (loc, pos) = if let Some((_file, line, column, position)) = diag.get_primary_location() {
        (
          Some(super::binding_log::BindingLogLocation {
            line: line as u32,
            column: column as u32,
            // Use error.id() for the file path since the diagnostic may only store the filename.
            file: error.id(),
          }),
          Some(position as u32),
        )
      } else {
        (None, None)
      };

      BindingError::NativeError(super::error::native_error::NativeError {
        kind: error.kind().to_string(),
        message: diag.to_color_string(),
        id: error.id(),
        exporter: error.exporter(),
        loc,
        pos,
      })
    }
  }
}
