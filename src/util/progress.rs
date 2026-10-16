use std::fmt;

use indicatif::{ProgressBar, ProgressState, ProgressStyle};

/// Formats a rate with 3 significant digits and an SI suffix, so that the
/// progress bar does not jitter with every update.
pub struct SignificantRate(pub f64);

impl fmt::Display for SignificantRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (value, suffix) = match self.0 {
            r if r >= 1e9 => (r / 1e9, "G"),
            r if r >= 1e6 => (r / 1e6, "M"),
            r if r >= 1e3 => (r / 1e3, "k"),
            r => (r, ""),
        };
        let decimals = match value {
            v if v >= 100.0 => 0,
            v if v >= 10.0 => 1,
            _ => 2,
        };
        write!(f, "{value:.decimals$}{suffix}")
    }
}

/// Style of a progress bar over input rows, with a stable rate display.
pub fn rows_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner} {prefix} {msg} {wide_bar} {human_pos}/{human_len} {rate:>9} {eta:>7}",
    )
    .expect("progress template")
    .with_key("rate", |state: &ProgressState, w: &mut dyn fmt::Write| {
        let _ = write!(w, "{}/s", SignificantRate(state.per_sec()));
    })
}

/// Prints a line above the progress bar, prefixed with the elapsed time.
pub fn log(progress: &ProgressBar, msg: String) {
    progress.println(format!("[{:>8.1?}] {msg}", progress.elapsed()));
}
