//! Unit tests for the selection engine.
//!
//! Every test here runs without a network: the engine takes prices and balances
//! as arguments precisely so the routing policy can be pinned down exactly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::cooldown::Cooldowns;
use crate::pricing::{ModelPrices, Offer};

/// The pinned selection with nothing cooling down, which is what every test
/// below wants unless it is about cooldowns. Shadows the real function, so a
/// test reads as the policy question it is asking rather than as an argument
/// list; the cooldown tests call [`super::select_pinned`] directly.
fn select_pinned(
    config: &Config,
    ladder: &Ladder,
    prices: &PriceTable,
    credits: &CreditState,
    exclude: &[usize],
    pin: Option<&Pin>,
) -> Selection {
    super::select_pinned(
        config,
        ladder,
        prices,
        credits,
        &Cooldowns::new(),
        exclude,
        pin,
    )
}

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

/// Ladder order is not precedence: the cheapest affordable rung wins, even when
/// an earlier rung would also have served.
#[test]
fn picks_the_cheapest_rung_that_fits_rather_than_the_first() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let prices = prices(&[
        ("surplus", "deepseek-v4-pro", 0.10),
        ("surplus", "glm-5.2", 0.05),
    ]);

    let selection = select(&config, ladder, &prices, &funded(), &[]);
    let chosen = selection.chosen.unwrap();

    assert_eq!(chosen.rung, 1);
    assert_eq!(chosen.model, "glm-5.2");
    // With every multiplier at 1.0 the score is just the price.
    assert_eq!(chosen.score, Some(0.05));
    // Rung 0 could have served and was outbid, so it is not a skip: only the
    // two rungs with no price data are.
    assert_eq!(
        selection
            .skipped
            .iter()
            .map(|skip| skip.rung)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
}

/// A multiplier is what lets a dearer rung win: at 2.0, pro is worth paying for
/// up to twice the price of a baseline rung.
#[test]
fn a_multiplier_lets_a_dearer_rung_outrank_a_cheaper_one() {
    let config = Config::parse(&CONFIG.replace(
        r#"  model = "deepseek-v4-pro"
  max_cost_per_1m = 0.30"#,
        r#"  model = "deepseek-v4-pro"
  max_cost_per_1m = 0.30
  score_multiplier = 2.0"#,
    ))
    .unwrap();
    let ladder = config.ladder("reasoning").unwrap();

    // Pro at 0.10 scores 0.05 against GLM's 0.09, so the dearer model wins.
    let selection = select(
        &config,
        ladder,
        &prices(&[
            ("surplus", "deepseek-v4-pro", 0.10),
            ("surplus", "glm-5.2", 0.09),
        ]),
        &funded(),
        &[],
    );
    let chosen = selection.chosen.unwrap();
    assert_eq!(chosen.model, "deepseek-v4-pro");
    assert_eq!(chosen.score, Some(0.05));
    assert!((chosen.score_multiplier - 2.0).abs() < f64::EPSILON);

    // Past twice the price the multiplier no longer covers it, and the same
    // ladder takes the cheaper model instead. This is the whole policy.
    let selection = select(
        &config,
        ladder,
        &prices(&[
            ("surplus", "deepseek-v4-pro", 0.20),
            ("surplus", "glm-5.2", 0.09),
        ]),
        &funded(),
        &[],
    );
    assert_eq!(selection.chosen.unwrap().model, "glm-5.2");
}

/// Equal scores fall back to ladder order, which is the only remaining thing a
/// reader can predict.
#[test]
fn an_exact_tie_falls_back_to_ladder_order() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let selection = select(
        &config,
        ladder,
        &prices(&[
            ("surplus", "deepseek-v4-pro", 0.07),
            ("surplus", "glm-5.2", 0.07),
        ]),
        &funded(),
        &[],
    );

    assert_eq!(selection.chosen.unwrap().rung, 0);
}

