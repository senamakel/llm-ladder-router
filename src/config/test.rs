//! Unit tests for configuration loading and validation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

/// A configuration exercising both dialects, both ceiling levels, and both
/// ladders.
const EXAMPLE: &str = r#"
[server]
bind = "0.0.0.0:9000"
request_timeout = "30s"

[pricing]
refresh = "10m"
stale_after = "45m"

[credits]
refresh = "2m"
min_balance_usd = 1.5

[providers.surplus]
kind = "surplus"
base_url = "https://api.surplusintelligence.ai"
api_key_env = "SURPLUS_API_KEY"
max_cost_per_1m = 0.50

[providers.openrouter]
kind = "open_router"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
headers = { "HTTP-Referer" = "https://example.test/" }

[[ladders]]
name = "flash"
cost_basis = "completion"

  [[ladders.rungs]]
  provider = "surplus"
  model = "deepseek-v4-flash"
  max_cost_per_1m = 0.15

  [[ladders.rungs]]
  provider = "openrouter"
  model = "deepseek/deepseek-v4-flash"
  max_cost_per_1m = 0.30
  prefer = ["deepinfra"]

[[ladders]]
name = "reasoning"

  [[ladders.rungs]]
  provider = "surplus"
  model = "deepseek-v4-pro"
"#;

#[test]
fn parses_a_full_configuration() {
    let config = Config::parse(EXAMPLE).unwrap();

    assert_eq!(config.server.bind, "0.0.0.0:9000");
    assert_eq!(
        config.server.request_timeout,
        std::time::Duration::from_secs(30)
    );
    assert_eq!(
        config.pricing.stale_after,
        std::time::Duration::from_secs(45 * 60)
    );
    assert!((config.credits.min_balance_usd - 1.5).abs() < f64::EPSILON);

    assert_eq!(config.providers["surplus"].kind, ProviderKind::Surplus);
    assert_eq!(
        config.providers["openrouter"].kind,
        ProviderKind::OpenRouter
    );
    assert_eq!(
        config.providers["openrouter"].headers["HTTP-Referer"],
        "https://example.test/"
    );

    let flash = config.ladder("flash").unwrap();
    assert_eq!(flash.cost_basis, CostBasis::Completion);
    assert_eq!(flash.rungs.len(), 2);
    assert_eq!(flash.rungs[1].prefer, vec!["deepinfra"]);
}

#[test]
fn defaults_the_cost_basis_to_completion() {
    let config = Config::parse(EXAMPLE).unwrap();
    // The reasoning ladder omits `cost_basis` entirely.
    assert_eq!(
        config.ladder("reasoning").unwrap().cost_basis,
        CostBasis::Completion
    );
}

#[test]
fn applies_defaults_when_optional_sections_are_absent() {
    let config = Config::parse(
        r#"
        [providers.openrouter]
        kind = "open_router"
        base_url = "https://openrouter.ai/api/v1"
        api_key_env = "OPENROUTER_API_KEY"

        [[ladders]]
        name = "only"
          [[ladders.rungs]]
          provider = "openrouter"
          model = "m"
        "#,
    )
    .unwrap();

    assert_eq!(config.server.bind, "127.0.0.1:6969");
    assert_eq!(
        config.pricing.refresh,
        std::time::Duration::from_secs(15 * 60)
    );
    assert_eq!(
        config.credits.refresh,
        std::time::Duration::from_secs(5 * 60)
    );
    assert!(config.credits.min_balance_usd.abs() < f64::EPSILON);
}

#[test]
fn the_tighter_of_the_rung_and_provider_ceilings_applies() {
    let config = Config::parse(EXAMPLE).unwrap();
    let flash = config.ladder("flash").unwrap();

    // Rung 0.15 against provider 0.50: the rung is tighter.
    assert_eq!(config.cap_for(&flash.rungs[0]), Some(0.15));
    // Rung 0.30 with no provider ceiling on OpenRouter.
    assert_eq!(config.cap_for(&flash.rungs[1]), Some(0.30));
}

#[test]
fn a_rung_without_a_ceiling_inherits_the_providers() {
    let config = Config::parse(
        r#"
        [providers.surplus]
        kind = "surplus"
        base_url = "https://api.surplusintelligence.ai"
        api_key_env = "SURPLUS_API_KEY"
        max_cost_per_1m = 0.40

        [[ladders]]
        name = "only"
          [[ladders.rungs]]
          provider = "surplus"
          model = "glm-5.2"
        "#,
    )
    .unwrap();
    let rung = &config.ladder("only").unwrap().rungs[0];
    assert_eq!(config.cap_for(rung), Some(0.40));
}

#[test]
fn a_rung_with_neither_ceiling_is_uncapped() {
    let config = Config::parse(EXAMPLE).unwrap();
    let reasoning = config.ladder("reasoning").unwrap();
    // Surplus carries a 0.50 provider ceiling, so this rung is not uncapped.
    assert_eq!(config.cap_for(&reasoning.rungs[0]), Some(0.50));

    let uncapped = Config::parse(
        r#"
        [providers.openrouter]
        kind = "open_router"
        base_url = "https://openrouter.ai/api/v1"
        api_key_env = "OPENROUTER_API_KEY"

        [[ladders]]
        name = "only"
          [[ladders.rungs]]
          provider = "openrouter"
          model = "m"
        "#,
    )
    .unwrap();
    assert_eq!(
        uncapped.cap_for(&uncapped.ladder("only").unwrap().rungs[0]),
        None
    );
}

