//! Unit tests for the proxy's own logic.
//!
//! The request path is covered end to end in `tests/ladder_proxy.rs`, against
//! mock marketplaces. What is left here is the parts that need no upstream:
//! caller authentication and the shape of a routing decision's response.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::ladder::SkipReason;

const OPEN: &str = r#"
[providers.openrouter]
kind = "openrouter"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"

[[ladders]]
name = "flash"
  [[ladders.rungs]]
  provider = "openrouter"
  model = "m"
"#;

fn state_with(server: &str) -> State {
    let text = format!("[server]\n{server}\n{OPEN}");
    let config = Config::parse(&text).unwrap();
    let (_, state) = build(config).unwrap();
    state
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    headers
}

#[test]
fn a_router_with_no_key_accepts_everyone() {
    let state = state_with("bind = \"127.0.0.1:6969\"");
    assert!(authorized(&state, &HeaderMap::new()));
}

#[test]
fn the_configured_key_is_accepted_under_either_header() {
    let state = state_with("api_key = \"s3cret\"");

    assert!(authorized(
        &state,
        &headers(&[("authorization", "Bearer s3cret")])
    ));
    // Anthropic clients send the key this way, and both surfaces accept both.
    assert!(authorized(&state, &headers(&[("x-api-key", "s3cret")])));
}

#[test]
fn a_wrong_or_absent_key_is_rejected() {
    let state = state_with("api_key = \"s3cret\"");

    assert!(!authorized(&state, &HeaderMap::new()));
    assert!(!authorized(
        &state,
        &headers(&[("authorization", "Bearer wrong")])
    ));
    assert!(!authorized(&state, &headers(&[("x-api-key", "wrong")])));
    // The scheme matters: a bare key in Authorization is not a bearer token.
    assert!(!authorized(
        &state,
        &headers(&[("authorization", "s3cret")])
    ));
    // A prefix of the real key must not pass.
    assert!(!authorized(&state, &headers(&[("x-api-key", "s3cre")])));
}

#[test]
fn a_blank_configured_key_leaves_the_router_open() {
    // An empty string is a misconfiguration, not a key that nobody can guess;
    // treating it as a key would lock everyone out silently.
    let state = state_with("api_key = \"\"");
    assert!(authorized(&state, &HeaderMap::new()));
}

#[test]
fn an_exhausted_ladder_explains_every_rung_it_passed_over() {
    let skipped = vec![
        Skipped {
            rung: 0,
            provider: "surplus".to_string(),
            model: "deepseek-v4-pro".to_string(),
            reason: SkipReason::NoSellerUnderCap {
                cap_per_1m: 0.30,
                cheapest_per_1m: Some(0.63),
            },
        },
        Skipped {
            rung: 1,
            provider: "openrouter".to_string(),
            model: "deepseek/deepseek-v4-flash".to_string(),
            reason: SkipReason::UpstreamFailed {
                detail: "503 down".to_string(),
            },
        },
    ];

    let response = problem(StatusCode::BAD_GATEWAY, "no rung could serve", &skipped);
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[test]
fn a_routing_decision_is_stamped_onto_the_response() {
    let chosen = Chosen {
        rung: 2,
        provider: "surplus".to_string(),
        model: "glm-5.2".to_string(),
        cap_per_1m: Some(0.30),
        admitted: vec!["Z.ai".to_string()],
        cheapest_per_1m: Some(0.009),
        min_discount_pct: Some(91),
        prefer: Vec::new(),
    };

    let response = with_routing_headers(
        Response::new(axum::body::Body::empty()),
        "reasoning",
        &chosen,
        2,
    );
    let headers = response.headers();

    assert_eq!(headers[HEADER_LADDER], "reasoning");
    assert_eq!(headers[HEADER_RUNG], "2");
    assert_eq!(headers[HEADER_PROVIDER], "surplus");
    assert_eq!(headers[HEADER_MODEL], "glm-5.2");
    assert_eq!(headers[HEADER_SKIPPED], "2");
    assert_eq!(headers[HEADER_CAP], "0.3");
}

#[test]
fn an_uncapped_rung_carries_no_ceiling_header() {
    let chosen = Chosen {
        rung: 0,
        provider: "openrouter".to_string(),
        model: "m".to_string(),
        cap_per_1m: None,
        admitted: Vec::new(),
        cheapest_per_1m: None,
        min_discount_pct: None,
        prefer: Vec::new(),
    };

    let response = with_routing_headers(
        Response::new(axum::body::Body::empty()),
        "flash",
        &chosen,
        0,
    );
    assert!(response.headers().get(HEADER_CAP).is_none());
}

#[tokio::test]
async fn the_ladders_are_listed_as_models() {
    let state = state_with("bind = \"127.0.0.1:6969\"");
    let Json(body) = list_models(AxumState(state)).await;

    assert_eq!(body["object"], "list");
    assert_eq!(body["data"][0]["id"], "flash");
    assert_eq!(body["data"][0]["owned_by"], "llm-ladder-router");
    assert_eq!(body["data"][0]["rungs"], 1);
}