/// An unpriced rung is not a cheap one. It cannot be ranked, so it waits behind
/// every rung that can be — otherwise a missing price snapshot would read as a
/// score of zero and win every request.
#[test]
fn an_unpriced_rung_ranks_behind_every_priced_one() {
    let config = Config::parse(&CONFIG.replace(
        r#"  model = "deepseek-v4-pro"
  max_cost_per_1m = 0.30"#,
        r#"  model = "deepseek-v4-pro""#,
    ))
    .unwrap();
    let ladder = config.ladder("reasoning").unwrap();

    // Rung 0 is uncapped and unpriced, so it is admissible but unscored.
    let selection = select(
        &config,
        ladder,
        &prices(&[("surplus", "glm-5.2", 0.29)]),
        &funded(),
        &[],
    );
    let chosen = selection.chosen.unwrap();
    assert_eq!(chosen.model, "glm-5.2");

    // With nothing priced it is the last resort rather than no answer at all.
    let selection = select(&config, ladder, &prices(&[]), &funded(), &[]);
    let chosen = selection.chosen.unwrap();
    assert_eq!(chosen.rung, 0);
    assert_eq!(chosen.score, None);
}

/// A rung priced out of its own ceiling is a skip, and says what it cost.
#[test]
fn a_rung_over_its_ceiling_is_skipped_with_both_figures() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    // Pro's cheapest is 0.63 against a 0.30 ceiling; GLM fits.
    let prices = prices(&[
        ("surplus", "deepseek-v4-pro", 0.63),
        ("surplus", "glm-5.2", 0.09),
    ]);

    let selection = select(&config, ladder, &prices, &funded(), &[]);

    assert_eq!(selection.chosen.unwrap().rung, 1);
    let priced_out = selection
        .skipped
        .iter()
        .find(|skip| skip.rung == 0)
        .expect("rung 0 is skipped");
    match &priced_out.reason {
        SkipReason::NoSellerUnderCap {
            cap_per_1m,
            cheapest_per_1m,
        } => {
            assert!((cap_per_1m - 0.30).abs() < f64::EPSILON);
            assert_eq!(*cheapest_per_1m, Some(0.63));
        }
        other => panic!("expected NoSellerUnderCap, got {other:?}"),
    }
}

#[test]
fn ranks_every_rung_and_takes_the_only_affordable_one() {
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
        selection
            .skipped
            .iter()
            .map(|skip| skip.rung)
            .collect::<Vec<_>>(),
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

    let selection = select(
        &config,
        ladder,
        &PriceTable::new(),
        &CreditState::new(),
        &[],
    );
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
    let text = CONFIG.replace(
        "name = \"reasoning\"",
        "name = \"reasoning\"\ncost_basis = \"prompt\"",
    );
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
    assert_eq!(prices.discount_floor_pct(0.0), Some(99));
}

// --- How a routing decision explains itself -------------------------------
// These messages are what a 502 shows an operator, so each one is pinned.

#[test]
fn a_missing_credential_names_the_variable() {
    let reason = SkipReason::MissingCredential {
        variable: "SURPLUS_API_KEY".to_string(),
    };
    assert_eq!(reason.to_string(), "credential SURPLUS_API_KEY is unset");
}

#[test]
fn an_exhausted_balance_names_both_figures() {
    let reason = SkipReason::ExhaustedBalance {
        remaining_usd: 0.1,
        floor_usd: 0.5,
    };
    assert_eq!(reason.to_string(), "balance $0.10 is below the $0.50 floor");
}

#[test]
fn missing_and_stale_price_data_read_differently() {
    assert_eq!(SkipReason::NoPriceData.to_string(), "no price data");
    assert_eq!(
        SkipReason::StalePriceData.to_string(),
        "price data is stale"
    );
}

#[test]
fn a_priced_out_rung_names_the_ceiling_and_the_cheapest_seller() {
    let reason = SkipReason::NoSellerUnderCap {
        cap_per_1m: 0.3,
        cheapest_per_1m: Some(0.63),
    };
    // Knowing the floor was 0.63 against a 0.30 ceiling is what makes the
    // decision reviewable.
    assert_eq!(
        reason.to_string(),
        "no seller under $0.3/Mtok; cheapest is $0.63/Mtok"
    );
}

#[test]
fn a_rung_with_no_usable_seller_at_all_says_so() {
    let reason = SkipReason::NoSellerUnderCap {
        cap_per_1m: 0.3,
        cheapest_per_1m: None,
    };
    assert_eq!(reason.to_string(), "no usable seller under $0.3/Mtok");
}

