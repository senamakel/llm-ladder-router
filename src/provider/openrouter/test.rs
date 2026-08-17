//! Unit tests for the OpenRouter dialect, against payloads captured from the
//! live API on 2026-08-18.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::CostBasis;

const ENDPOINTS: &str = include_str!("../../../tests/fixtures/openrouter-endpoints.json");
const CREDITS: &str = include_str!("../../../tests/fixtures/openrouter-credits.json");

fn chosen(cap: Option<f64>, prefer: Vec<String>) -> Chosen {
    Chosen {
        rung: 0,
        provider: "openrouter".to_string(),
        model: "deepseek/deepseek-v4-flash".to_string(),
        cap_per_1m: cap,
        admitted: Vec::new(),
        cheapest_per_1m: None,
        min_discount_pct: Some(91),
        prefer,
    }
}

#[test]
fn parses_the_real_endpoints_payload() {
    let prices = parse_endpoints(ENDPOINTS.as_bytes()).unwrap();
    assert_eq!(prices.offers.len(), 6);

    let first = &prices.offers[0];
    assert_eq!(first.provider, "DigitalOcean");
    // The steering slug is read, not derived from the display name.
    assert_eq!(first.tag.as_deref(), Some("digitalocean"));
    assert!(first.usable);
    // 0.000000168 USD per token is 0.168 USD per million.
    assert!((first.completion_per_1m - 0.168).abs() < 1e-9, "{first:?}");
    assert!((first.prompt_per_1m - 0.0679).abs() < 1e-9, "{first:?}");
    // OpenRouter publishes no undiscounted reference price.
    assert_eq!(first.direct_completion_per_1m, None);
}

#[test]
fn a_quantized_endpoint_keeps_its_qualified_tag() {
    let prices = parse_endpoints(ENDPOINTS.as_bytes()).unwrap();
    let tags: Vec<&str> = prices
        .offers
        .iter()
        .filter_map(|offer| offer.tag.as_deref())
        .collect();
    // Lowercasing the display name would have produced "deepinfra", which is a
    // different endpoint from "deepinfra/fp8".
    assert!(tags.contains(&"deepinfra/fp8"), "{tags:?}");
}

#[test]
fn a_deranked_endpoint_is_not_usable() {
    let body = serde_json::json!({
        "data": { "endpoints": [
            { "provider_name": "Down", "tag": "down", "status": -1,
              "pricing": { "prompt": "0.000000001", "completion": "0.000000001" } },
            { "provider_name": "Up", "tag": "up", "status": 0,
              "pricing": { "prompt": "0.000000001", "completion": "0.000000001" } },
        ]}
    });
    let prices = parse_endpoints(body.to_string().as_bytes()).unwrap();

    assert!(!prices.offers[0].usable);
    assert!(prices.offers[1].usable);
    assert_eq!(prices.admitted(None, CostBasis::Completion).len(), 1);
}

#[test]
fn parses_the_real_credits_payload() {
    // total_credits 20 minus total_usage leaves the spendable remainder.
    let remaining = parse_credits(CREDITS.as_bytes()).unwrap();
    assert!(remaining.is_finite());
    assert!(remaining <= 20.0, "{remaining}");
}

#[test]
fn rejects_payloads_that_do_not_match_the_schema() {
    assert!(matches!(
        parse_endpoints(b"{}").unwrap_err(),
        Error::UnreadablePayload { .. }
    ));
    assert!(matches!(
        parse_credits(b"not json").unwrap_err(),
        Error::UnreadablePayload { .. }
    ));
}

#[test]
fn the_endpoints_path_names_the_model() {
    assert_eq!(
        endpoints_path("deepseek/deepseek-v4-flash"),
        "/models/deepseek/deepseek-v4-flash/endpoints"
    );
}

#[test]
fn each_wire_format_has_its_own_path() {
    assert_eq!(inference_path(Wire::OpenAi), "/chat/completions");
    assert_eq!(inference_path(Wire::Anthropic), "/messages");
}

#[test]
fn the_ceiling_travels_in_the_provider_object() {
    let mut body = serde_json::json!({ "model": "flash", "temperature": 0.5 });
    apply_routing(&mut body, &chosen(Some(0.30), vec!["deepinfra".to_string()]));

    assert_eq!(body["model"], "deepseek/deepseek-v4-flash");
    assert_eq!(body["provider"]["max_price"]["completion"], 0.30);
    assert_eq!(body["provider"]["order"][0], "deepinfra");
    assert_eq!(body["provider"]["allow_fallbacks"], true);
    // An exclusive pin has been observed to hang while idle endpoints sat idle.
    assert!(body["provider"].get("only").is_none());
    // Untouched caller fields survive.
    assert_eq!(body["temperature"], 0.5);
}

#[test]
fn an_uncapped_rung_without_preferences_sends_no_provider_object() {
    let mut body = serde_json::json!({ "model": "flash" });
    apply_routing(&mut body, &chosen(None, Vec::new()));

    assert_eq!(body["model"], "deepseek/deepseek-v4-flash");
    assert!(body.get("provider").is_none());
}

#[test]
fn a_non_object_body_is_left_alone() {
    let mut body = serde_json::json!("not an object");
    apply_routing(&mut body, &chosen(Some(0.30), Vec::new()));
    assert_eq!(body, serde_json::json!("not an object"));
}

#[test]
fn an_unsatisfiable_ceiling_advances_the_ladder() {
    // This arrives as a 404, which would otherwise read as a caller error.
    assert_eq!(
        classify(
            reqwest::StatusCode::NOT_FOUND,
            br#"{"error":{"message":"No endpoints found that satisfy the max price for this request"}}"#
        ),
        Disposition::Advance
    );
}

#[test]
fn an_upstream_failure_reported_as_a_400_advances_the_ladder() {
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"message":"Provider returned error"}}"#
        ),
        Disposition::Advance
    );
}

#[test]
fn a_genuine_caller_error_stops_the_ladder() {
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"message":"messages: field required"}}"#
        ),
        Disposition::CallerError
    );
    // A plain 404 that is not about price is still the caller's problem.
    assert_eq!(
        classify(reqwest::StatusCode::NOT_FOUND, b"{}"),
        Disposition::CallerError
    );
}

#[test]
fn server_errors_and_rate_limits_advance_the_ladder() {
    assert_eq!(
        classify(reqwest::StatusCode::INTERNAL_SERVER_ERROR, b"{}"),
        Disposition::Advance
    );
    assert_eq!(
        classify(reqwest::StatusCode::TOO_MANY_REQUESTS, b"{}"),
        Disposition::Advance
    );
}

#[test]
fn a_success_is_served() {
    assert_eq!(classify(reqwest::StatusCode::OK, b"{}"), Disposition::Served);
}
