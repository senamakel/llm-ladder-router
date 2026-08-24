//! Unit tests for the proxy's own logic.
//!
//! The request path is covered end to end in `tests/ladder_proxy.rs`, against
//! mock marketplaces. What is left here is the parts that need no upstream:
//! caller authentication and the shape of a routing decision's response.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::ladder::SkipReason;

/// A provider deliberately pointed at a closed loopback port.
///
/// These tests must never reach a real marketplace: the process environment can
/// hold a working credential, and a test that dispatches would spend real money
/// and depend on the network. Port 1 refuses connections immediately.
///
/// The credential is named after a variable nothing sets, and supplied by
/// injection below. Reading a real variable would make these tests pass or fail
/// depending on whether the machine happens to have a key — which is how CI and
/// a developer's shell come to disagree.
const OPEN: &str = r#"
[providers.openrouter]
kind = "openrouter"
base_url = "http://127.0.0.1:1"
api_key_env = "LADDER_TEST_UNSET_KEY"

[[ladders]]
name = "flash"
  [[ladders.rungs]]
  provider = "openrouter"
  model = "m"
"#;

fn state_with(server: &str) -> State {
    let text = format!("[server]\n{server}\n{OPEN}");
    let config = Config::parse(&text).unwrap();
    // Inject the credential so the rung is always reached and always fails at
    // the closed port, whatever the environment holds.
    let credentials = BTreeMap::from([("openrouter".to_string(), "test-key".to_string())]);
    let (_, state) = build_with_credentials(config, &credentials).unwrap();
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
        reasoning_effort: None,
        score_multiplier: 1.0,
        score: None,
    };

    let response = with_routing_headers(
        Response::new(axum::body::Body::empty()),
        "reasoning",
        &chosen,
        2,
        Some("thread-1"),
        true,
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
        reasoning_effort: None,
        score_multiplier: 1.0,
        score: None,
    };

    let response = with_routing_headers(
        Response::new(axum::body::Body::empty()),
        "flash",
        &chosen,
        0,
        None,
        false,
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
    // So a client discovering ladders can tell which endpoint each answers on.
    assert_eq!(body["data"][0]["surface"], "chat");
}

/// The guard that keeps a chat ladder off the embeddings endpoint, and an
/// embeddings ladder off the chat ones. Neither model can answer the other's
/// request, so the pairing is decided here rather than by an upstream 400 at
/// every rung in turn.
#[test]
fn a_surface_answers_only_its_own_wire_formats() {
    assert!(serves(Surface::Chat, Wire::OpenAi));
    assert!(serves(Surface::Chat, Wire::Anthropic));
    // Responses is a chat surface too: a different request shape for the same
    // conversation, which the same ladder of chat models can answer.
    assert!(serves(Surface::Chat, Wire::Responses));
    assert!(!serves(Surface::Chat, Wire::Embeddings));

    assert!(serves(Surface::Embeddings, Wire::Embeddings));
    assert!(!serves(Surface::Embeddings, Wire::OpenAi));
    assert!(!serves(Surface::Embeddings, Wire::Responses));
    // There is no Anthropic embeddings format to serve.
    assert!(!serves(Surface::Embeddings, Wire::Anthropic));

    assert_eq!(surface_name(Surface::Chat), "chat");
    assert_eq!(surface_name(Surface::Embeddings), "embeddings");
}

