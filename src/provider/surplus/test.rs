//! Unit tests for the Surplus dialect.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::CostBasis;

fn chosen(min_discount_pct: Option<u8>) -> Chosen {
    Chosen {
        rung: 0,
        provider: "surplus".to_string(),
        model: "glm-5.2".to_string(),
        cap_per_1m: Some(0.30),
        admitted: Vec::new(),
        cheapest_per_1m: None,
        min_discount_pct,
        prefer: Vec::new(),
        reasoning_effort: None,
        score_multiplier: 1.0,
        score: None,
    }
}

/// One offer shaped exactly like the live order book, in micro-USD per Mtok.
fn order_book(price_output: f64, available: bool, healthy: bool) -> String {
    serde_json::json!({
        "offers": [{
            "provider": "Z.ai",
            "price_input_per_1m": 3076.0,
            "price_output_per_1m": price_output,
            "direct_output_per_1m": 3_740_000.0,
            "available": available,
            "healthy": healthy,
        }]
    })
    .to_string()
}

#[test]
fn parses_an_order_book_and_converts_micro_usd_to_usd() {
    let prices = parse_order_book(order_book(9668.0, true, true).as_bytes()).unwrap();
    let offer = &prices.offers[0];

    assert_eq!(offer.provider, "Z.ai");
    // 9668 micro-USD per Mtok is $0.009668 per Mtok.
    assert!(
        (offer.completion_per_1m - 0.009_668).abs() < 1e-9,
        "{offer:?}"
    );
    assert!((offer.prompt_per_1m - 0.003_076).abs() < 1e-9, "{offer:?}");
    assert!((offer.direct_completion_per_1m.unwrap() - 3.74).abs() < 1e-9);
    assert!(offer.usable);
    // Surplus exposes no per-seller steering slug.
    assert_eq!(offer.tag, None);
}

#[test]
fn an_offer_must_be_both_available_and_healthy_to_be_usable() {
    for (available, healthy) in [(false, true), (true, false), (false, false)] {
        let prices = parse_order_book(order_book(100.0, available, healthy).as_bytes()).unwrap();
        assert!(
            !prices.offers[0].usable,
            "available={available} healthy={healthy} should not be usable"
        );
    }
}

#[test]
fn an_offer_missing_its_prices_is_read_as_free_rather_than_rejected() {
    let body = serde_json::json!({
        "offers": [{ "provider": "Sparse", "available": true, "healthy": true }]
    });
    let prices = parse_order_book(body.to_string().as_bytes()).unwrap();

    // A sparse row must not take down the whole refresh.
    assert_eq!(prices.offers[0].provider, "Sparse");
    assert!(prices.offers[0].completion_per_1m.abs() < f64::EPSILON);
    assert_eq!(prices.offers[0].direct_completion_per_1m, None);
}

#[test]
fn an_offer_without_a_provider_name_is_still_read() {
    let body = serde_json::json!({
        "offers": [{ "price_output_per_1m": 100.0, "available": true, "healthy": true }]
    });
    let prices = parse_order_book(body.to_string().as_bytes()).unwrap();
    assert_eq!(prices.offers[0].provider, "unknown");
}

#[test]
fn the_spendable_balance_is_the_lesser_of_balance_and_allowance() {
    // Micro-USD strings, exactly as the live endpoint returns them.
    let body = br#"{"balance_usdc":"74673082","allowance_usdc":"74673033"}"#;
    let balance = parse_balance(body).unwrap();

    // An allowance below the balance is the real limit.
    assert!((balance - 74.673_033).abs() < 1e-6, "{balance}");
}