#[test]
fn an_upstream_failure_carries_the_detail_through() {
    let reason = SkipReason::UpstreamFailed {
        detail: "503 all sellers unhealthy".to_string(),
    };
    assert_eq!(
        reason.to_string(),
        "upstream failed: 503 all sellers unhealthy"
    );
}

// --- Branches a validated configuration cannot reach ----------------------

#[test]
fn a_rung_naming_a_provider_the_config_lacks_is_skipped() {
    // Configuration validation rejects this, so it is only reachable by
    // assembling a `Config` by hand. The engine must still not panic.
    let mut config = config();
    let ladder = config.ladder("reasoning").unwrap().clone();
    config.providers.remove("surplus");

    let selection = select(&config, &ladder, &PriceTable::new(), &funded(), &[]);

    assert!(selection.chosen.is_none());
    // The three Surplus rungs lose their provider; the OpenRouter rung is
    // capped and has no prices, so nothing can serve.
    assert_eq!(selection.skipped.len(), 4);
    assert_eq!(
        selection
            .skipped
            .iter()
            .filter(|skip| matches!(skip.reason, SkipReason::MissingCredential { .. }))
            .count(),
        3
    );
}

#[test]
fn an_uncapped_rung_whose_sellers_are_all_down_is_skipped() {
    let config = Config::parse(
        r#"
        [providers.openrouter]
        kind = "openrouter"
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

    let mut down = offer("seller", 0.01);
    down.usable = false;
    let mut table = PriceTable::new();
    table.insert("openrouter", "m", ModelPrices::new(vec![down]));

    let selection = select(&config, ladder, &table, &CreditState::new(), &[]);

    // No ceiling, but nobody can serve: that is a failure to try, not a price
    // decision, and it must not be reported as a chosen rung.
    assert!(selection.chosen.is_none());
    match &selection.skipped[0].reason {
        SkipReason::NoSellerUnderCap {
            cap_per_1m,
            cheapest_per_1m,
        } => {
            assert!(cap_per_1m.is_infinite());
            assert_eq!(*cheapest_per_1m, None);
        }
        other => panic!("expected NoSellerUnderCap, got {other:?}"),
    }
}

// --- Session pinning -------------------------------------------------------

use crate::session::{Pin, PinRejected};

fn a_pin(rung: usize, cap: Option<f64>, sub_provider: Option<&str>) -> Pin {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let rung_config = &ladder.rungs[rung];
    Pin {
        ladder: "reasoning".to_string(),
        rung,
        provider: rung_config.provider.clone(),
        model: rung_config.model.clone(),
        sub_provider: sub_provider.map(str::to_string),
        cap_per_1m: cap,
        pinned_at: std::time::Instant::now(),
    }
}

/// Prices where every rung of the reasoning ladder fits comfortably.
fn all_affordable() -> PriceTable {
    prices(&[
        ("surplus", "deepseek-v4-pro", 0.05),
        ("surplus", "glm-5.2", 0.05),
        ("surplus", "deepseek-v4-flash", 0.05),
        ("openrouter", "deepseek/deepseek-v4-flash", 0.05),
    ])
}

#[test]
fn a_pinned_session_stays_on_its_rung_even_when_a_better_one_is_free() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();

    // Rung 0 is affordable, so an unpinned request would take it.
    let unpinned = select(&config, ladder, &all_affordable(), &funded(), &[]);
    assert_eq!(unpinned.chosen.unwrap().rung, 0);

    // Pinned to rung 2, the conversation stays there: a hop would cost its
    // whole history at uncached rates.
    let pin = a_pin(2, Some(0.15), None);
    let selection = select_pinned(
        &config,
        ladder,
        &all_affordable(),
        &funded(),
        &[],
        Some(&pin),
    );

    let chosen = selection.chosen.unwrap();
    assert_eq!(chosen.rung, 2);
    assert!(selection.pinned);
    assert_eq!(selection.pin_rejected, None);
    // Rungs above the pin are not "skipped" — they were never considered.
    assert!(selection.skipped.is_empty());
}

