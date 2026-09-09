//! Unit tests for the binary's own logic.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{DEFAULT_CONFIG, check_with, config_path, init_tracing, run, run_with};

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
    // `--config` as the final argument has nothing to read, and guessing would
    // be worse than using the documented default.
    assert_eq!(parse(&["--config"]), DEFAULT_CONFIG);
}

#[test]
fn installing_tracing_twice_is_harmless() {
    init_tracing();
    init_tracing();
}

#[tokio::test]
async fn a_missing_configuration_file_stops_the_router() {
    let error = run_with("/nonexistent/ladder-router/config.toml")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cannot read config"), "{error}");
}

#[tokio::test]
async fn an_invalid_configuration_stops_the_router() {
    let path = std::env::temp_dir().join("llm-ladder-router-invalid.toml");
    std::fs::write(&path, "this is not = valid [toml").unwrap();

    let error = run(["--config".to_string(), path.display().to_string()].into_iter())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid config"), "{error}");

    std::fs::remove_file(&path).unwrap();
}

#[tokio::test]
async fn a_valid_configuration_is_loaded_and_handed_to_the_server() {
    // Hold a port so `serve` gets as far as binding and then fails, which
    // exercises the whole load-and-serve path without leaving a server running.
    let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = held.local_addr().unwrap();

    let path = std::env::temp_dir().join("llm-ladder-router-cli-valid.toml");
    std::fs::write(
        &path,
        format!(
            r#"
            [server]
            bind = "{taken}"

            [providers.openrouter]
            kind = "openrouter"
            # A closed loopback port: this test must never reach a real
            # marketplace, since the environment may hold a working key.
            base_url = "http://127.0.0.1:1"
            api_key_env = "LADDER_TEST_UNSET_KEY"

            [[ladders]]
            name = "flash"
              [[ladders.rungs]]
              provider = "openrouter"
              model = "m"
            "#
        ),
    )
    .unwrap();

    let error = run_with(&path.display().to_string()).await.unwrap_err();
    assert!(error.to_string().contains("cannot bind"), "{error}");

    drop(held);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn check_accepts_a_valid_configuration_without_binding() {
    let path = std::env::temp_dir().join(format!("ladder-check-ok-{}.toml", std::process::id()));
    std::fs::write(
        &path,
        r#"
        [server]
        # A port nothing may bind, so a check that started a server would fail
        # rather than pass quietly.
        bind = "0.0.0.0:6969"

        [providers.openrouter]
        kind = "openrouter"
        base_url = "http://127.0.0.1:1"
        api_key_env = "LADDER_TEST_UNSET_KEY"

        [[ladders]]
        name = "flash"
        aliases = ["chat-v1"]
          [[ladders.rungs]]
          provider = "openrouter"
          model = "m"
        "#,
    )
    .unwrap();

    check_with(&path.display().to_string()).unwrap();

    std::fs::remove_file(&path).unwrap();
}

#[test]
fn check_rejects_an_invalid_configuration() {
    let path = std::env::temp_dir().join(format!("ladder-check-bad-{}.toml", std::process::id()));
    // A rung naming a provider that does not exist: the shape of mistake a
    // deployment script must catch before overwriting a working config.
    std::fs::write(
        &path,
        r#"
        [providers.openrouter]
        kind = "openrouter"
        base_url = "http://127.0.0.1:1"
        api_key_env = "LADDER_TEST_UNSET_KEY"

        [[ladders]]
        name = "flash"
          [[ladders.rungs]]
          provider = "nowhere"
          model = "m"
        "#,
    )
    .unwrap();

    let error = check_with(&path.display().to_string()).unwrap_err();
    assert!(error.to_string().contains("unknown provider"), "{error}");

    std::fs::remove_file(&path).unwrap();
}

#[tokio::test]
async fn the_check_flag_validates_instead_of_serving() {
    let path = std::env::temp_dir().join(format!("ladder-check-flag-{}.toml", std::process::id()));
    std::fs::write(
        &path,
        r#"
        [server]
        bind = "0.0.0.0:6969"

        [providers.openrouter]
        kind = "openrouter"
        base_url = "http://127.0.0.1:1"
        api_key_env = "LADDER_TEST_UNSET_KEY"

        [[ladders]]
        name = "flash"
          [[ladders.rungs]]
          provider = "openrouter"
          model = "m"
        "#,
    )
    .unwrap();

    // Returns rather than serving; without `--check` this would block on the
    // listening socket.
    run([
        "--config".to_string(),
        path.display().to_string(),
        "--check".to_string(),
    ]
    .into_iter())
    .await
    .unwrap();

    std::fs::remove_file(&path).unwrap();
}
