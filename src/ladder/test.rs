//! Unit tests for the selection engine.
//!
//! Every test here runs without a network: the engine takes prices and balances
//! as arguments precisely so the routing policy can be pinned down exactly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::CostBasis;
use crate::pricing::{ModelPrices, Offer};

/// Four rungs across both providers, mirroring the shipped reasoning ladder.
const CONFIG: &str = r#"
[credits]
min_balance_usd = 1.0

[providers.surplus]
kind = "surplus"
base_url = "https://api.surplusintelligence.ai"
api_key_env = "SURPLUS_API_KEY"

[providers.openrouter]
kind = "open_router"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"

[[ladders]]
name = "reasoning"

  [[ladders.rungs]]
  provider = "surplus"
  model = "deepseek-v4-pro"
  max_cost_per_1m = 0.30

  [[ladders.rungs]]
  provider = "surplus"
  model = "glm-5.2"
  max_cost_per_1m = 0.30

  [[ladders.rungs]]
  provider = "surplus"
  model = "deepseek-v4-flash"
  max_cost_per_1m = 0.15

  [[ladders.rungs]]
  provider = "openrouter"
  model = "deepseek/deepseek-v4-flash"
  max_cost_per_1m = 0.30
"#;

fn config() -> Config {
    Config::parse(CONFIG).unwrap()
}

fn offer(provider: &str, completion_per_1m: f64) -> Offer {
    Offer {
        provider: provider.to_string(),
        tag: None,
        prompt_per_1m: completion_per_1m / 2.0,
        completion_per_1m,
        direct_completion_per_1m: Some(3.74),
        usable: true,
    }
}

/// A price table where every rung's model has one offer at the given price.
fn prices(entries: &[(&str, &str, f64)]) -> PriceTable {
    let mut table = PriceTable::new();
    for (provider, model, price) in entries {
        table.insert(
            *provider,
            *model,
            ModelPrices::new(vec![offer("seller", *price)]),
        );
    }
    table
}

/// Balances high enough that no rung is skipped for being broke.
fn funded() -> CreditState {
    let mut credits = CreditState::new();
    credits.set_balance("surplus", 100.0);
    credits.set_balance("openrouter", 100.0);
    credits
}

#[test]
fn picks_the_first_rung_whose_sellers_fit() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let prices = prices(&[
        ("surplus", "deepseek-v4-pro", 0.10),
        ("surplus", "glm-5.2", 0.05),
    ]);

    let selection = select(&config, ladder, &prices, &funded(), &[]);
    let chosen = selection.chosen.unwrap();

    assert_eq!(chosen.rung, 0);
    assert_eq!(chosen.model, "deepseek-v4-pro");
    assert!(selection.skipped.is_empty());
}

#[test]
fn steps_down_when_no_seller_is_under_the_ceiling() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    // Pro's cheapest is 0.63 against a 0.30 ceiling; GLM fits.
    let prices = prices(&[
        ("surplus", "deepseek-v4-pro", 0.63),
        ("surplus", "glm-5.2", 0.09),
    ]);

    let selection = select(&config, ladder, &prices, &funded(), &[]);

    assert_eq!(selection.chosen.unwrap().rung, 1);
    assert_eq!(selection.skipped.len(), 1);
    match &selection.skipped[0].reason {
        SkipReason::NoSellerUnderCap { cap_per_1m, cheapest_per_1m } => {
            assert!((cap_per_1m - 0.30).abs() < f64::EPSILON);
            assert_eq!(*cheapest_per_1m, Some(0.63));
        }
        other => panic!("expected NoSellerUnderCap, got {other:?}"),
    }
}

