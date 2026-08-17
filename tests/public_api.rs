//! Integration tests against the public API only.
//!
//! These exercise the crate the way a downstream consumer would: build a
//! configuration, feed it prices and balances, and check the routing decision.
//! The wire-level behavior lives in `ladder_proxy.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use llm_ladder_router::{
    Config, CreditState, Error, ModelPrices, Offer, PriceTable, SkipReason, select,
};

const CONFIG: &str = r#"
[providers.surplus]
kind = "surplus"
base_url = "https://api.surplusintelligence.ai"
api_key_env = "SURPLUS_API_KEY"
max_cost_per_1m = 0.50

[providers.openrouter]
kind = "open_router"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"

[[ladders]]
name = "flash"

  [[ladders.rungs]]
  provider = "surplus"
  model = "deepseek-v4-flash"
  max_cost_per_1m = 0.15

  [[ladders.rungs]]
  provider = "openrouter"
  model = "deepseek/deepseek-v4-flash"
  max_cost_per_1m = 0.30
"#;

fn offer(name: &str, completion_per_1m: f64) -> Offer {
    Offer {
        provider: name.to_string(),
        tag: None,
        prompt_per_1m: completion_per_1m / 2.0,
        completion_per_1m,
        direct_completion_per_1m: Some(0.1652),
        usable: true,
    }
}

fn funded() -> CreditState {
    let mut credits = CreditState::new();
    credits.set_balance("surplus", 74.67);
    credits.set_balance("openrouter", 12.0);
    credits
}

#[test]
fn a_ladder_prefers_its_first_affordable_rung() {
    let config = Config::parse(CONFIG).unwrap();
    let ladder = config.ladder("flash").unwrap();

    let mut prices = PriceTable::new();
    prices.insert(
        "surplus",
        "deepseek-v4-flash",
        ModelPrices::new(vec![offer("Z.ai", 0.108)]),
    );

    let chosen = select(&config, ladder, &prices, &funded(), &[])
        .chosen
        .unwrap();
    assert_eq!(chosen.provider, "surplus");
    assert_eq!(chosen.rung, 0);
}

#[test]
fn a_ladder_falls_through_to_the_backstop_when_the_cheap_rung_is_priced_out() {
    let config = Config::parse(CONFIG).unwrap();
    let ladder = config.ladder("flash").unwrap();

    let mut prices = PriceTable::new();
    // Above the 0.15 ceiling.
    prices.insert(
        "surplus",
        "deepseek-v4-flash",
        ModelPrices::new(vec![offer("Z.ai", 0.40)]),
    );
    prices.insert(
        "openrouter",
        "deepseek/deepseek-v4-flash",
        ModelPrices::new(vec![offer("DeepInfra", 0.1372)]),
    );

    let selection = select(&config, ladder, &prices, &funded(), &[]);
    assert_eq!(selection.chosen.unwrap().provider, "openrouter");
    assert!(matches!(
        selection.skipped[0].reason,
        SkipReason::NoSellerUnderCap { .. }
    ));
}

#[test]
fn the_provider_ceiling_binds_a_rung_that_asked_for_more() {
    let config = Config::parse(
        r#"
        [providers.surplus]
        kind = "surplus"
        base_url = "https://api.surplusintelligence.ai"
        api_key_env = "SURPLUS_API_KEY"
        max_cost_per_1m = 0.20

        [[ladders]]
        name = "only"
          [[ladders.rungs]]
          provider = "surplus"
          model = "glm-5.2"
          max_cost_per_1m = 5.00
        "#,
    )
    .unwrap();
    let ladder = config.ladder("only").unwrap();

    // The rung asked for 5.00 but the provider caps everything at 0.20.
    assert_eq!(config.cap_for(&ladder.rungs[0]), Some(0.20));

    let mut prices = PriceTable::new();
    prices.insert(
        "surplus",
        "glm-5.2",
        ModelPrices::new(vec![offer("Z.ai", 0.90)]),
    );

    let selection = select(&config, ladder, &prices, &funded(), &[]);
    assert!(
        selection.chosen.is_none(),
        "0.90 is above the provider ceiling of 0.20"
    );
}

#[test]
fn an_unknown_ladder_is_not_found() {
    let config = Config::parse(CONFIG).unwrap();
    assert!(config.ladder("nonexistent").is_none());
}

#[test]
fn an_invalid_configuration_is_rejected_with_a_specific_error() {
    let error = Config::parse(
        r#"
        [providers.openrouter]
        kind = "open_router"
        base_url = "https://openrouter.ai/api/v1"
        api_key_env = "OPENROUTER_API_KEY"

        [[ladders]]
        name = "broken"
          [[ladders.rungs]]
          provider = "missing"
          model = "m"
        "#,
    )
    .unwrap_err();
    assert!(matches!(error, Error::UnknownProvider { .. }));
}
