//! Unit tests for the background refreshers.
//!
//! These stand up loopback upstreams rather than mocking the client, because
//! what matters is the fail-soft behavior: a refresh that fails must leave the
//! previous snapshot in place instead of stripping a rung of its price data.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Json;
use axum::routing::get;

use super::*;
use crate::pricing::{ModelPrices, Offer};

/// A loopback Surplus that serves an order book until it is told to fail.
async fn upstream(fail_after: usize) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();

    let app = axum::Router::new()
        .route(
            "/api/markets/{model}",
            get(move || {
                let counter = counter.clone();
                async move {
                    let seen = counter.fetch_add(1, Ordering::SeqCst);
                    if seen >= fail_after {
                        return (
                            axum::http::StatusCode::OK,
                            Json(serde_json::json!({ "not": "an order book" })),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        Json(serde_json::json!({
                            "offers": [{
                                "provider": "Z.ai",
                                "price_input_per_1m": 3076.0,
                                "price_output_per_1m": 9668.0,
                                "direct_output_per_1m": 3_740_000.0,
                                "available": true,
                                "healthy": true,
                            }]
                        })),
                    )
                }
            }),
        )
        .route(
            "/v1/buyer/me",
            get(|| async {
                Json(serde_json::json!({
                    "balance_usdc": "74673082",
                    "allowance_usdc": "74673033",
                }))
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("http://{address}"), calls)
}

fn state_for(base_url: &str, credential: Option<&str>) -> State {
    let config = Config::parse(&format!(
        r#"
        [providers.surplus]
        kind = "surplus"
        base_url = "{base_url}"
        api_key_env = "LADDER_TEST_UNSET_KEY"

        [[ladders]]
        name = "flash"
          [[ladders.rungs]]
          provider = "surplus"
          model = "glm-5.2"
          max_cost_per_1m = 0.30
        "#
    ))
    .unwrap();

    let credentials = credential
        .map(|key| BTreeMap::from([("surplus".to_string(), key.to_string())]))
        .unwrap_or_default();
    let (_, state) = build_with_credentials(config, &credentials).unwrap();
    state
}

#[tokio::test]
async fn a_successful_refresh_fills_the_price_table() {
    let (base_url, _) = upstream(usize::MAX).await;
    let state = state_for(&base_url, Some("key"));

    refresh_prices_once(&state).await;

    let prices = state.prices.read().await;
    let model = prices.get("surplus", "glm-5.2").unwrap();
    assert_eq!(model.offers.len(), 1);
    assert!((model.offers[0].completion_per_1m - 0.009_668).abs() < 1e-9);
}

#[tokio::test]
async fn a_failed_refresh_keeps_the_previous_snapshot() {
    // Serves once, then returns a body that does not parse.
    let (base_url, calls) = upstream(1).await;
    let state = state_for(&base_url, Some("key"));

    refresh_prices_once(&state).await;
    assert!(state.prices.read().await.get("surplus", "glm-5.2").is_some());

    refresh_prices_once(&state).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // Stripping the rung of its price data would silently make it unroutable,
    // which is worse than routing on a slightly older snapshot.
    assert!(
        state.prices.read().await.get("surplus", "glm-5.2").is_some(),
        "a failed refresh must not drop the previous prices"
    );
}

#[tokio::test]
async fn a_totally_failed_refresh_leaves_the_table_untouched() {
    let (base_url, _) = upstream(0).await;
    let state = state_for(&base_url, Some("key"));

    // Seed a snapshot the refresher could otherwise clobber.
    state.prices.write().await.insert(
        "surplus",
        "glm-5.2",
        ModelPrices::new(vec![Offer {
            provider: "seeded".to_string(),
            tag: None,
            prompt_per_1m: 0.1,
            completion_per_1m: 0.1,
            direct_completion_per_1m: Some(3.74),
            usable: true,
        }]),
    );

    refresh_prices_once(&state).await;

    let prices = state.prices.read().await;
    assert_eq!(
        prices.get("surplus", "glm-5.2").unwrap().offers[0].provider,
        "seeded"
    );
}

#[tokio::test]
async fn a_balance_poll_records_the_spendable_amount() {
    let (base_url, _) = upstream(usize::MAX).await;
    let state = state_for(&base_url, Some("key"));

    refresh_credits_once(&state).await;

    let credits = state.credits.read().await;
    let balance = credits.balance("surplus").unwrap();
    // The allowance is the lesser of the two, so it is what gets recorded.
    assert!((balance.remaining_usd - 74.673_033).abs() < 1e-6);
    assert!(credits.unusable("surplus", "K", 1.0).is_none());
}

#[tokio::test]
async fn a_provider_without_a_credential_is_marked_unusable() {
    let (base_url, _) = upstream(usize::MAX).await;
    let state = state_for(&base_url, None);

    refresh_credits_once(&state).await;

    let credits = state.credits.read().await;
    assert!(matches!(
        credits.unusable("surplus", "LADDER_TEST_UNSET_KEY", 0.0),
        Some(crate::ladder::SkipReason::MissingCredential { .. })
    ));
}

#[tokio::test]
async fn an_unreadable_balance_leaves_the_provider_usable() {
    // No /v1/buyer/me route at all, so the poll fails to parse.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, axum::Router::new()).await.unwrap();
    });
    let state = state_for(&format!("http://{address}"), Some("key"));

    refresh_credits_once(&state).await;

    // Taking a provider out of service because its balance endpoint hiccuped
    // would be a self-inflicted outage.
    let credits = state.credits.read().await;
    assert!(credits.balance("surplus").is_none());
    assert!(credits.unusable("surplus", "K", 99.0).is_none());
}
