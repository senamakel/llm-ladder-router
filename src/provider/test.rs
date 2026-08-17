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
    assert_eq!(
        classify_status(reqwest::StatusCode::UNAUTHORIZED),
        Disposition::CallerError
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::UNPROCESSABLE_ENTITY),
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
