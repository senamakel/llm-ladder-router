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
    run_with(&config_path(std::env::args().skip(1))).await
}

/// Loads one configuration file and serves from it.
async fn run_with(path: &str) -> Result<()> {
    let config = Config::load(path)?;
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

#[cfg(test)]
mod test {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{DEFAULT_CONFIG, config_path};

    fn parse(args: &[&str]) -> String {
        config_path(args.iter().map(|arg| (*arg).to_string()))
    }

    #[test]
    fn defaults_when_no_config_is_named() {
        assert_eq!(parse(&[]), DEFAULT_CONFIG);
        assert_eq!(parse(&["--verbose"]), DEFAULT_CONFIG);
    }

    #[test]
    fn reads_the_separated_form() {
        assert_eq!(parse(&["--config", "/etc/ladder.toml"]), "/etc/ladder.toml");
    }

    #[test]
    fn reads_the_joined_form() {
        assert_eq!(parse(&["--config=/etc/ladder.toml"]), "/etc/ladder.toml");
    }

    #[test]
    fn skips_arguments_before_the_flag() {
        assert_eq!(parse(&["--verbose", "--config", "a.toml"]), "a.toml");
    }

    #[test]
    fn the_first_config_wins() {
        assert_eq!(
            parse(&["--config", "first.toml", "--config", "second.toml"]),
            "first.toml"
        );
    }

    #[test]
    fn a_trailing_flag_with_no_value_falls_back_to_the_default() {
        // `--config` as the final argument has nothing to read, and guessing
        // would be worse than using the documented default.
        assert_eq!(parse(&["--config"]), DEFAULT_CONFIG);
    }

    #[tokio::test]
    async fn a_missing_configuration_file_stops_the_router() {
        // `run` is the whole binary minus argument parsing and logging setup.
        let error = super::run_with("/nonexistent/ladder-router/config.toml")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot read config"), "{error}");
    }
}