#[test]
fn a_pin_steers_back_to_the_sub_provider_holding_the_cache() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();

    let mut table = PriceTable::new();
    table.insert(
        "surplus",
        "deepseek-v4-pro",
        ModelPrices::new(vec![offer("cheap", 0.05), offer("warm", 0.10)]),
    );

    let pin = a_pin(0, Some(0.30), Some("warm"));
    let chosen = select_pinned(&config, ladder, &table, &funded(), &[], Some(&pin))
        .chosen
        .unwrap();

    // "cheap" is cheaper, but "warm" already holds the prefix, so it leads.
    assert_eq!(chosen.prefer.first().map(String::as_str), Some("warm"));
}

#[test]
fn a_pin_never_admits_a_sub_provider_the_ceiling_rules_out() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();

    let mut table = PriceTable::new();
    table.insert(
        "surplus",
        "deepseek-v4-pro",
        // "warm" is now above the 0.30 ceiling.
        ModelPrices::new(vec![offer("cheap", 0.05), offer("warm", 0.90)]),
    );

    let pin = a_pin(0, Some(0.30), Some("warm"));
    let chosen = select_pinned(&config, ladder, &table, &funded(), &[], Some(&pin))
        .chosen
        .unwrap();

    // A warm cache is not a reason to exceed the budget.
    assert!(!chosen.prefer.iter().any(|prefer| prefer == "warm"));
    assert_eq!(chosen.admitted, vec!["cheap"]);
}

#[test]
fn a_pin_is_dropped_when_its_rung_is_priced_out() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();

    // Rung 0 no longer fits; rung 1 does.
    let table = prices(&[
        ("surplus", "deepseek-v4-pro", 9.0),
        ("surplus", "glm-5.2", 0.05),
    ]);

    let pin = a_pin(0, Some(0.30), None);
    let selection = select_pinned(&config, ladder, &table, &funded(), &[], Some(&pin));

    assert_eq!(selection.pin_rejected, Some(PinRejected::RungUnavailable));
    assert!(!selection.pinned);
    assert_eq!(selection.chosen.unwrap().rung, 1);
}

#[test]
fn a_pin_is_dropped_when_the_ceiling_changed() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();

    // The pin was taken under a 0.90 ceiling; the config now says 0.30.
    let pin = a_pin(0, Some(0.90), None);
    let selection = select_pinned(
        &config,
        ladder,
        &all_affordable(),
        &funded(),
        &[],
        Some(&pin),
    );

    // A changed ceiling is a changed policy, and the pin must not outlive it.
    assert_eq!(selection.pin_rejected, Some(PinRejected::CeilingChanged));
    assert_eq!(selection.chosen.unwrap().rung, 0);
}

#[test]
fn a_pin_from_another_ladder_is_ignored() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();

    let mut pin = a_pin(0, Some(0.30), None);
    pin.ladder = "flash".to_string();

    let selection = select_pinned(
        &config,
        ladder,
        &all_affordable(),
        &funded(),
        &[],
        Some(&pin),
    );

    // A different ladder is a request for a different capability.
    assert_eq!(selection.pin_rejected, Some(PinRejected::DifferentLadder));
}

#[test]
fn a_pin_past_the_end_of_a_shortened_ladder_is_ignored() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();

    let mut pin = a_pin(0, Some(0.30), None);
    pin.rung = 99;

    let selection = select_pinned(
        &config,
        ladder,
        &all_affordable(),
        &funded(),
        &[],
        Some(&pin),
    );
    assert_eq!(selection.pin_rejected, Some(PinRejected::RungGone));
}

#[test]
fn a_pin_whose_rung_now_names_a_different_model_is_ignored() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();

    // The ladder was reordered under the pin's feet.
    let mut pin = a_pin(0, Some(0.30), None);
    pin.model = "some-other-model".to_string();

    let selection = select_pinned(
        &config,
        ladder,
        &all_affordable(),
        &funded(),
        &[],
        Some(&pin),
    );
    assert_eq!(selection.pin_rejected, Some(PinRejected::RungGone));
}

