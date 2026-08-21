//! Unit tests for the provider dispatch layer.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::ProviderKind;

fn provider(kind: ProviderKind) -> Provider {
    Provider {
        kind,
        base_url: "https://example.test/v1".to_string(),
        api_key_env: "LADDER_TEST_UNSET_KEY".to_string(),
        max_cost_per_1m: None,
        headers: std::collections::BTreeMap::new(),
    }
}

#[test]
fn an_injected_credential_is_used() {
    let client = Client::with_credential(
        "surplus",
        provider(ProviderKind::Surplus),
        reqwest::Client::new(),
        Some("secret".to_string()),
    );

    assert_eq!(client.name(), "surplus");
    assert!(client.has_credential());
    assert_eq!(client.credential_variable(), "LADDER_TEST_UNSET_KEY");
}

#[test]
fn a_blank_credential_counts_as_absent() {
    // An empty or whitespace-only variable is a misconfiguration, not a key.
    for blank in ["", "   ", "\n"] {
        let client = Client::with_credential(
            "surplus",
            provider(ProviderKind::Surplus),
            reqwest::Client::new(),
            Some(blank.to_string()),
        );
        assert!(
            !client.has_credential(),
            "{blank:?} should not count as a key"
        );
    }
}

#[test]
fn an_absent_environment_variable_leaves_the_client_without_a_credential() {
    // The variable name is deliberately one nothing sets.
    let client = Client::new(
        "surplus",
        provider(ProviderKind::Surplus),
        reqwest::Client::new(),
    );
    assert!(!client.has_credential());
}

#[test]
fn a_credential_is_trimmed() {
    let client = Client::with_credential(
        "surplus",
        provider(ProviderKind::Surplus),
        reqwest::Client::new(),
        Some("  secret  ".to_string()),
    );
    assert!(client.has_credential());
}

#[test]
fn the_serving_sub_provider_is_read_from_the_response_body() {
    assert_eq!(
        served_by(br#"{"provider":"DeepInfra","choices":[]}"#),
        Some("DeepInfra".to_string())
    );
}

#[test]
fn a_body_without_a_provider_field_names_nobody() {
    // Anthropic responses carry no top-level `provider`, and guessing is worse
    // than reporting nothing.
    assert_eq!(served_by(br#"{"type":"message","content":[]}"#), None);
    assert_eq!(served_by(b"not json"), None);
    assert_eq!(served_by(br#"{"provider":42}"#), None);
}

#[test]
fn statuses_are_classified_by_who_is_at_fault() {
    assert_eq!(
        classify_status(reqwest::StatusCode::OK),
        Disposition::Served
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::NO_CONTENT),
        Disposition::Served
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::BAD_GATEWAY),
        Disposition::Advance
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS),
        Disposition::Advance
    );
    // The router holds the marketplace credential, not the caller, so an
    // upstream refusing to authenticate us is a rung that cannot serve rather
    // than a request that cannot be made. A live Surplus outage answered 403
    // to every ladder and killed five runs that had a second provider one rung
    // below.
    assert_eq!(
        classify_status(reqwest::StatusCode::UNAUTHORIZED),
        Disposition::Advance
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::FORBIDDEN),
        Disposition::Advance
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED),
        Disposition::Advance
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::REQUEST_TIMEOUT),
        Disposition::Advance
    );
    // The rest of 4xx describes the request, which every rung would refuse
    // identically, so walking the ladder would only report the last refusal.
    assert_eq!(
        classify_status(reqwest::StatusCode::UNPROCESSABLE_ENTITY),
        Disposition::CallerError
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::BAD_REQUEST),
        Disposition::CallerError
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::NOT_FOUND),
        Disposition::CallerError
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::PAYLOAD_TOO_LARGE),
        Disposition::CallerError
    );
}

