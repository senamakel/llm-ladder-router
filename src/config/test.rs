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

    // Every interface, not loopback: the example is what the container image
    // ships, and a container that binds loopback answers nobody.
    assert_eq!(config.server.bind, "0.0.0.0:6969");

    // The example is the documentation for the three ladders the router ships
    // with; a change to any of them should be deliberate.
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

    let max = config.ladder("max-reasoning").unwrap();
    assert_eq!(
        max.rungs
            .iter()
            .map(|rung| (rung.provider.as_str(), rung.model.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("surplus", "deepseek-v4-pro"),
            ("surplus", "glm-5.2"),
            ("surplus", "gpt-5.6-luna"),
            ("openrouter", "deepseek/deepseek-v4-pro"),
        ],
        "no rung of the deepest ladder may be a fast model"
    );

    // Every rung asks for depth, and the effort each one asks for is the one
    // its model family accepts.
    assert_eq!(max.effort_for(&max.rungs[0]).as_deref(), Some("high"));
    assert_eq!(max.effort_for(&max.rungs[2]).as_deref(), Some("xhigh"));

    // The provider ceilings must not clamp it. The tighter of the two wins, so
    // a marketplace ceiling below a rung's own would silently undo the price
    // this ladder was written to pay — and the ladder would step down to a
    // cheaper model while reading as though it had not.
    for rung in &max.rungs {
        let cap = config.cap_for(rung).unwrap();
        assert!(
            (cap - rung.max_cost_per_1m.unwrap()).abs() < f64::EPSILON,
            "`{}` is clamped to {cap} by its provider's ceiling",
            rung.model
        );
    }
}

/// A ladder-level effort reaches every rung, and a rung's own overrides it.
///
/// Both halves matter: without the default a max-reasoning ladder would have to
/// repeat itself on every rung, and without the override it could not say
/// `xhigh` on the one model family that accepts it.
#[test]
fn reasoning_effort_defaults_down_the_ladder_and_a_rung_overrides_it() {
    let config = Config::parse(
        r#"
        [providers.surplus]
        kind = "surplus"
        base_url = "https://api.surplusintelligence.ai"
        api_key_env = "SURPLUS_API_KEY"

        [[ladders]]
        name = "max-reasoning"
        reasoning_effort = "high"
          [[ladders.rungs]]
          provider = "surplus"
          model = "deepseek-v4-pro"
          [[ladders.rungs]]
          provider = "surplus"
          model = "gpt-5.6-luna"
          reasoning_effort = "xhigh"
        "#,
    )
    .unwrap();

    let ladder = config.ladder("max-reasoning").unwrap();
    assert_eq!(ladder.effort_for(&ladder.rungs[0]).as_deref(), Some("high"));
    assert_eq!(
        ladder.effort_for(&ladder.rungs[1]).as_deref(),
        Some("xhigh")
    );
}

/// A ladder that declares nothing asks for nothing, so every ladder written
/// before this field behaves exactly as it did.
#[test]
fn a_ladder_without_an_effort_asks_for_none() {
    let config = Config::parse(EXAMPLE).unwrap();
    let flash = config.ladder("flash").unwrap();
    assert!(flash.effort_for(&flash.rungs[0]).is_none());
}

/// A blank effort is a configuration mistake, not "unset": relayed as an empty
/// string it is a 400 the failover loop attributes to the caller, so the ladder
/// would stop rather than step down.
#[test]
fn a_blank_reasoning_effort_is_rejected() {
    let error = Config::parse(
        r#"
        [providers.surplus]
        kind = "surplus"
        base_url = "https://api.surplusintelligence.ai"
        api_key_env = "SURPLUS_API_KEY"

        [[ladders]]
        name = "max-reasoning"
          [[ladders.rungs]]
          provider = "surplus"
          model = "deepseek-v4-pro"
          reasoning_effort = "  "
        "#,
    )
    .unwrap_err();

    assert!(
        matches!(&error, Error::Empty { what } if what.contains("reasoning_effort")),
        "unexpected error: {error}"
    );
}
