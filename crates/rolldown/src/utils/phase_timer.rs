//! Wall-clock timings of the build's phases, printed to stderr when `ROLLDOWN_PHASE_TIMINGS` is set.

use std::{sync::LazyLock, time::Instant};

static ENABLED: LazyLock<bool> =
  LazyLock::new(|| std::env::var_os("ROLLDOWN_PHASE_TIMINGS").is_some());

pub struct PhaseTimer {
  label: String,
  start: Option<Instant>,
}

impl PhaseTimer {
  pub fn start(label: impl FnOnce() -> String) -> Self {
    if *ENABLED {
      Self { label: label(), start: Some(Instant::now()) }
    } else {
      Self { label: String::new(), start: None }
    }
  }

  #[expect(clippy::print_stderr)] // Lab timings go to stderr on purpose.
  pub fn lap(&mut self, phase: &str) {
    if let Some(start) = self.start {
      eprintln!(
        "[rolldown-phase] {} {phase} {:.1}ms",
        self.label,
        start.elapsed().as_secs_f64() * 1000.0
      );
      self.start = Some(Instant::now());
    }
  }
}