#[test]
fn walks_every_rung_of_a_four_rung_ladder_in_order() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    // Only the last rung fits; the three Surplus rungs are all too expensive.
    let prices = prices(&[
        ("surplus", "deepseek-v4-pro", 9.0),
        ("surplus", "glm-5.2", 9.0),
        ("surplus", "deepseek-v4-flash", 9.0),
        ("openrouter", "deepseek/deepseek-v4-flash", 0.14),
    ]);

    let selection = select(&config, ladder, &prices, &funded(), &[]);
    let chosen = selection.chosen.unwrap();

    assert_eq!(chosen.rung, 3);
    assert_eq!(chosen.provider, "openrouter");
    assert_eq!(
        selection.skipped.iter().map(|skip| skip.rung).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[test]
fn reports_every_rung_when_none_can_serve() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let prices = prices(&[
        ("surplus", "deepseek-v4-pro", 9.0),
        ("surplus", "glm-5.2", 9.0),
        ("surplus", "deepseek-v4-flash", 9.0),
        ("openrouter", "deepseek/deepseek-v4-flash", 9.0),
    ]);

    let selection = select(&config, ladder, &prices, &funded(), &[]);

    assert!(selection.chosen.is_none());
    assert_eq!(selection.skipped.len(), 4);
}

#[test]
fn excluded_rungs_are_not_reconsidered() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let prices = prices(&[
        ("surplus", "deepseek-v4-pro", 0.10),
        ("surplus", "glm-5.2", 0.10),
        ("surplus", "deepseek-v4-flash", 0.10),
    ]);

    // Rung 0 was already tried and failed upstream.
    let selection = select(&config, ladder, &prices, &funded(), &[0]);
    assert_eq!(selection.chosen.unwrap().rung, 1);

    let selection = select(&config, ladder, &prices, &funded(), &[0, 1]);
    assert_eq!(selection.chosen.unwrap().rung, 2);
}

#[test]
fn skips_a_rung_whose_provider_balance_is_spent() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let prices = prices(&[
        ("surplus", "deepseek-v4-pro", 0.01),
        ("openrouter", "deepseek/deepseek-v4-flash", 0.20),
    ]);

    let mut credits = CreditState::new();
    credits.set_balance("surplus", 0.10);
    credits.set_balance("openrouter", 50.0);

    let selection = select(&config, ladder, &prices, &credits, &[]);

    assert_eq!(selection.chosen.unwrap().provider, "openrouter");
    assert!(matches!(
        selection.skipped[0].reason,
        SkipReason::ExhaustedBalance { .. }
    ));
}

#[test]
fn skips_a_rung_whose_credential_is_missing() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let prices = prices(&[
        ("surplus", "deepseek-v4-pro", 0.01),
        ("openrouter", "deepseek/deepseek-v4-flash", 0.20),
    ]);

    let mut credits = CreditState::new();
    credits.set_missing_credential("surplus", "SURPLUS_API_KEY");
    credits.set_balance("openrouter", 50.0);

    let selection = select(&config, ladder, &prices, &credits, &[]);

    assert_eq!(selection.chosen.unwrap().provider, "openrouter");
    match &selection.skipped[0].reason {
        SkipReason::MissingCredential { variable } => assert_eq!(variable, "SURPLUS_API_KEY"),
        other => panic!("expected MissingCredential, got {other:?}"),
    }
}

#[test]
fn skips_a_capped_rung_with_no_price_data() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    // Only the final rung has prices at all.
    let prices = prices(&[("openrouter", "deepseek/deepseek-v4-flash", 0.20)]);

    let selection = select(&config, ladder, &prices, &funded(), &[]);

    assert_eq!(selection.chosen.unwrap().rung, 3);
    assert!(
        selection
            .skipped
            .iter()
            .all(|skip| skip.reason == SkipReason::NoPriceData)
    );
}

#[test]
fn tries_an_uncapped_rung_even_without_price_data() {
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
    let ladder = config.ladder("only").unwrap();

    let selection = select(&config, ladder, &PriceTable::new(), &CreditState::new(), &[]);
    let chosen = selection.chosen.unwrap();

    // Without a ceiling there is nothing to check prices against, so refusing
    // to try would be a needless failure.
    assert_eq!(chosen.rung, 0);
    assert_eq!(chosen.cap_per_1m, None);
}