#[test]
fn rejects_a_rung_naming_an_undefined_provider() {
    let error = Config::parse(
        r#"
        [providers.openrouter]
        kind = "open_router"
        base_url = "https://openrouter.ai/api/v1"
        api_key_env = "OPENROUTER_API_KEY"

        [[ladders]]
        name = "typo"
          [[ladders.rungs]]
          provider = "openrouetr"
          model = "m"
        "#,
    )
    .unwrap_err();

    match error {
        Error::UnknownProvider {
            ladder,
            rung,
            provider,
        } => {
            assert_eq!(ladder, "typo");
            assert_eq!(rung, 0);
            assert_eq!(provider, "openrouetr");
        }
        other => panic!("expected UnknownProvider, got {other:?}"),
    }
}

#[test]
fn rejects_ceilings_that_are_not_usable_amounts_of_money() {
    for bad in ["0.0", "-1.0", "nan", "inf"] {
        let text = format!(
            r#"
            [providers.openrouter]
            kind = "open_router"
            base_url = "https://openrouter.ai/api/v1"
            api_key_env = "OPENROUTER_API_KEY"

            [[ladders]]
            name = "only"
              [[ladders.rungs]]
              provider = "openrouter"
              model = "m"
              max_cost_per_1m = {bad}
            "#
        );
        let error = Config::parse(&text)
            .err()
            .unwrap_or_else(|| panic!("{bad} should be rejected"));
        assert!(
            matches!(error, Error::InvalidPrice { .. } | Error::ConfigParse(_)),
            "{bad} produced {error:?}"
        );
    }
}

#[test]
fn rejects_a_negative_provider_ceiling() {
    let error = Config::parse(
        r#"
        [providers.openrouter]
        kind = "open_router"
        base_url = "https://openrouter.ai/api/v1"
        api_key_env = "OPENROUTER_API_KEY"
        max_cost_per_1m = -0.5

        [[ladders]]
        name = "only"
          [[ladders.rungs]]
          provider = "openrouter"
          model = "m"
        "#,
    )
    .unwrap_err();
    assert!(matches!(error, Error::InvalidPrice { .. }), "{error:?}");
}

#[test]
fn rejects_duplicate_ladder_names() {
    let error = Config::parse(
        r#"
        [providers.openrouter]
        kind = "open_router"
        base_url = "https://openrouter.ai/api/v1"
        api_key_env = "OPENROUTER_API_KEY"

        [[ladders]]
        name = "same"
          [[ladders.rungs]]
          provider = "openrouter"
          model = "a"

        [[ladders]]
        name = "same"
          [[ladders.rungs]]
          provider = "openrouter"
          model = "b"
        "#,
    )
    .unwrap_err();
    assert!(matches!(error, Error::DuplicateLadder(name) if name == "same"));
}

#[test]
fn rejects_an_empty_ladder_list() {
    let error = Config::parse(
        r#"
        [providers.openrouter]
        kind = "open_router"
        base_url = "https://openrouter.ai/api/v1"
        api_key_env = "OPENROUTER_API_KEY"
        ladders = []
        "#,
    );
    assert!(error.is_err());
}

#[test]
fn rejects_a_ladder_with_no_rungs() {
    let error = Config::parse(
        r#"
        [providers.openrouter]
        kind = "open_router"
        base_url = "https://openrouter.ai/api/v1"
        api_key_env = "OPENROUTER_API_KEY"

        [[ladders]]
        name = "hollow"
        rungs = []
        "#,
    )
    .unwrap_err();
    assert!(matches!(error, Error::Empty { .. }), "{error:?}");
}

#[test]
fn rejects_an_unknown_field() {
    let error = Config::parse(
        r#"
        [providers.openrouter]
        kind = "open_router"
        base_url = "https://openrouter.ai/api/v1"
        api_key_env = "OPENROUTER_API_KEY"
        surprise = true

        [[ladders]]
        name = "only"
          [[ladders.rungs]]
          provider = "openrouter"
          model = "m"
        "#,
    )
    .unwrap_err();
    assert!(matches!(error, Error::ConfigParse(_)), "{error:?}");
}

#[test]
fn reports_the_path_when_the_file_is_missing() {
    let error = Config::load("/nonexistent/llm-ladder-router/config.toml").unwrap_err();
    match error {
        Error::ConfigRead { path, .. } => assert!(path.contains("config.toml")),
        other => panic!("expected ConfigRead, got {other:?}"),
    }
}

#[test]
fn the_shipped_example_config_is_valid() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.example.toml"))
        .unwrap();
    let config = Config::parse(&text).unwrap();

    // The example is the documentation for the two ladders the router ships
    // with; a change to either should be deliberate.
    assert_eq!(
        config
            .ladder("flash")
            .unwrap()
            .rungs
            .iter()
            .map(|rung| (rung.provider.as_str(), rung.model.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("surplus", "gpt-5.6-luna"),
            ("surplus", "deepseek-v4-flash"),
            ("openrouter", "deepseek/deepseek-v4-flash"),
        ]
    );

    assert_eq!(
        config
            .ladder("reasoning")
            .unwrap()
            .rungs
            .iter()
            .map(|rung| (rung.provider.as_str(), rung.model.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("surplus", "deepseek-v4-pro"),
            ("surplus", "glm-5.2"),
            ("surplus", "gpt-5.6-luna"),
            ("surplus", "deepseek-v4-flash"),
            ("openrouter", "deepseek/deepseek-v4-flash"),
        ]
    );
}