/// A loopback upstream that answers every route this crate calls.
async fn upstream() -> String {
    use axum::routing::{get, post};

    let app = axum::Router::new()
        .route(
            "/models/{author}/{model}/endpoints",
            get(|| async {
                axum::Json(serde_json::json!({
                    "data": { "endpoints": [{
                        "provider_name": "DeepInfra", "tag": "deepinfra", "status": 0,
                        "pricing": { "prompt": "0.0000001", "completion": "0.0000002" },
                    }]}
                }))
            }),
        )
        .route(
            "/credits",
            get(|| async {
                axum::Json(serde_json::json!({
                    "data": { "total_credits": 20.0, "total_usage": 8.0 }
                }))
            }),
        )
        .route(
            "/chat/completions",
            post(|headers: axum::http::HeaderMap| async move {
                // Echo back what the client sent, so the test can assert on it.
                axum::Json(serde_json::json!({
                    "provider": "DeepInfra",
                    "authorization": headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default(),
                    "referer": headers
                        .get("http-referer")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default(),
                    "anthropic_version": headers
                        .get("anthropic-version")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default(),
                }))
            }),
        )
        // Mistral's own path, which carries the version in the path rather than
        // in the base URL. Echoes the body so a test can see the rewrite.
        .route(
            "/v1/chat/completions",
            post(|body: axum::Json<serde_json::Value>| async move {
                axum::Json(serde_json::json!({ "sent": body.0 }))
            }),
        )
        // Venice's own path, which roots its version at `/api/v1`. Echoes the
        // body for the same reason.
        .route(
            "/api/v1/chat/completions",
            post(|body: axum::Json<serde_json::Value>| async move {
                axum::Json(serde_json::json!({ "sent": body.0 }))
            }),
        )
        .route(
            "/messages",
            post(|headers: axum::http::HeaderMap| async move {
                axum::Json(serde_json::json!({
                    "anthropic_version": headers
                        .get("anthropic-version")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default(),
                }))
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}")
}

fn openrouter_client(base_url: &str) -> Client {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert(
        "HTTP-Referer".to_string(),
        "https://example.test/".to_string(),
    );
    Client::with_credential(
        "openrouter",
        Provider {
            kind: ProviderKind::OpenRouter,
            base_url: base_url.to_string(),
            api_key_env: "LADDER_TEST_UNSET_KEY".to_string(),
            max_cost_per_1m: None,
            headers,
        },
        reqwest::Client::new(),
        Some("test-key".to_string()),
    )
}

fn chosen() -> Chosen {
    Chosen {
        rung: 0,
        provider: "openrouter".to_string(),
        model: "deepseek/deepseek-v4-flash".to_string(),
        cap_per_1m: Some(0.30),
        admitted: Vec::new(),
        cheapest_per_1m: None,
        min_discount_pct: None,
        prefer: vec!["deepinfra".to_string()],
        reasoning_effort: None,
        score_multiplier: 1.0,
        score: None,
    }
}

#[tokio::test]
async fn openrouter_prices_and_balance_are_fetched_and_parsed() {
    let client = openrouter_client(&upstream().await);

    let prices = client
        .fetch_prices("deepseek/deepseek-v4-flash")
        .await
        .unwrap();
    assert_eq!(prices.offers[0].provider, "DeepInfra");
    assert!((prices.offers[0].completion_per_1m - 0.2).abs() < 1e-9);

    // total_credits 20 minus total_usage 8.
    let balance = client.fetch_balance().await.unwrap();
    assert!((balance - 12.0).abs() < 1e-9);
}

#[tokio::test]
async fn the_credential_and_configured_headers_reach_the_upstream() {
    let client = openrouter_client(&upstream().await);

    let dispatched = client
        .infer(
            &chosen(),
            Wire::OpenAi,
            &serde_json::json!({ "messages": [] }),
        )
        .await
        .unwrap();

    let body: serde_json::Value = serde_json::from_slice(&dispatched.body).unwrap();
    assert_eq!(body["authorization"], "Bearer test-key");
    assert_eq!(body["referer"], "https://example.test/");
    // The OpenAI surface must not carry an Anthropic version header.
    assert_eq!(body["anthropic_version"], "");
    assert_eq!(dispatched.served_by, Some("DeepInfra".to_string()));
    assert_eq!(client.classify(&dispatched), Disposition::Served);
}

#[tokio::test]
async fn the_anthropic_surface_sends_the_version_header() {
    let client = openrouter_client(&upstream().await);

    let dispatched = client
        .infer(
            &chosen(),
            Wire::Anthropic,
            &serde_json::json!({ "messages": [] }),
        )
        .await
        .unwrap();

    let body: serde_json::Value = serde_json::from_slice(&dispatched.body).unwrap();
    // Both marketplaces reject a Messages request without it.
    assert_eq!(body["anthropic_version"], "2023-06-01");
}

#[tokio::test]
async fn an_unreachable_upstream_is_reported_as_an_upstream_error() {
    // Port 1 on loopback refuses connections immediately.
    let client = openrouter_client("http://127.0.0.1:1");

    assert!(matches!(
        client.fetch_prices("m").await.unwrap_err(),
        Error::Upstream { .. }
    ));
    assert!(matches!(
        client.fetch_balance().await.unwrap_err(),
        Error::Upstream { .. }
    ));
    assert!(matches!(
        client
            .infer(&chosen(), Wire::OpenAi, &serde_json::json!({}))
            .await
            .unwrap_err(),
        Error::Upstream { .. }
    ));
}

#[tokio::test]
async fn a_trailing_slash_on_the_base_url_does_not_double_up() {
    let base = upstream().await;
    let client = openrouter_client(&format!("{base}/"));

    // A doubled slash would 404; this succeeding is the assertion.
    assert!(client.fetch_balance().await.is_ok());
}

/// A declared effort reaches the body the upstream actually receives.
#[test]
fn a_declared_effort_is_injected_on_the_openai_surface() {
    let mut chosen = chosen();
    chosen.reasoning_effort = Some("xhigh".to_string());
    let mut body = serde_json::json!({ "model": "max-reasoning", "messages": [] });

    apply_reasoning_effort(&mut body, &chosen, Wire::OpenAi);

    assert_eq!(body["reasoning_effort"], "xhigh");
}

/// The caller always wins: a request that asked to think less must not be made
/// expensive by the ladder it happened to select.
#[test]
fn a_callers_own_depth_is_never_overwritten() {
    let mut chosen = chosen();
    chosen.reasoning_effort = Some("xhigh".to_string());

    let mut explicit = serde_json::json!({ "messages": [], "reasoning_effort": "low" });
    apply_reasoning_effort(&mut explicit, &chosen, Wire::OpenAi);
    assert_eq!(explicit["reasoning_effort"], "low");

    // The other spelling counts as the caller having decided, too.
    let mut structured = serde_json::json!({ "messages": [], "reasoning": { "effort": "low" } });
    apply_reasoning_effort(&mut structured, &chosen, Wire::OpenAi);
    assert!(structured.get("reasoning_effort").is_none());
}

/// Anthropic spells depth as a `thinking` budget, so inventing an `OpenAI` field
/// there would be translating between dialects rather than relaying.
#[test]
fn the_anthropic_surface_is_left_alone() {
    let mut chosen = chosen();
    chosen.reasoning_effort = Some("high".to_string());
    let mut body = serde_json::json!({ "messages": [] });

    apply_reasoning_effort(&mut body, &chosen, Wire::Anthropic);

    assert!(body.get("reasoning_effort").is_none());
}

/// A ladder that declares nothing changes nothing.
#[test]
fn no_declared_effort_inserts_nothing() {
    let mut body = serde_json::json!({ "messages": [] });
    apply_reasoning_effort(&mut body, &chosen(), Wire::OpenAi);
    assert!(body.get("reasoning_effort").is_none());
}

/// The upstream's own backoff is read when it gives one, and only in the form
/// that is unambiguous.
#[test]
fn retry_after_is_read_in_its_delta_seconds_form() {
    assert_eq!(
        parse_retry_after(Some("45")),
        Some(std::time::Duration::from_secs(45))
    );
    assert_eq!(
        parse_retry_after(Some("  45  ")),
        Some(std::time::Duration::from_secs(45))
    );
    // "Retry immediately" is not a request to be parked.
    assert_eq!(parse_retry_after(Some("0")), None);
    // The HTTP-date form is legal and would need a clock-skew guess to convert;
    // the configured default is a better answer than a guess.
    assert_eq!(
        parse_retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
        None
    );
    assert_eq!(parse_retry_after(None), None);
}

fn mistral_client(base_url: &str) -> Client {
    Client::with_credential(
        "mistral",
        Provider {
            kind: ProviderKind::Mistral,
            base_url: base_url.to_string(),
            api_key_env: "LADDER_TEST_UNSET_KEY".to_string(),
            max_cost_per_1m: None,
            headers: std::collections::BTreeMap::new(),
        },
        reqwest::Client::new(),
        Some("test-key".to_string()),
    )
}

fn scribe_rung() -> Chosen {
    Chosen {
        rung: 0,
        provider: "mistral".to_string(),
        model: "labs-leanstral-1-5".to_string(),
        cap_per_1m: None,
        admitted: Vec::new(),
        cheapest_per_1m: None,
        min_discount_pct: None,
        prefer: Vec::new(),
        reasoning_effort: None,
        score_multiplier: 1.0,
        score: None,
    }
}

/// A direct endpoint has one seller, so there is no order book and no balance.
/// Asked for either, it says so rather than inventing an empty answer that the
/// engine would read as "every seller is down".
#[tokio::test]
async fn a_direct_provider_publishes_neither_prices_nor_a_balance() {
    let client = mistral_client(&upstream().await);
    assert!(!client.is_marketplace());

    match client.fetch_prices("labs-leanstral-1-5").await {
        Err(Error::NoMarketData { provider, what }) => {
            assert_eq!(provider, "mistral");
            assert_eq!(what, "order book");
        }
        other => panic!("expected NoMarketData, got {other:?}"),
    }

    match client.fetch_balance().await {
        Err(Error::NoMarketData { what, .. }) => assert_eq!(what, "balance"),
        other => panic!("expected NoMarketData, got {other:?}"),
    }
}

#[tokio::test]
async fn a_direct_provider_serves_the_openai_surface() {
    let client = mistral_client(&upstream().await);

    let dispatched = client
        .infer(
            &scribe_rung(),
            Wire::OpenAi,
            &serde_json::json!({ "model": "scribe", "messages": [] }),
        )
        .await
        .unwrap();

    assert!(dispatched.status.is_success());
    let echoed: serde_json::Value = serde_json::from_slice(&dispatched.body).unwrap();
    // The ladder name the caller sent is replaced by the rung's model, and the
    // request went to Mistral's own path rather than a marketplace's.
    assert_eq!(echoed["sent"]["model"], "labs-leanstral-1-5");
}

/// There is no Anthropic surface here, so the request is declined before the
/// round trip. The failover loop reads that as this rung's failure and takes
/// the next one.
#[tokio::test]
async fn a_direct_provider_declines_the_anthropic_surface() {
    let client = mistral_client(&upstream().await);

    match client
        .infer(
            &scribe_rung(),
            Wire::Anthropic,
            &serde_json::json!({ "messages": [] }),
        )
        .await
    {
        Err(Error::UnsupportedWire { provider, wire }) => {
            assert_eq!(provider, "mistral");
            assert_eq!(wire, "Anthropic Messages");
        }
        other => panic!("expected UnsupportedWire, got {other:?}"),
    }
}

fn venice_client(base_url: &str) -> Client {
    Client::with_credential(
        "venice",
        Provider {
            kind: ProviderKind::Venice,
            base_url: base_url.to_string(),
            api_key_env: "LADDER_TEST_UNSET_KEY".to_string(),
            max_cost_per_1m: None,
            headers: std::collections::BTreeMap::new(),
        },
        reqwest::Client::new(),
        Some("test-key".to_string()),
    )
}

fn uncensored_rung() -> Chosen {
    Chosen {
        rung: 1,
        provider: "venice".to_string(),
        model: "venice-uncensored-1.2".to_string(),
        cap_per_1m: None,
        admitted: Vec::new(),
        cheapest_per_1m: None,
        min_discount_pct: None,
        prefer: Vec::new(),
        reasoning_effort: None,
        score_multiplier: 1.0,
        score: None,
    }
}

/// Venice is direct for the same reasons Mistral is, and answers the same way
/// when asked for market data it does not publish.
#[tokio::test]
async fn venice_publishes_neither_prices_nor_a_balance() {
    let client = venice_client(&upstream().await);
    assert!(!client.is_marketplace());

    match client.fetch_prices("venice-uncensored-1.2").await {
        Err(Error::NoMarketData { provider, what }) => {
            assert_eq!(provider, "venice");
            assert_eq!(what, "order book");
        }
        other => panic!("expected NoMarketData, got {other:?}"),
    }

    match client.fetch_balance().await {
        Err(Error::NoMarketData { provider, what }) => {
            assert_eq!(provider, "venice");
            assert_eq!(what, "balance");
        }
        other => panic!("expected NoMarketData, got {other:?}"),
    }
}

/// The rewrite that matters on this provider travels with the request: the
/// model, and the refusal of Venice's house system prompt.
#[tokio::test]
async fn venice_serves_the_openai_surface_without_its_house_system_prompt() {
    let client = venice_client(&upstream().await);

    let dispatched = client
        .infer(
            &uncensored_rung(),
            Wire::OpenAi,
            &serde_json::json!({ "model": "uncensored", "messages": [] }),
        )
        .await
        .unwrap();

    assert!(dispatched.status.is_success());
    let echoed: serde_json::Value = serde_json::from_slice(&dispatched.body).unwrap();
    assert_eq!(echoed["sent"]["model"], "venice-uncensored-1.2");
    assert_eq!(
        echoed["sent"]["venice_parameters"]["include_venice_system_prompt"],
        false
    );
}

#[tokio::test]
async fn venice_declines_the_anthropic_surface() {
    let client = venice_client(&upstream().await);

    match client
        .infer(
            &uncensored_rung(),
            Wire::Anthropic,
            &serde_json::json!({ "messages": [] }),
        )
        .await
    {
        Err(Error::UnsupportedWire { provider, wire }) => {
            assert_eq!(provider, "venice");
            assert_eq!(wire, "Anthropic Messages");
        }
        other => panic!("expected UnsupportedWire, got {other:?}"),
    }
}

/// The marketplaces both poll; the direct endpoints are the ones that do not.
#[test]
fn only_the_marketplaces_are_polled_for_market_data() {
    for kind in [ProviderKind::OpenRouter, ProviderKind::Surplus] {
        let client = Client::with_credential(
            "market",
            provider(kind),
            reqwest::Client::new(),
            Some("k".to_string()),
        );
        assert!(client.is_marketplace());
    }
}