#[test]
fn skips_a_capped_rung_whose_prices_are_stale() {
    let mut config = config();
    config.pricing.stale_after = std::time::Duration::ZERO;
    let ladder = config.ladder("reasoning").unwrap().clone();
    let prices = prices(&[
        ("surplus", "deepseek-v4-pro", 0.01),
        ("surplus", "glm-5.2", 0.01),
        ("surplus", "deepseek-v4-flash", 0.01),
        ("openrouter", "deepseek/deepseek-v4-flash", 0.01),
    ]);
    // A zero tolerance makes every snapshot stale as soon as any time passes.
    std::thread::sleep(std::time::Duration::from_millis(2));

    let selection = select(&config, &ladder, &prices, &funded(), &[]);

    assert!(selection.chosen.is_none());
    assert!(
        selection
            .skipped
            .iter()
            .all(|skip| skip.reason == SkipReason::StalePriceData)
    );
}

#[test]
fn an_unusable_offer_never_fits() {
    let mut table = PriceTable::new();
    let mut down = offer("seller", 0.01);
    down.usable = false;
    table.insert("surplus", "deepseek-v4-pro", ModelPrices::new(vec![down]));

    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let selection = select(&config, ladder, &table, &funded(), &[]);

    // Priced far below the ceiling, but the marketplace reports it as down.
    assert!(matches!(
        selection.skipped[0].reason,
        SkipReason::NoSellerUnderCap { .. }
    ));
}

#[test]
fn admitted_sub_providers_are_reported_cheapest_first() {
    let mut table = PriceTable::new();
    table.insert(
        "openrouter",
        "deepseek/deepseek-v4-flash",
        ModelPrices::new(vec![
            offer("expensive", 0.28),
            offer("cheap", 0.14),
            offer("dearest", 0.90),
        ]),
    );

    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let selection = select(&config, ladder, &table, &funded(), &[0, 1, 2]);
    let chosen = selection.chosen.unwrap();

    // 0.90 is above the 0.30 ceiling and must not appear.
    assert_eq!(chosen.admitted, vec!["cheap", "expensive"]);
    assert_eq!(chosen.cheapest_per_1m, Some(0.14));
}

#[test]
fn the_cost_basis_decides_which_price_is_compared() {
    let text = CONFIG.replace("name = \"reasoning\"", "name = \"reasoning\"\ncost_basis = \"prompt\"");
    let config = Config::parse(&text).unwrap();
    let ladder = config.ladder("reasoning").unwrap();

    // `offer` sets the prompt price to half the completion price, so a
    // completion price of 0.50 is above the 0.30 ceiling while its prompt
    // price of 0.25 is below it.
    let prices = prices(&[("surplus", "deepseek-v4-pro", 0.50)]);
    let selection = select(&config, ladder, &prices, &funded(), &[]);

    assert_eq!(selection.chosen.unwrap().rung, 0);
}

#[test]
fn a_ceiling_is_restated_as_the_equivalent_discount() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let prices = prices(&[("surplus", "glm-5.2", 0.05)]);

    let selection = select(&config, ladder, &prices, &funded(), &[0]);
    let chosen = selection.chosen.unwrap();

    // A 0.30 ceiling against a 3.74 direct price is a 91.97% discount, floored
    // to 91 so the filter never asks for more than the ceiling implies.
    assert_eq!(chosen.min_discount_pct, Some(91));
}

#[test]
fn the_discount_never_reaches_the_hundred_that_matches_nothing() {
    let prices = ModelPrices::new(vec![Offer {
        provider: "seller".to_string(),
        tag: None,
        prompt_per_1m: 0.0,
        completion_per_1m: 0.0,
        direct_completion_per_1m: Some(3.74),
        usable: true,
    }]);

    // A ceiling of zero would imply a 100% discount, which Surplus rejects
    // outright and which would make an affordable rung unreachable.
    assert_eq!(prices.discount_floor_pct(0.0, CostBasis::Completion), Some(99));
}
