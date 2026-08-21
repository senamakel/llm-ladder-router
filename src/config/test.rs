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

    // The example is the documentation for the four ladders the router ships
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

    // One rung, one seller, no ceiling: "this model or nothing".
    let scribe = config.ladder("scribe").unwrap();
    assert_eq!(scribe.rungs.len(), 1);
    assert_eq!(scribe.rungs[0].model, "labs-leanstral-1-5");
    assert!(config.cap_for(&scribe.rungs[0]).is_none());
    assert!(!config.providers["mistral"].kind.is_marketplace());

    // Two rungs, one model: the priced one first, the house it comes from
    // behind it as the rung that cannot be outbid, only fallen back to.
    let uncensored = config.ladder("uncensored").unwrap();
    assert_eq!(
        uncensored
            .rungs
            .iter()
            .map(|rung| (rung.provider.as_str(), rung.model.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("surplus", "venice-uncensored-1.2"),
            ("venice", "venice-uncensored-1.2"),
        ]
    );
    assert!(config.cap_for(&uncensored.rungs[1]).is_none());
    assert!(!config.providers["venice"].kind.is_marketplace());

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
    // its model family accepts. Named on the rung rather than inherited, so
    // that reading a rung tells you what it will send.
    assert_eq!(
        max.rungs
            .iter()
            .map(|rung| max.effort_for(rung))
            .collect::<Vec<_>>(),
        ["high", "high", "xhigh", "high"]
            .map(|effort| Some(effort.to_string()))
            .to_vec()
    );
    for rung in &max.rungs {
        assert!(
            rung.reasoning_effort.is_some(),
            "`{}` leaves its depth to the ladder default",
            rung.model
        );
    }

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

/// A rate limit parks a rung for as long as the upstream asked, and for the
/// configured default when it asked for nothing.
#[test]
fn the_upstream_backoff_wins_and_the_default_fills_in() {
    let limits = RateLimits::default();

    let asked = limits.cooldown_for(Some(std::time::Duration::from_secs(90)));
    assert_eq!(asked.duration, std::time::Duration::from_secs(90));
    assert!(asked.requested);

    let silent = limits.cooldown_for(None);
    assert_eq!(silent.duration, limits.cooldown);
    assert!(!silent.requested);
}

/// A header asking for an hour would take a rung out of its ladder for an hour
/// on one busy minute, and a ladder whose good rungs are parked serves from its
/// worst.
#[test]
fn an_outsized_backoff_is_clamped() {
    let limits = RateLimits::default();
    let cooled = limits.cooldown_for(Some(std::time::Duration::from_secs(3600)));

    assert_eq!(cooled.duration, limits.max_cooldown);
}

/// A direct provider publishes no order book, so a ceiling on it can never be
/// checked and every rung under it would be skipped for missing price data —
/// a ladder that silently serves nothing.
#[test]
fn a_ceiling_on_a_direct_provider_is_refused() {
    let with_provider_cap = Config::parse(
        r#"
        [providers.mistral]
        kind = "mistral"
        base_url = "https://api.mistral.ai"
        api_key_env = "MISTRAL_API_KEY"
        max_cost_per_1m = 0.50

        [[ladders]]
        name = "scribe"
          [[ladders.rungs]]
          provider = "mistral"
          model = "labs-leanstral-1-5"
        "#,
    )
    .unwrap_err();
    assert!(
        matches!(&with_provider_cap, Error::UnpriceableCeiling { provider, .. } if provider == "mistral"),
        "unexpected error: {with_provider_cap}"
    );

    let with_rung_cap = Config::parse(
        r#"
        [providers.mistral]
        kind = "mistral"
        base_url = "https://api.mistral.ai"
        api_key_env = "MISTRAL_API_KEY"

        [[ladders]]
        name = "scribe"
          [[ladders.rungs]]
          provider = "mistral"
          model = "labs-leanstral-1-5"
          max_cost_per_1m = 0.50
        "#,
    )
    .unwrap_err();
    assert!(
        matches!(&with_rung_cap, Error::UnpriceableCeiling { field, .. } if field.contains("rung 0")),
        "unexpected error: {with_rung_cap}"
    );
}

/// The price machinery applies to a marketplace and is meaningless against a
/// direct endpoint; asking once here is what keeps every caller from growing
/// its own match.
#[test]
fn only_the_marketplaces_are_marketplaces() {
    assert!(ProviderKind::OpenRouter.is_marketplace());
    assert!(ProviderKind::Surplus.is_marketplace());
    assert!(!ProviderKind::Mistral.is_marketplace());
    assert!(!ProviderKind::Venice.is_marketplace());
}

/// Venice is direct too, so the same refusal applies to a ceiling on it.
#[test]
fn a_ceiling_on_venice_is_refused() {
    let error = Config::parse(
        r#"
        [providers.venice]
        kind = "venice"
        base_url = "https://api.venice.ai"
        api_key_env = "LADDER_TEST_UNSET_KEY"

        [[ladders]]
        name = "uncensored"
          [[ladders.rungs]]
          provider = "venice"
          model = "venice-uncensored-1.2"
          max_cost_per_1m = 0.50
        "#,
    )
    .unwrap_err();
    assert!(
        matches!(&error, Error::UnpriceableCeiling { provider, .. } if provider == "venice"),
        "unexpected error: {error}"
    );
}

/// The shipped multipliers must express the ladders' intent, not just parse.
///
/// `max-reasoning` is the one that would fail silently: with mild multipliers a
/// cheap seller on the weakest rung outranks the strongest model and the ladder
/// quietly stops being about depth, while every test about *parsing* still
/// passes.
#[test]
fn the_shipped_multipliers_keep_each_ladder_about_what_it_is_for() {
    fn multipliers(ladder: &Ladder) -> Vec<f64> {
        ladder
            .rungs
            .iter()
            .map(Rung::effective_score_multiplier)
            .collect()
    }

    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config.example.toml"))
        .unwrap();
    let config = Config::parse(&text).unwrap();

    let max = config.ladder("max-reasoning").unwrap();
    let pro = max.rungs[0].effective_score_multiplier();
    let weakest = multipliers(max).into_iter().fold(f64::INFINITY, f64::min);
    assert!(
        pro / weakest >= 4.0,
        "the deepest ladder tolerates only a {}x premium for its strongest \
         model, so a cheap seller on a weaker rung would win it",
        pro / weakest
    );

    // The cheap ladder is the opposite bet: any of these will do, so nothing
    // should be able to outrank price by much.
    let flash = multipliers(config.ladder("flash").unwrap());
    let spread = flash.iter().copied().fold(f64::NEG_INFINITY, f64::max)
        / flash.iter().copied().fold(f64::INFINITY, f64::min);
    assert!(
        spread <= 2.0,
        "the throughput ladder has grown a {spread}x quality preference"
    );
}
