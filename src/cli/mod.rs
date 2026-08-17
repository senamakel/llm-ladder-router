//! Everything the `ladder` binary does, minus the entry point itself.
//!
//! The binary lives at `bin/ladder.rs`, outside `src/`, and is a shell around
//! [`run`]. That split is deliberate: a `main` function cannot be exercised by a
//! unit test, so keeping it down to a handful of delegating lines leaves every
//! decision the binary makes here, under the same coverage bar as the rest of
//! the crate.

use crate::config::Config;
use crate::error::Result;

/// Where the configuration is read from when `--config` is not given.
pub const DEFAULT_CONFIG: &str = "config.toml";

/// Runs the router from the process arguments.
///
/// # Errors
///
/// Returns whatever [`run_with`] returns for the resolved path.
pub async fn run(args: impl Iterator<Item = String>) -> Result<()> {
    run_with(&config_path(args)).await
}

/// Loads one configuration file and serves from it.
///
/// # Errors
///
/// Returns a configuration error if the file cannot be read or is invalid, and
/// a bind or serve error if the server cannot start.
pub async fn run_with(path: &str) -> Result<()> {
    let config = Config::load(path)?;
    tracing::info!(
        path = %path,
        ladders = config.ladders.len(),
        providers = config.providers.len(),
        "configuration loaded"
    );
    crate::proxy::serve(config).await
}

/// Reads `--config <path>` from the arguments, falling back to the default.
///
/// Both the separated and joined spellings are accepted, because both are
/// idiomatic and neither is worth a parsing dependency.
#[must_use]
pub fn config_path(args: impl Iterator<Item = String>) -> String {
    let mut args = args;
    while let Some(arg) = args.next() {
        if arg == "--config" {
            if let Some(path) = args.next() {
                return path;
            }
        } else if let Some(path) = arg.strip_prefix("--config=") {
            return path.to_string();
        }
    }
    DEFAULT_CONFIG.to_string()
}

/// Installs the tracing subscriber the binary logs through.
///
/// Honors `RUST_LOG` and falls back to `info`.
pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // A second call in the same process is a no-op rather than a panic, which
    // keeps this safe to call from tests.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod test;
