//! The `ladder` proxy binary.
//!
//! Deliberately thin: it reads arguments, initializes logging, loads the
//! configuration, and hands straight to the library. Everything worth testing
//! lives behind [`llm_ladder_router`].

use llm_ladder_router::{Config, Result};

/// Where the configuration is read from when `--config` is not given.
const DEFAULT_CONFIG: &str = "config.toml";

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // A local `.env` is a convenience for development; real deployments set
    // the variables themselves, so a missing file is not an error.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "router stopped");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let path = config_path(std::env::args().skip(1));
    let config = Config::load(&path)?;
    tracing::info!(
        path = %path,
        ladders = config.ladders.len(),
        providers = config.providers.len(),
        "configuration loaded"
    );
    llm_ladder_router::serve(config).await
}

/// Reads `--config <path>` from the arguments, falling back to the default.
fn config_path(args: impl Iterator<Item = String>) -> String {
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                if let Some(path) = args.next() {
                    return path;
                }
            }
            other => {
                if let Some(path) = other.strip_prefix("--config=") {
                    return path.to_string();
                }
            }
        }
    }
    DEFAULT_CONFIG.to_string()
}