/// A loopback Surplus good enough for `serve` to start against.
async fn upstream() -> String {
    use axum::routing::{get, post};

    let app = axum::Router::new()
        .route(
            "/api/markets/{model}",
            get(|| async {
                Json(serde_json::json!({
                    "offers": [{
                        "provider": "Z.ai",
                        "price_input_per_1m": 3076.0,
                        "price_output_per_1m": 9668.0,
                        "direct_output_per_1m": 3_740_000.0,
                        "available": true,
                        "healthy": true,
                    }]
                }))
            }),
        )
        .route(
            "/v1/buyer/me",
            get(|| async { Json(serde_json::json!({ "balance_usdc": "74673082" })) }),
        )
        .route(
            "/{prefix}/v1/chat/completions",
            post(|| async { Json(serde_json::json!({ "provider": "Z.ai", "choices": [] })) }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}")
}

/// A port nothing is listening on right now.
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn serving_config(bind: &str, upstream: &str) -> Config {
    Config::parse(&format!(
        r#"
        [server]
        bind = "{bind}"

        [providers.surplus]
        kind = "surplus"
        base_url = "{upstream}"
        api_key_env = "LADDER_TEST_UNSET_KEY"

        [[ladders]]
        name = "flash"
          [[ladders.rungs]]
          provider = "surplus"
          model = "glm-5.2"
          max_cost_per_1m = 0.30
        "#
    ))
    .unwrap()
}

#[tokio::test]
async fn serve_loads_prices_before_it_accepts_traffic() {
    let upstream = upstream().await;
    let port = free_port().await;
    let bind = format!("127.0.0.1:{port}");
    let bind_for_server = bind.clone();

    let credentials = BTreeMap::from([("surplus".to_string(), "test-key".to_string())]);
    let server = tokio::spawn(async move {
        serve_with_credentials(serving_config(&bind_for_server, &upstream), &credentials).await
    });

    // Wait for the port to answer. Because `serve` refreshes before binding, a
    // health check that succeeds proves the price table is already populated —
    // which is the whole point of that ordering.
    let mut answered = false;
    for _ in 0..100 {
        if reqwest::get(format!("http://{bind}/healthz")).await.is_ok() {
            answered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(answered, "server never bound");

    // The first request must route rather than fail for want of price data.
    let response = reqwest::Client::new()
        .post(format!("http://{bind}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "flash",
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-ladder-provider"], "surplus");

    server.abort();
}

#[tokio::test]
async fn serve_reports_an_address_it_cannot_bind() {
    let upstream = upstream().await;
    let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = held.local_addr().unwrap().to_string();

    let error = serve(serving_config(&taken, &upstream)).await.unwrap_err();
    drop(held);

    match error {
        Error::Bind { address, .. } => assert_eq!(address, taken),
        other => panic!("expected Bind, got {other:?}"),
    }
}

#[tokio::test]
async fn a_request_without_a_model_field_is_rejected() {
    let state = state_with("bind = \"127.0.0.1:6969\"");
    let response = route(
        state,
        &HeaderMap::new(),
        serde_json::json!({}),
        Wire::OpenAi,
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_request_naming_an_unknown_ladder_lists_the_known_ones() {
    let state = state_with("bind = \"127.0.0.1:6969\"");
    let response = route(
        state,
        &HeaderMap::new(),
        serde_json::json!({ "model": "nope" }),
        Wire::OpenAi,
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("unknown ladder nope"), "{message}");
    assert!(message.contains("flash"), "{message}");
}

#[tokio::test]
async fn an_unauthenticated_request_never_reaches_a_ladder() {
    let state = state_with("api_key = \"s3cret\"");
    let response = route(
        state,
        &HeaderMap::new(),
        serde_json::json!({ "model": "flash" }),
        Wire::Anthropic,
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_ladder_whose_only_rung_fails_explains_itself() {
    // The rung is uncapped, so it is tried; the upstream port is closed, so it
    // fails, and the ladder runs out.
    let state = state_with("bind = \"127.0.0.1:6969\"");
    refresh_credits_once(&state).await;

    let response = route(
        state,
        &HeaderMap::new(),
        serde_json::json!({ "model": "flash" }),
        Wire::OpenAi,
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let skipped = body["error"]["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["rung"], 0);
    assert_eq!(skipped[0]["provider"], "openrouter");
    assert!(
        skipped[0]["reason"]
            .as_str()
            .unwrap()
            .contains("upstream failed"),
        "{skipped:?}"
    );
}