#[test]
fn either_figure_alone_is_enough() {
    assert!((parse_balance(br#"{"balance_usdc":"1000000"}"#).unwrap() - 1.0).abs() < 1e-9);
    assert!((parse_balance(br#"{"allowance_usdc":"2000000"}"#).unwrap() - 2.0).abs() < 1e-9);
}

#[test]
fn a_profile_with_neither_figure_is_unreadable() {
    assert!(matches!(
        parse_balance(b"{}").unwrap_err(),
        Error::UnreadablePayload { .. }
    ));
    assert!(matches!(
        parse_order_book(b"[]").unwrap_err(),
        Error::UnreadablePayload { .. }
    ));
}

#[test]
fn the_discount_prefix_sits_in_a_different_place_on_each_surface() {
    // Verified against the live API: the other orderings 404.
    assert_eq!(
        inference_path(&chosen(Some(95)), Wire::OpenAi),
        "/min95/v1/chat/completions"
    );
    assert_eq!(
        inference_path(&chosen(Some(95)), Wire::Anthropic),
        "/anthropic/min95/v1/messages"
    );
}

#[test]
fn a_rung_needing_no_discount_uses_the_plain_path() {
    for discount in [None, Some(0)] {
        assert_eq!(
            inference_path(&chosen(discount), Wire::OpenAi),
            "/v1/chat/completions"
        );
        assert_eq!(
            inference_path(&chosen(discount), Wire::Anthropic),
            "/anthropic/v1/messages"
        );
    }
}

#[test]
fn the_paths_name_the_model_and_the_buyer() {
    assert_eq!(order_book_path("glm-5.2"), "/api/markets/glm-5.2");
    assert_eq!(balance_path(), "/v1/buyer/me");
}

#[test]
fn only_the_model_is_rewritten_in_the_body() {
    let mut body = serde_json::json!({ "model": "flash", "temperature": 0.5 });
    apply_routing(&mut body, &chosen(Some(95)));

    assert_eq!(body["model"], "glm-5.2");
    assert_eq!(body["temperature"], 0.5);
    // The ceiling travels in the path; OpenRouter's provider object must never
    // reach a Surplus endpoint.
    assert!(body.get("provider").is_none());
    assert!(body.get("max_price_per_1m").is_none());
}

#[test]
fn a_non_object_body_is_left_alone() {
    let mut body = serde_json::json!(42);
    apply_routing(&mut body, &chosen(Some(95)));
    assert_eq!(body, serde_json::json!(42));
}

#[test]
fn the_marketplace_refusals_advance_the_ladder() {
    // No seller met the discount floor.
    assert_eq!(
        classify(
            reqwest::StatusCode::NOT_FOUND,
            br#"{"error":{"code":"minimum_discount_not_met"}}"#
        ),
        Disposition::Advance
    );
    // Nobody carries the model.
    assert_eq!(
        classify(
            reqwest::StatusCode::NOT_FOUND,
            br#"{"error":{"code":"no_sellers_for_model"}}"#
        ),
        Disposition::Advance
    );
    // The balance ran out mid-flight.
    assert_eq!(
        classify(reqwest::StatusCode::PAYMENT_REQUIRED, b"{}"),
        Disposition::Advance
    );
    // Every seller is down.
    assert_eq!(
        classify(reqwest::StatusCode::SERVICE_UNAVAILABLE, b"{}"),
        Disposition::Advance
    );
}

#[test]
fn a_sub_provider_schema_rejection_advances() {
    // The exact shape seen on 2026-08-24: a sub-provider that accepts only a
    // string `content` refusing an Anthropic block array, relayed as a 400.
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":"2 request validation errors: Input should be a valid string, field: 'messages[1].content.str'"}"#
        ),
        Disposition::Advance
    );
    // The same failure wearing Surplus's relay prefix.
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":"Provider returned 400: bad content block"}"#
        ),
        Disposition::Advance
    );
}

#[test]
fn a_genuine_caller_error_stops_the_ladder() {
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"code":"invalid_request_error"}}"#
        ),
        Disposition::CallerError
    );
}

#[test]
fn a_success_is_served() {
    assert_eq!(
        classify(reqwest::StatusCode::OK, b"{}"),
        Disposition::Served
    );
}

#[test]
fn the_order_book_admits_only_sellers_under_the_ceiling() {
    let body = serde_json::json!({
        "offers": [
            { "provider": "cheap", "price_output_per_1m": 9_668.0, "direct_output_per_1m": 3_740_000.0, "available": true, "healthy": true },
            { "provider": "dear",  "price_output_per_1m": 900_000.0, "direct_output_per_1m": 3_740_000.0, "available": true, "healthy": true },
        ]
    });
    let prices = parse_order_book(body.to_string().as_bytes()).unwrap();

    // $0.30/Mtok admits the 0.0097 seller and excludes the 0.90 one.
    let admitted = prices.admitted(Some(0.30), CostBasis::Completion);
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].provider, "cheap");
}
