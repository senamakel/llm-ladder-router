//! Unit tests for the normalized price table.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::CostBasis;

fn offer(provider: &str, prompt: f64, completion: f64) -> Offer {
    Offer {
        provider: provider.to_string(),
        tag: None,
        prompt_per_1m: prompt,
        completion_per_1m: completion,
        direct_completion_per_1m: Some(3.74),
        usable: true,
    }
}

#[test]
fn the_cost_basis_selects_which_price_is_compared() {
    let offer = offer("Z.ai", 0.10, 0.30);

    assert!((offer.price(CostBasis::Prompt) - 0.10).abs() < f64::EPSILON);
    assert!((offer.price(CostBasis::Completion) - 0.30).abs() < f64::EPSILON);
    assert!((offer.price(CostBasis::Blended) - 0.20).abs() < f64::EPSILON);
}

#[test]
fn an_offer_at_exactly_the_ceiling_fits() {
    let offer = offer("Z.ai", 0.10, 0.30);
    // The ceiling is a maximum, not an exclusive bound.
    assert!(offer.fits(Some(0.30), CostBasis::Completion));
    assert!(!offer.fits(Some(0.29), CostBasis::Completion));
}

#[test]
fn an_uncapped_rung_admits_every_usable_offer() {
    let offer = offer("Z.ai", 100.0, 900.0);
    assert!(offer.fits(None, CostBasis::Completion));
}

#[test]
fn an_unusable_offer_fits_nothing() {
    let mut offer = offer("Z.ai", 0.001, 0.001);
    offer.usable = false;
    assert!(!offer.fits(None, CostBasis::Completion));
    assert!(!offer.fits(Some(100.0), CostBasis::Completion));
}

#[test]
fn admitted_offers_come_back_cheapest_first() {
    let prices = ModelPrices::new(vec![
        offer("dear", 0.5, 0.90),
        offer("cheap", 0.05, 0.09),
        offer("middle", 0.2, 0.30),
    ]);

    let admitted = prices.admitted(Some(0.50), CostBasis::Completion);
    assert_eq!(
        admitted
            .iter()
            .map(|offer| offer.provider.as_str())
            .collect::<Vec<_>>(),
        vec!["cheap", "middle"]
    );
}

#[test]
fn the_floor_is_the_cheapest_usable_offer_whatever_the_ceiling() {
    let mut unusable = offer("free-but-down", 0.0, 0.0);
    unusable.usable = false;
    let prices = ModelPrices::new(vec![
        unusable,
        offer("dear", 0.5, 0.90),
        offer("less", 0.3, 0.63),
    ]);

    // The floor explains a skipped rung, so it ignores the ceiling but not
    // whether the seller can actually serve.
    assert_eq!(prices.floor(CostBasis::Completion), Some(0.63));
}

#[test]
fn the_floor_is_absent_when_no_offer_is_usable() {
    let mut down = offer("down", 0.1, 0.1);
    down.usable = false;
    assert_eq!(
        ModelPrices::new(vec![down]).floor(CostBasis::Completion),
        None
    );
}

#[test]
fn a_ceiling_becomes_the_discount_that_matches_it() {
    let prices = ModelPrices::new(vec![offer("Z.ai", 0.05, 0.09)]);

    // 0.30 against a 3.74 direct price leaves 8.02%, so 91% must be discounted.
    assert_eq!(
        prices.discount_floor_pct(0.30, CostBasis::Completion),
        Some(91)
    );
    // A ceiling at the direct price needs no discount at all.
    assert_eq!(
        prices.discount_floor_pct(3.74, CostBasis::Completion),
        Some(0)
    );
    // A ceiling above the direct price still needs none.
    assert_eq!(
        prices.discount_floor_pct(99.0, CostBasis::Completion),
        Some(0)
    );
}

#[test]
fn there_is_no_discount_to_compute_without_a_direct_price() {
    let mut offer = offer("Z.ai", 0.05, 0.09);
    offer.direct_completion_per_1m = None;
    let prices = ModelPrices::new(vec![offer]);

    // OpenRouter publishes no direct price, and guessing one would be wrong.
    assert_eq!(prices.discount_floor_pct(0.30, CostBasis::Completion), None);
}

#[test]
fn a_fresh_snapshot_is_not_stale() {
    let prices = ModelPrices::new(vec![offer("Z.ai", 0.1, 0.1)]);
    assert!(!prices.is_stale(std::time::Duration::from_secs(60)));
    std::thread::sleep(std::time::Duration::from_millis(2));
    assert!(prices.is_stale(std::time::Duration::ZERO));
}

#[test]
fn the_table_holds_one_entry_per_provider_and_model() {
    let mut table = PriceTable::new();
    assert!(table.is_empty());

    table.insert(
        "surplus",
        "glm-5.2",
        ModelPrices::new(vec![offer("a", 0.1, 0.1)]),
    );
    table.insert(
        "openrouter",
        "glm-5.2",
        ModelPrices::new(vec![offer("b", 0.2, 0.2)]),
    );

    assert_eq!(table.len(), 2);
    // Same model, different provider: these must not collide.
    assert_eq!(
        table.get("surplus", "glm-5.2").unwrap().offers[0].provider,
        "a"
    );
    assert_eq!(
        table.get("openrouter", "glm-5.2").unwrap().offers[0].provider,
        "b"
    );
    assert!(table.get("surplus", "absent").is_none());
    assert!(table.get("absent", "glm-5.2").is_none());
}

#[test]
fn inserting_the_same_key_replaces_the_snapshot() {
    let mut table = PriceTable::new();
    table.insert(
        "surplus",
        "glm-5.2",
        ModelPrices::new(vec![offer("old", 0.1, 0.1)]),
    );
    table.insert(
        "surplus",
        "glm-5.2",
        ModelPrices::new(vec![offer("new", 0.2, 0.2)]),
    );

    assert_eq!(table.len(), 1);
    assert_eq!(
        table.get("surplus", "glm-5.2").unwrap().offers[0].provider,
        "new"
    );
}