#[test]
fn a_pinned_rung_that_already_failed_this_request_falls_through_quietly() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();

    let pin = a_pin(0, Some(0.30), None);
    // Rung 0 was tried and failed upstream a moment ago.
    let selection = select_pinned(
        &config,
        ladder,
        &all_affordable(),
        &funded(),
        &[0],
        Some(&pin),
    );

    // The pin is still justified; it just cannot be used for this attempt, so
    // it is not reported as rejected.
    assert_eq!(selection.pin_rejected, None);
    assert_eq!(selection.chosen.unwrap().rung, 1);
}

#[test]
fn a_pin_is_dropped_when_its_providers_balance_is_spent() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();

    let mut credits = CreditState::new();
    credits.set_balance("surplus", 0.01);
    credits.set_balance("openrouter", 50.0);

    let pin = a_pin(0, Some(0.30), None);
    let selection = select_pinned(
        &config,
        ladder,
        &all_affordable(),
        &credits,
        &[],
        Some(&pin),
    );

    assert_eq!(selection.pin_rejected, Some(PinRejected::RungUnavailable));
    assert_eq!(selection.chosen.unwrap().provider, "openrouter");
}

#[test]
fn selecting_without_a_pin_is_never_reported_as_pinned() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();

    let selection = select(&config, ladder, &all_affordable(), &funded(), &[]);
    assert!(!selection.pinned);
    assert_eq!(selection.pin_rejected, None);
}

/// A rung that answered 429 is out of the running while it cools, and says how
/// long it has left — the same shape as being priced out, because from the
/// engine's side it is the same thing: this rung cannot serve right now.
#[test]
fn a_rate_limited_rung_is_skipped_while_it_cools() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let prices = prices(&[
        ("surplus", "deepseek-v4-pro", 0.10),
        ("surplus", "glm-5.2", 0.05),
    ]);

    let mut cooldowns = Cooldowns::new();
    cooldowns.cool("surplus", "glm-5.2", std::time::Duration::from_secs(30));

    let selection =
        super::select_pinned(&config, ladder, &prices, &funded(), &cooldowns, &[], None);

    // The cheapest rung is parked, so the request takes the next best rather
    // than spending a round trip to be refused again.
    assert_eq!(selection.chosen.unwrap().model, "deepseek-v4-pro");
    let cooled = selection
        .skipped
        .iter()
        .find(|skip| skip.model == "glm-5.2")
        .expect("the rate-limited rung is reported");
    match cooled.reason {
        SkipReason::RateLimited { retry_in_secs } => assert!((25..=30).contains(&retry_in_secs)),
        ref other => panic!("expected RateLimited, got {other:?}"),
    }
}

/// A pin is not a way around a rate limit. The pinned rung goes through the
/// same admissibility check as any other, so a throttled one drops its pin
/// rather than being asked again for the sake of a warm cache.
#[test]
fn a_rate_limited_rung_loses_its_pin() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let mut cooldowns = Cooldowns::new();
    cooldowns.cool(
        "surplus",
        "deepseek-v4-flash",
        std::time::Duration::from_secs(30),
    );

    let pin = a_pin(2, Some(0.15), None);
    let selection = super::select_pinned(
        &config,
        ladder,
        &all_affordable(),
        &funded(),
        &cooldowns,
        &[],
        Some(&pin),
    );

    assert!(!selection.pinned);
    assert_eq!(selection.pin_rejected, Some(PinRejected::RungUnavailable));
    assert_ne!(selection.chosen.unwrap().rung, 2);
}

/// A cooldown that has run out is not a skip: the rung comes back on its own,
/// with no refresh and nothing to reset.
#[test]
fn an_expired_cooldown_puts_the_rung_back() {
    let config = config();
    let ladder = config.ladder("reasoning").unwrap();
    let prices = prices(&[("surplus", "glm-5.2", 0.05)]);

    let mut cooldowns = Cooldowns::new();
    cooldowns.cool("surplus", "glm-5.2", std::time::Duration::from_nanos(1));
    std::thread::sleep(std::time::Duration::from_millis(2));

    let selection =
        super::select_pinned(&config, ladder, &prices, &funded(), &cooldowns, &[], None);

    assert_eq!(selection.chosen.unwrap().model, "glm-5.2");
}

#[test]
fn a_rate_limited_skip_reads_as_a_wait() {
    assert_eq!(
        SkipReason::RateLimited { retry_in_secs: 42 }.to_string(),
        "rate limited, retry in 42s"
    );
}
