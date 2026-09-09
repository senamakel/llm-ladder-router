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
/// `--check` loads and validates the configuration, then exits without
/// binding. That is what lets a deployment script prove a config is good
/// *before* it replaces the one currently working: installing a config the
/// binary refuses would otherwise leave the service restart-looping against a
/// file that has already overwritten the last one that served.
///
/// # Errors
///
/// Returns whatever [`run_with`] or [`check_with`] returns for the resolved
/// path.
pub async fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let args: Vec<String> = args.collect();
    let path = config_path(args.iter().cloned());
    if args.iter().any(|arg| arg == "--check") {
        return check_with(&path);
    }
    run_with(&path).await
}

/// Loads one configuration file, reports what it declares, and returns.
///
/// The counterpart to [`run_with`] that never binds a socket.
///
/// # Errors
///
/// Returns a configuration error if the file cannot be read or is invalid.
pub fn check_with(path: &str) -> Result<()> {
    let config = Config::load(path)?;
    // Printed rather than logged: the caller of `--check` is a script or a
    // person at a terminal wanting the answer, not a log stream.
    println!("{path}: ok");
    println!("  bind:      {}", config.server.bind);
    println!("  providers: {}", config.providers.len());
    for ladder in &config.ladders {
        let names = if ladder.aliases.is_empty() {
            ladder.name.clone()
        } else {
            format!("{} (also {})", ladder.name, ladder.aliases.join(", "))
        };
        println!("  ladder:    {names} — {} rungs", ladder.rungs.len());
    }
    Ok(())
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
