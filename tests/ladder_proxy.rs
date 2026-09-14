//! End-to-end tests against mock marketplaces.
//!
//! Each test stands up loopback HTTP servers that impersonate Surplus and
//! `OpenRouter`, points a real router at them, and drives it through its own
//! public HTTP surface. That covers what unit tests cannot: what actually
//! reaches the wire, and whether a failing rung really advances the ladder.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

/// What a mock upstream should do when a completion arrives.
#[derive(Debug, Clone)]
enum Behavior {
    /// Serve normally, naming this sub-provider.
    Serve(String),
    /// Fail with a status and body, standing in for a real marketplace error.
    Fail(StatusCode, String),
}

/// Everything a mock upstream recorded, so a test can assert on the wire.
#[derive(Debug, Default)]
struct Recorded {
    /// The JSON bodies of every completion request received.
    bodies: Vec<serde_json::Value>,
    /// The paths every completion request arrived on.
    paths: Vec<String>,
}

#[derive(Clone)]
struct MockState {
    behavior: Behavior,
    /// USD per million output tokens quoted by every seller this mock lists.
    price_per_1m: f64,
    recorded: Arc<Mutex<Recorded>>,
}

/// Starts a mock Surplus and returns its base URL and recorder.
async fn mock_surplus(behavior: Behavior, price_per_1m: f64) -> (String, Arc<Mutex<Recorded>>) {
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let state = MockState {
        behavior,
        price_per_1m,
        recorded: recorded.clone(),
    };

    let app = Router::new()
        .route("/api/markets/{model}", get(surplus_order_book))
        .route("/v1/buyer/me", get(surplus_balance))
        // Both the plain and the discount-prefixed paths on both surfaces,
        // because which one the router picks is exactly what these tests check.
        .route("/v1/chat/completions", post(surplus_completions))
        .route("/{prefix}/v1/chat/completions", post(surplus_completions))
        .route("/anthropic/v1/messages", post(surplus_completions))
        .route("/anthropic/{prefix}/v1/messages", post(surplus_completions))
        .route("/v1/responses", post(surplus_completions))
        .route("/{prefix}/v1/responses", post(surplus_completions))
        .route("/v1/embeddings", post(surplus_embeddings))
        .with_state(state);

    (serve(app).await, recorded)
}

/// Starts a mock `OpenRouter` and returns its base URL and recorder.
async fn mock_openrouter(behavior: Behavior, price_per_1m: f64) -> (String, Arc<Mutex<Recorded>>) {
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let state = MockState {
        behavior,
        price_per_1m,
        recorded: recorded.clone(),
    };

    let app = Router::new()
        .route(
            "/models/{author}/{model}/endpoints",
            get(openrouter_endpoints),
        )
        .route("/credits", get(openrouter_credits))
        .route("/chat/completions", post(openrouter_completions))
        .route("/messages", post(openrouter_completions))
        .route("/responses", post(openrouter_completions))
        .with_state(state);

    (format!("{}/", serve(app).await), recorded)
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}")
}

async fn surplus_order_book(
    State(state): State<MockState>,
    Path(_model): Path<String>,
) -> Json<serde_json::Value> {
    // Micro-USD per million tokens, matching the real order book's units.
    let micro = state.price_per_1m * 1_000_000.0;
    Json(serde_json::json!({
        "offers": [{
            "provider": "Z.ai",
            "price_input_per_1m": micro / 2.0,
            "price_output_per_1m": micro,
            "direct_output_per_1m": 3_740_000.0,
            "available": true,
            "healthy": true,
        }]
    }))
}

async fn surplus_balance() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "balance_usdc": "74673082",
        "allowance_usdc": "74673033",
    }))
}

async fn surplus_completions(
    State(state): State<MockState>,
    uri: axum::http::Uri,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    {
        let mut recorded = state.recorded.lock().unwrap();
        recorded.bodies.push(body);
        recorded.paths.push(uri.path().to_string());
    }
    respond(&state.behavior)
}

async fn surplus_embeddings(
    State(state): State<MockState>,
    uri: axum::http::Uri,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    {
        let mut recorded = state.recorded.lock().unwrap();
        recorded.bodies.push(body);
        recorded.paths.push(uri.path().to_string());
    }
    match &state.behavior {
        Behavior::Serve(provider) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "object": "list",
                "provider": provider,
                "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }],
                "usage": { "prompt_tokens": 3, "total_tokens": 3 },
            })),
        ),
        behavior @ Behavior::Fail(..) => respond(behavior),
    }
}

async fn openrouter_endpoints(State(state): State<MockState>) -> Json<serde_json::Value> {
    // OpenRouter quotes USD per token, so the per-million figure is scaled down.
    let per_token = state.price_per_1m / 1_000_000.0;
    Json(serde_json::json!({
        "data": {
            "endpoints": [
                {
                    "provider_name": "DeepInfra",
                    "tag": "deepinfra",
                    "pricing": {
                        "prompt": per_token.to_string(),
                        "completion": per_token.to_string(),
                    },
                    "status": 0,
                },
                {
                    "provider_name": "DigitalOcean",
                    "tag": "digitalocean",
                    "pricing": {
                        "prompt": per_token.to_string(),
                        "completion": per_token.to_string(),
                    },
                    "status": 0,
                },
            ]
        }
    }))
}

async fn openrouter_credits() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "data": { "total_credits": 20, "total_usage": 8.0 } }))
}

async fn openrouter_completions(
    State(state): State<MockState>,
    uri: axum::http::Uri,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    {
        let mut recorded = state.recorded.lock().unwrap();
        recorded.bodies.push(body);
        recorded.paths.push(uri.path().to_string());
    }
    respond(&state.behavior)
}

fn respond(behavior: &Behavior) -> (StatusCode, Json<serde_json::Value>) {
    match behavior {
        Behavior::Serve(provider) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "provider": provider,
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "hello" },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 },
            })),
        ),
        Behavior::Fail(status, body) => (
            *status,
            Json(serde_json::json!({ "error": { "message": body, "code": body } })),
        ),
    }
}

/// Builds a two-rung flash ladder pointed at the two mocks.
fn config_for(surplus: &str, openrouter: &str, surplus_cap: f64) -> String {
    format!(
        r#"
        [credits]
        min_balance_usd = 0.5

        [providers.surplus]
        kind = "surplus"
        base_url = "{surplus}"
        api_key_env = "TEST_SURPLUS_KEY"

        [providers.openrouter]
        kind = "open_router"
        base_url = "{openrouter}"
        api_key_env = "TEST_OPENROUTER_KEY"

        [[ladders]]
        name = "flash"
        aliases = ["chat-v1"]

          [[ladders.rungs]]
          provider = "surplus"
          model = "deepseek-v4-flash"
          max_cost_per_1m = {surplus_cap}

          [[ladders.rungs]]
          provider = "openrouter"
          model = "deepseek/deepseek-v4-flash"
          max_cost_per_1m = 0.30
          prefer = ["deepinfra"]
        "#
    )
}

/// Starts a router against the two mocks and returns its base URL.
async fn start_router(config: &str) -> String {
    // Both providers need a credential or every rung is skipped. Injecting
    // them beats mutating the process environment, which is global state
    // shared with every other test in this binary.
    let credentials = std::collections::BTreeMap::from([
        ("surplus".to_string(), "test-surplus".to_string()),
        ("openrouter".to_string(), "test-openrouter".to_string()),
    ]);

    let config = llm_ladder_router::Config::parse(config).unwrap();
    let (app, state) =
        llm_ladder_router::proxy::build_with_credentials(config, &credentials).unwrap();

    // Refresh synchronously so the first request routes on real data rather
    // than racing the background loops.
    llm_ladder_router::proxy::refresh_prices_once(&state).await;
    llm_ladder_router::proxy::refresh_credits_once(&state).await;

    serve(app).await
}

async fn ask(router: &str, ladder: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{router}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": ladder,
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 8,
        }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn serves_from_the_first_rung_and_says_which_one_it_used() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, or_recorded) =
        mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = ask(&router, "flash").await;

    assert_eq!(response.status(), 200);
    let headers = response.headers().clone();
    assert_eq!(headers["x-ladder-name"], "flash");
    assert_eq!(headers["x-ladder-rung"], "0");
    assert_eq!(headers["x-ladder-provider"], "surplus");
    assert_eq!(headers["x-ladder-model"], "deepseek-v4-flash");
    assert_eq!(headers["x-ladder-sub-provider"], "Z.ai");
    assert_eq!(headers["x-ladder-skipped"], "0");

    // The backstop must not have been touched.
    assert!(or_recorded.lock().unwrap().bodies.is_empty());
}

#[tokio::test]
async fn skips_a_rung_priced_above_its_ceiling_without_calling_it() {
    // Surplus quotes 0.40 against a 0.15 ceiling, so it should never be asked.
    let (surplus, sp_recorded) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.40).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = ask(&router, "flash").await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-ladder-provider"], "openrouter");
    assert_eq!(response.headers()["x-ladder-rung"], "1");
    assert_eq!(response.headers()["x-ladder-skipped"], "1");

    // The whole point of local admission: the doomed rung cost no round trip.
    assert!(
        sp_recorded.lock().unwrap().bodies.is_empty(),
        "a rung priced out of its ceiling must not be dispatched to"
    );
}

#[tokio::test]
async fn advances_the_ladder_when_the_first_rung_fails_upstream() {
    let (surplus, sp_recorded) = mock_surplus(
        Behavior::Fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "all sellers unhealthy".to_string(),
        ),
        0.10,
    )
    .await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = ask(&router, "flash").await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-ladder-provider"], "openrouter");
    // It was tried, and it did fail, before the ladder moved on.
    assert_eq!(sp_recorded.lock().unwrap().bodies.len(), 1);
}

#[tokio::test]
async fn a_surplus_discount_rejection_advances_the_ladder() {
    let (surplus, _) = mock_surplus(
        Behavior::Fail(
            StatusCode::NOT_FOUND,
            "minimum_discount_not_met".to_string(),
        ),
        0.10,
    )
    .await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = ask(&router, "flash").await;

    // A 404 would normally read as a caller error; for Surplus it means the
    // discount filter matched nothing, which is a reason to step down.
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-ladder-provider"], "openrouter");
}

#[tokio::test]
async fn an_openrouter_max_price_rejection_advances_the_ladder() {
    let (surplus, _) = mock_surplus(
        Behavior::Fail(StatusCode::SERVICE_UNAVAILABLE, "down".to_string()),
        0.10,
    )
    .await;
    let (openrouter, _) = mock_openrouter(
        Behavior::Fail(
            StatusCode::NOT_FOUND,
            "No endpoints found that satisfy the max price for this request".to_string(),
        ),
        0.20,
    )
    .await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = ask(&router, "flash").await;

    // Both rungs refused, so the ladder is exhausted and says so per rung.
    assert_eq!(response.status(), 502);
    let body: serde_json::Value = response.json().await.unwrap();
    let skipped = body["error"]["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 2);
    assert!(skipped.iter().all(|entry| {
        entry["reason"]
            .as_str()
            .unwrap()
            .contains("upstream failed")
    }));
}

#[tokio::test]
async fn a_caller_error_is_returned_without_being_replayed() {
    let (surplus, _) = mock_surplus(
        Behavior::Fail(
            StatusCode::BAD_REQUEST,
            "messages must not be empty".to_string(),
        ),
        0.10,
    )
    .await;
    let (openrouter, or_recorded) =
        mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = ask(&router, "flash").await;

    assert_eq!(response.status(), 400);
    // Replaying a request the caller got wrong would fail identically and be
    // charged twice.
    assert!(
        or_recorded.lock().unwrap().bodies.is_empty(),
        "a caller error must not be retried at the next rung"
    );
}

/// The outage this was written for: Surplus answered `403 Forbidden` from its
/// own edge for about fifteen minutes, every ladder handed it straight back,
/// and five long agent runs died inside the same minute with a working second
/// provider sitting one rung below.
#[tokio::test]
async fn a_provider_refusing_this_router_advances_the_ladder() {
    let (surplus, sp_recorded) = mock_surplus(
        Behavior::Fail(StatusCode::FORBIDDEN, "Forbidden".to_string()),
        0.10,
    )
    .await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = ask(&router, "flash").await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-ladder-provider"], "openrouter");
    // Two authentications are in play and they are not the same one: the
    // caller authenticated to this router, and the credential the marketplace
    // rejected is the router's own. So this is a rung that cannot serve, not a
    // request that cannot be made.
    assert_eq!(sp_recorded.lock().unwrap().bodies.len(), 1);
}

#[tokio::test]
async fn a_refused_rung_is_parked_so_the_next_request_skips_it() {
    let (surplus, sp_recorded) = mock_surplus(
        Behavior::Fail(StatusCode::UNAUTHORIZED, "Unauthorized".to_string()),
        0.10,
    )
    .await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    for _ in 0..3 {
        assert_eq!(ask(&router, "flash").await.status(), 200);
    }

    // A marketplace whose edge is refusing does so for minutes. Without the
    // cooldown every request in that window pays a failed round trip to
    // rediscover it, which is the same waste a 429 is parked for.
    assert_eq!(
        sp_recorded.lock().unwrap().bodies.len(),
        1,
        "a refused rung must be tried once, then skipped while it cools down"
    );
}

#[tokio::test]
async fn a_surplus_ceiling_travels_as_a_discount_prefix_in_the_path() {
    let (surplus, sp_recorded) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    assert_eq!(ask(&router, "flash").await.status(), 200);

    let recorded = sp_recorded.lock().unwrap();
    // A 0.15 ceiling against a 3.74 direct price is a 95.9% discount, floored
    // to 95. The ceiling binds through the path, never the body.
    assert_eq!(recorded.paths[0], "/min95/v1/chat/completions");
    assert_eq!(recorded.bodies[0]["model"], "deepseek-v4-flash");
    assert!(
        recorded.bodies[0].get("provider").is_none(),
        "OpenRouter's provider object must never reach a Surplus endpoint"
    );
}

#[tokio::test]
async fn an_openrouter_ceiling_travels_in_the_provider_object() {
    let (surplus, _) = mock_surplus(
        Behavior::Fail(StatusCode::SERVICE_UNAVAILABLE, "down".to_string()),
        0.10,
    )
    .await;
    let (openrouter, or_recorded) =
        mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    assert_eq!(ask(&router, "flash").await.status(), 200);

    let recorded = or_recorded.lock().unwrap();
    let body = &recorded.bodies[0];
    assert_eq!(recorded.paths[0], "/chat/completions");
    assert_eq!(body["model"], "deepseek/deepseek-v4-flash");
    assert_eq!(body["provider"]["max_price"]["completion"], 0.30);
    assert_eq!(body["provider"]["order"][0], "deepinfra");
    // Never an exclusive pin: it has been observed to hang while idle
    // sub-providers sat unused.
    assert_eq!(body["provider"]["allow_fallbacks"], true);
    assert!(body["provider"].get("only").is_none());
}

#[tokio::test]
async fn the_callers_own_parameters_survive_the_round_trip() {
    let (surplus, sp_recorded) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    reqwest::Client::new()
        .post(format!("{router}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "flash",
            "messages": [{ "role": "user", "content": "hi" }],
            "temperature": 0.2,
            "tools": [{ "type": "function", "function": { "name": "noop" } }],
        }))
        .send()
        .await
        .unwrap();

    let recorded = sp_recorded.lock().unwrap();
    let body = &recorded.bodies[0];
    // Only the routing fields are rewritten; anything the router does not model
    // must pass through untouched.
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["tools"][0]["function"]["name"], "noop");
    assert_eq!(body["messages"][0]["content"], "hi");
}

#[tokio::test]
async fn an_unknown_ladder_is_rejected_and_lists_the_known_ones() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = ask(&router, "nonexistent").await;

    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("unknown ladder nonexistent"), "{message}");
    assert!(message.contains("flash"), "{message}");
}

#[tokio::test]
async fn the_ladders_are_advertised_as_models() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let body: serde_json::Value = reqwest::get(format!("{router}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["data"][0]["id"], "flash");
    assert_eq!(body["data"][0]["rungs"], 2);
}

/// The same two-rung ladder, but behind a caller key.
fn config_with_key(surplus: &str, openrouter: &str, key: &str) -> String {
    config_for(surplus, openrouter, 0.15).replace(
        "[credits]",
        &format!("[server]\napi_key = \"{key}\"\n\n[credits]"),
    )
}

#[tokio::test]
async fn a_configured_key_is_required_and_accepted_under_either_header() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_with_key(&surplus, &openrouter, "s3cret")).await;

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "flash",
        "messages": [{ "role": "user", "content": "hi" }],
    });

    let unauthenticated = client
        .post(format!("{router}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), 401);

    let wrong = client
        .post(format!("{router}/v1/chat/completions"))
        .header("authorization", "Bearer wrong")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let bearer = client
        .post(format!("{router}/v1/chat/completions"))
        .header("authorization", "Bearer s3cret")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(bearer.status(), 200);

    // Anthropic clients send the key this way; both surfaces accept both.
    let x_api_key = client
        .post(format!("{router}/v1/chat/completions"))
        .header("x-api-key", "s3cret")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(x_api_key.status(), 200);
}

#[tokio::test]
async fn the_anthropic_surface_routes_the_same_ladders() {
    let (surplus, sp_recorded) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = reqwest::Client::new()
        .post(format!("{router}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": "flash",
            "max_tokens": 16,
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-ladder-provider"], "surplus");
    assert_eq!(response.headers()["x-ladder-rung"], "0");

    let recorded = sp_recorded.lock().unwrap();
    // The discount prefix sits after `/anthropic` on this surface, which is the
    // only ordering the live API accepts.
    assert_eq!(recorded.paths[0], "/anthropic/min95/v1/messages");
    assert_eq!(recorded.bodies[0]["model"], "deepseek-v4-flash");
    assert_eq!(recorded.bodies[0]["max_tokens"], 16);
}

#[tokio::test]
async fn the_anthropic_surface_falls_through_to_the_backstop_too() {
    let (surplus, _) = mock_surplus(
        Behavior::Fail(
            StatusCode::NOT_FOUND,
            "minimum_discount_not_met".to_string(),
        ),
        0.10,
    )
    .await;
    let (openrouter, or_recorded) =
        mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = reqwest::Client::new()
        .post(format!("{router}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": "flash",
            "max_tokens": 16,
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-ladder-provider"], "openrouter");

    let recorded = or_recorded.lock().unwrap();
    // OpenRouter's Anthropic surface lives at /messages and still takes the
    // ceiling in the body.
    assert_eq!(recorded.paths[0], "/messages");
    assert_eq!(
        recorded.bodies[0]["provider"]["max_price"]["completion"],
        0.30
    );
}

#[tokio::test]
async fn the_responses_surface_routes_the_same_ladders() {
    let (surplus, sp_recorded) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = reqwest::Client::new()
        .post(format!("{router}/v1/responses"))
        .json(&serde_json::json!({
            "model": "flash",
            "input": "hi",
            "max_output_tokens": 16,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-ladder-provider"], "surplus");
    assert_eq!(response.headers()["x-ladder-rung"], "0");

    let recorded = sp_recorded.lock().unwrap();
    // Surplus puts this surface under the same root as chat completions, with
    // the discount prefix leading — not under `/anthropic`.
    assert_eq!(recorded.paths[0], "/min95/v1/responses");
    assert_eq!(recorded.bodies[0]["model"], "deepseek-v4-flash");
    // A responses-shaped body is relayed as it stands rather than translated
    // into a messages array.
    assert_eq!(recorded.bodies[0]["input"], "hi");
    assert_eq!(recorded.bodies[0]["max_output_tokens"], 16);
}

#[tokio::test]
async fn the_responses_surface_falls_through_to_the_backstop_too() {
    let (surplus, _) = mock_surplus(
        Behavior::Fail(
            StatusCode::NOT_FOUND,
            "minimum_discount_not_met".to_string(),
        ),
        0.10,
    )
    .await;
    let (openrouter, or_recorded) =
        mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = reqwest::Client::new()
        .post(format!("{router}/v1/responses"))
        .json(&serde_json::json!({ "model": "flash", "input": "hi" }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-ladder-provider"], "openrouter");

    let recorded = or_recorded.lock().unwrap();
    // OpenRouter's responses surface lives at /responses and still takes the
    // ceiling in the body, exactly as its other two surfaces do.
    assert_eq!(recorded.paths[0], "/responses");
    assert_eq!(
        recorded.bodies[0]["provider"]["max_price"]["completion"],
        0.30
    );
}

/// Records which sub-provider each mock call was steered to.
fn steered_to(recorded: &Arc<Mutex<Recorded>>, index: usize) -> Option<String> {
    recorded.lock().unwrap().bodies[index]["provider"]["order"][0]
        .as_str()
        .map(str::to_string)
}

#[tokio::test]
async fn a_session_stays_on_the_rung_that_served_it() {
    // Surplus is affordable, so an unpinned request takes rung 0.
    let (surplus, sp_recorded) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let client = reqwest::Client::new();
    let ask = |session: &str| {
        let client = client.clone();
        let router = router.clone();
        let session = session.to_string();
        async move {
            client
                .post(format!("{router}/v1/chat/completions"))
                .header("x-ladder-session", session)
                .json(&serde_json::json!({
                    "model": "flash",
                    "messages": [{ "role": "user", "content": "hi" }],
                }))
                .send()
                .await
                .unwrap()
        }
    };

    let first = ask("thread-1").await;
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["x-ladder-rung"], "0");
    assert_eq!(first.headers()["x-ladder-session"], "thread-1");
    // Nothing was pinned yet when this one was routed.
    assert_eq!(first.headers()["x-ladder-pinned"], "false");

    let second = ask("thread-1").await;
    assert_eq!(second.status(), 200);
    assert_eq!(second.headers()["x-ladder-rung"], "0");
    assert_eq!(second.headers()["x-ladder-pinned"], "true");

    assert_eq!(sp_recorded.lock().unwrap().bodies.len(), 2);
}

#[tokio::test]
async fn a_pinned_session_is_steered_back_to_its_sub_provider() {
    let (surplus, _) = mock_surplus(
        Behavior::Fail(StatusCode::SERVICE_UNAVAILABLE, "down".to_string()),
        0.10,
    )
    .await;
    // The rung prefers "deepinfra", but the marketplace actually serves from
    // DigitalOcean — so that is where the warm cache lives.
    let (openrouter, or_recorded) =
        mock_openrouter(Behavior::Serve("DigitalOcean".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let client = reqwest::Client::new();
    for _ in 0..2 {
        let response = client
            .post(format!("{router}/v1/chat/completions"))
            .header("x-ladder-session", "thread-2")
            .json(&serde_json::json!({
                "model": "flash",
                "messages": [{ "role": "user", "content": "hi" }],
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }

    // The first call steers by the rung's configured preference. The second
    // leads with the sub-provider that actually served, resolved from the
    // display name the marketplace reported to the slug it steers on.
    assert_eq!(steered_to(&or_recorded, 0).as_deref(), Some("deepinfra"));
    assert_eq!(steered_to(&or_recorded, 1).as_deref(), Some("digitalocean"));

    // The configured preference is kept as a fallback behind the warm one.
    let second = &or_recorded.lock().unwrap().bodies[1];
    assert_eq!(second["provider"]["order"][1], "deepinfra");
    assert_eq!(second["provider"]["allow_fallbacks"], true);
}

#[tokio::test]
async fn two_sessions_are_pinned_independently() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let client = reqwest::Client::new();
    for session in ["thread-a", "thread-b"] {
        for _ in 0..2 {
            let response = client
                .post(format!("{router}/v1/chat/completions"))
                .header("x-ladder-session", session)
                .json(&serde_json::json!({
                    "model": "flash",
                    "messages": [{ "role": "user", "content": "hi" }],
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.headers()["x-ladder-session"], session);
        }
    }
}

#[tokio::test]
async fn a_request_without_a_session_is_not_pinned() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = ask(&router, "flash").await;

    assert_eq!(response.status(), 200);
    assert!(response.headers().get("x-ladder-session").is_none());
    assert!(response.headers().get("x-ladder-pinned").is_none());
}

#[tokio::test]
async fn the_session_can_come_from_the_bodys_own_identifiers() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let client = reqwest::Client::new();

    // OpenAI's `user`, so an unmodified client still gets sticky routing.
    let openai = client
        .post(format!("{router}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "flash",
            "user": "customer-7",
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(openai.headers()["x-ladder-session"], "customer-7");

    // Anthropic's `metadata.user_id`.
    let anthropic = client
        .post(format!("{router}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": "flash",
            "max_tokens": 16,
            "metadata": { "user_id": "customer-9" },
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(anthropic.headers()["x-ladder-session"], "customer-9");
}

#[tokio::test]
async fn claude_code_session_headers_keep_a_conversation_pinned() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let client = reqwest::Client::new();
    for pinned in ["false", "true"] {
        let response = client
            .post(format!("{router}/v1/messages"))
            .header("x-claude-code-session-id", "claude-thread-1")
            .json(&serde_json::json!({
                "model": "flash",
                "max_tokens": 16,
                "messages": [{ "role": "user", "content": "hi" }],
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-ladder-session"], "claude-thread-1");
        assert_eq!(response.headers()["x-ladder-pinned"], pinned);
    }
}

#[tokio::test]
async fn codex_session_identifiers_keep_a_conversation_pinned() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let client = reqwest::Client::new();
    for (header, session, prompt_cache_key) in [
        (Some("session-id"), "codex-session-1", None),
        (Some("thread-id"), "codex-thread-1", None),
        (None, "codex-cache-1", Some("codex-cache-1")),
    ] {
        for pinned in ["false", "true"] {
            let mut request =
                client
                    .post(format!("{router}/v1/responses"))
                    .json(&serde_json::json!({
                        "model": "flash",
                        "prompt_cache_key": prompt_cache_key,
                        "input": "hi",
                    }));
            if let Some(header) = header {
                request = request.header(header, session);
            }
            let response = request.send().await.unwrap();

            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["x-ladder-session"], session);
            assert_eq!(response.headers()["x-ladder-pinned"], pinned);
        }
    }
}

#[tokio::test]
async fn a_pin_never_survives_its_rung_being_priced_out() {
    // Surplus starts affordable at 0.10 against a 0.15 ceiling.
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;

    // A ceiling below what Surplus quotes: the pin cannot rescue rung 0.
    let router = start_router(&config_for(&surplus, &openrouter, 0.05)).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{router}/v1/chat/completions"))
        .header("x-ladder-session", "thread-3")
        .json(&serde_json::json!({
            "model": "flash",
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .unwrap();

    // The budget wins over stickiness, every time.
    assert_eq!(response.headers()["x-ladder-provider"], "openrouter");
}

/// A 429 advances the ladder like any other upstream failure — and, unlike any
/// other, is remembered: the next request does not spend a round trip finding
/// out the same thing.
#[tokio::test]
async fn a_rate_limited_rung_is_not_asked_again_while_it_cools() {
    let (surplus, sp_recorded) = mock_surplus(
        Behavior::Fail(StatusCode::TOO_MANY_REQUESTS, "rate limited".to_string()),
        0.10,
    )
    .await;
    let (openrouter, or_recorded) =
        mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let first = ask(&router, "flash").await;
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["x-ladder-provider"], "openrouter");

    let second = ask(&router, "flash").await;
    assert_eq!(second.status(), 200);
    assert_eq!(second.headers()["x-ladder-provider"], "openrouter");

    // Asked once, then parked: the second request went straight past it.
    assert_eq!(sp_recorded.lock().unwrap().bodies.len(), 1);
    assert_eq!(or_recorded.lock().unwrap().bodies.len(), 2);
}

/// A rung that broke is not a rung that refused. A 503 is re-tested on the next
/// request, because an upstream having a bad second is not the same as one
/// deliberately throttling for a minute.
#[tokio::test]
async fn a_broken_rung_is_tried_again_on_the_next_request() {
    let (surplus, sp_recorded) = mock_surplus(
        Behavior::Fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "all sellers unhealthy".to_string(),
        ),
        0.10,
    )
    .await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    ask(&router, "flash").await;
    ask(&router, "flash").await;

    assert_eq!(sp_recorded.lock().unwrap().bodies.len(), 2);
}

/// An embeddings ladder walks the same machinery as a chat one, and reaches the
/// surface the caller asked for rather than the chat endpoint.
#[tokio::test]
async fn an_embeddings_request_reaches_the_embeddings_endpoint() {
    let (surplus, recorded) = mock_surplus(Behavior::Serve("Venice AI".to_string()), 0.001).await;
    let router = start_router(&embeddings_config(&surplus)).await;

    let response = reqwest::Client::new()
        .post(format!("{router}/v1/embeddings"))
        .json(&serde_json::json!({ "model": "vectors", "input": "hello" }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers().get("x-ladder-model").unwrap(),
        "venice-embed-1"
    );
    // No ceiling can be enforced on this surface, so none is claimed.
    assert!(response.headers().get("x-ladder-cap-per-1m").is_none());

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["data"][0]["object"], "embedding");

    let recorded = recorded.lock().unwrap();
    // Never a `/min{N}/` prefix: every prefixed spelling 404s on the live API.
    assert_eq!(recorded.paths, vec!["/v1/embeddings".to_string()]);
    // The caller's body is relayed with only the model rewritten.
    assert_eq!(recorded.bodies[0]["input"], "hello");
    assert_eq!(recorded.bodies[0]["model"], "venice-embed-1");
}

/// A chat model cannot answer an embeddings request, so the mismatch is refused
/// at the door rather than walked down a ladder that would fail identically at
/// every rung and bill for each attempt.
#[tokio::test]
async fn a_ladder_declared_for_one_surface_refuses_the_other() {
    let (surplus, recorded) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.001).await;
    let router = start_router(&embeddings_config(&surplus)).await;
    let client = reqwest::Client::new();

    // The embeddings ladder, asked for a chat completion.
    let response = client
        .post(format!("{router}/v1/chat/completions"))
        .json(&serde_json::json!({ "model": "vectors", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("embeddings"), "{message}");

    // And the chat ladder, asked for embeddings.
    let response = client
        .post(format!("{router}/v1/embeddings"))
        .json(&serde_json::json!({ "model": "prose", "input": "hello" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);

    // Neither reached an upstream.
    assert!(recorded.lock().unwrap().paths.is_empty());
}

/// One provider, two ladders: the embeddings one and a chat one to mismatch it
/// against.
fn embeddings_config(surplus: &str) -> String {
    format!(
        r#"
        [credits]
        min_balance_usd = 0.5

        [providers.surplus]
        kind = "surplus"
        base_url = "{surplus}"
        api_key_env = "TEST_SURPLUS_KEY"

        [[ladders]]
        name = "vectors"
        surface = "embeddings"

          [[ladders.rungs]]
          provider = "surplus"
          model = "venice-embed-1"

        [[ladders]]
        name = "prose"

          [[ladders.rungs]]
          provider = "surplus"
          model = "deepseek-v4-flash"
        "#
    )
}

#[tokio::test]
async fn serves_a_request_that_names_the_ladder_by_an_alias() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = ask(&router, "chat-v1").await;

    assert_eq!(response.status(), 200);
    // The ladder reports itself under its own name, so a log line stays
    // comparable however the caller spelled the request.
    assert_eq!(response.headers()["x-ladder-name"], "flash");
    assert_eq!(response.headers()["x-ladder-model"], "deepseek-v4-flash");
}

#[tokio::test]
async fn serves_a_request_carrying_a_context_variant_marker() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    for name in ["flash[1m]", "chat-v1[1m]", "Flash"] {
        let response = ask(&router, name).await;

        assert_eq!(response.status(), 200, "{name} should have routed");
        assert_eq!(response.headers()["x-ladder-name"], "flash");
    }
}

#[tokio::test]
async fn a_name_that_matches_no_ladder_lists_every_spelling_that_would_have() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let response = ask(&router, "no-such-ladder").await;

    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap().to_string();
    assert!(message.contains("no-such-ladder"), "{message}");
    assert!(message.contains("flash"), "{message}");
    assert!(message.contains("chat-v1"), "{message}");
}

#[tokio::test]
async fn the_model_list_advertises_every_name_a_ladder_answers_to() {
    let (surplus, _) = mock_surplus(Behavior::Serve("Z.ai".to_string()), 0.10).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&config_for(&surplus, &openrouter, 0.15)).await;

    let body: serde_json::Value = reqwest::get(format!("{router}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let flash = &body["data"][0];
    assert_eq!(flash["id"], "flash");
    assert_eq!(flash["aliases"][0], "chat-v1");
}

/// A mock Surplus for the media surfaces: an order book quoted per image or
/// per job, the two generation routes with and without a discount prefix, and
/// a video job that can be polled and cancelled.
async fn mock_surplus_media(
    behavior: Behavior,
    price_per_unit: f64,
) -> (String, Arc<Mutex<Recorded>>) {
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let state = MockState {
        behavior,
        price_per_1m: price_per_unit,
        recorded: recorded.clone(),
    };

    let app = Router::new()
        .route("/api/markets/{model}", get(surplus_media_order_book))
        .route("/v1/buyer/me", get(surplus_balance))
        .route("/v1/images/generations", post(surplus_images))
        .route("/{prefix}/v1/images/generations", post(surplus_images))
        .route("/v1/video/generations", post(surplus_video))
        .route("/{prefix}/v1/video/generations", post(surplus_video))
        .route(
            "/v1/video/generations/{id}",
            get(surplus_video_job).delete(surplus_video_job),
        )
        .with_state(state);

    (serve(app).await, recorded)
}

async fn surplus_media_order_book(
    State(state): State<MockState>,
    Path(model): Path<String>,
) -> Json<serde_json::Value> {
    // Micro-USD per unit, with the per-token fields at zero, which is how the
    // real order book quotes every image and video model. The direct prices
    // are the live ones for `seedream-4.5` ($0.04 an image) and
    // `kling-o3-pro-text-to-video` ($0.45 a job).
    let micro = state.price_per_1m * 1_000_000.0;
    let (unit, direct) = if model.contains("video") {
        ("job", 450_000.0)
    } else {
        ("image", 40_000.0)
    };
    Json(serde_json::json!({
        "offers": [{
            "provider": "Venice AI",
            "price_input_per_1m": 0,
            "price_output_per_1m": 0,
            "direct_output_per_1m": 0,
            "media_unit_price": micro,
            "direct_media_unit_price": direct,
            "media_unit": unit,
            "available": true,
            "healthy": true,
        }]
    }))
}

async fn surplus_images(
    State(state): State<MockState>,
    uri: axum::http::Uri,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    {
        let mut recorded = state.recorded.lock().unwrap();
        recorded.bodies.push(body);
        recorded.paths.push(uri.path().to_string());
    }
    match &state.behavior {
        Behavior::Serve(provider) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "created": 1_789_331_147,
                "provider": provider,
                "data": [{ "b64_json": "aGk=" }],
            })),
        ),
        behavior @ Behavior::Fail(..) => respond(behavior),
    }
}

/// The `media.job` shape the live API answers a video submission with.
fn video_job(status: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "01M2E752E5S0GQ5AYRJWB8WGD1",
        "object": "media.job",
        "kind": "video",
        "status": status,
        "poll_url": "https://api.surplusintelligence.ai/v1/video/generations/01M2E752E5S0GQ5AYRJWB8WGD1",
        "served_by": "api.venice.ai",
    })
}

async fn surplus_video(
    State(state): State<MockState>,
    uri: axum::http::Uri,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    {
        let mut recorded = state.recorded.lock().unwrap();
        recorded.bodies.push(body);
        recorded.paths.push(uri.path().to_string());
    }
    match &state.behavior {
        // The live API answers a queued job with 202, and the router relays
        // the status as it relays everything else.
        Behavior::Serve(_) => (StatusCode::ACCEPTED, Json(video_job("queued"))),
        behavior @ Behavior::Fail(..) => respond(behavior),
    }
}

/// Polls or cancels the one job this mock knows about; any other id is the
/// live API's 404.
async fn surplus_video_job(
    State(state): State<MockState>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    state
        .recorded
        .lock()
        .unwrap()
        .paths
        .push(format!("{method} {}", uri.path()));
    if id != "01M2E752E5S0GQ5AYRJWB8WGD1" {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": { "code": "not_found" } })),
        );
    }
    let status = if method == axum::http::Method::DELETE {
        "canceled"
    } else {
        "completed"
    };
    (StatusCode::OK, Json(video_job(status)))
}

/// A media Surplus beside an `OpenRouter`, with an images ladder that names a
/// rung on each, a video ladder, and a chat ladder to mismatch against. The
/// `OpenRouter` rung goes first so a test can see it declined.
fn media_config(surplus: &str, openrouter: &str) -> String {
    format!(
        r#"
        [credits]
        min_balance_usd = 0.5

        [providers.surplus]
        kind = "surplus"
        base_url = "{surplus}"
        api_key_env = "TEST_SURPLUS_KEY"
        max_cost_per_1m = 1.00

        [providers.openrouter]
        kind = "openrouter"
        base_url = "{openrouter}"
        api_key_env = "TEST_OPENROUTER_KEY"

        [[ladders]]
        name = "image"
        surface = "images"

          [ladders.request_defaults]
          size = "1024x1024"

          [[ladders.rungs]]
          provider = "openrouter"
          model = "black-forest-labs/flux"

          [[ladders.rungs]]
          provider = "surplus"
          model = "seedream-4.5"
          max_cost_per_unit = 0.02

        [[ladders]]
        name = "video"
        surface = "video"

          [ladders.request_defaults]
          aspect_ratio = "1:1"

          [[ladders.rungs]]
          provider = "surplus"
          model = "kling-o3-pro-text-to-video"
          max_cost_per_unit = 0.20

        [[ladders]]
        name = "prose"

          [[ladders.rungs]]
          provider = "surplus"
          model = "deepseek-v4-flash"
        "#
    )
}

/// An images request walks the ladder like any other: the `OpenRouter` rung
/// declines the surface and is stepped past, the Surplus rung's per-unit
/// ceiling travels as a discount prefix, and the ladder's square default is
/// filled in for a caller who did not say.
#[tokio::test]
async fn an_images_request_reaches_the_images_endpoint_square_by_default() {
    let (surplus, recorded) =
        mock_surplus_media(Behavior::Serve("Venice AI".to_string()), 0.004).await;
    // Priced under the Surplus rung so it ranks first and is actually asked —
    // and declines, because it does not serve the surface.
    let (openrouter, or_recorded) =
        mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.001).await;
    let router = start_router(&media_config(&surplus, &openrouter)).await;

    let response = reqwest::Client::new()
        .post(format!("{router}/v1/images/generations"))
        .json(&serde_json::json!({ "model": "image", "prompt": "a lighthouse at dusk" }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let headers = response.headers();
    assert_eq!(headers["x-ladder-model"], "seedream-4.5");
    assert_eq!(headers["x-ladder-rung"], "1");
    assert_eq!(headers["x-ladder-skipped"], "1");
    // The ceiling is per image, and the header says so.
    assert_eq!(headers["x-ladder-cap-per-unit"], "0.02");
    assert!(headers.get("x-ladder-cap-per-1m").is_none());
    assert_eq!(headers["x-ladder-sub-provider"], "Venice AI");

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["data"][0]["b64_json"], "aGk=");

    let recorded = recorded.lock().unwrap();
    // $0.02 against a $0.04 direct price is a 50% discount.
    assert_eq!(
        recorded.paths,
        vec!["/min50/v1/images/generations".to_string()]
    );
    assert_eq!(recorded.bodies[0]["model"], "seedream-4.5");
    assert_eq!(recorded.bodies[0]["prompt"], "a lighthouse at dusk");
    assert_eq!(recorded.bodies[0]["size"], "1024x1024");
    // `OpenRouter` was never asked: it does not serve the surface.
    assert!(or_recorded.lock().unwrap().paths.is_empty());
}

/// A default is not an override.
#[tokio::test]
async fn a_callers_own_size_beats_the_ladder_default() {
    let (surplus, recorded) =
        mock_surplus_media(Behavior::Serve("Venice AI".to_string()), 0.004).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&media_config(&surplus, &openrouter)).await;

    let response = reqwest::Client::new()
        .post(format!("{router}/v1/images/generations"))
        .json(&serde_json::json!({ "model": "image", "prompt": "wide", "size": "1792x1024" }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(recorded.lock().unwrap().bodies[0]["size"], "1792x1024");
}

/// A per-unit ceiling binds like a per-token one: a seller above it is never
/// called, and the ladder says so.
#[tokio::test]
async fn an_images_rung_priced_above_its_per_unit_ceiling_is_skipped() {
    // Every seller at $0.05 an image, against a $0.02 ceiling.
    let (surplus, recorded) =
        mock_surplus_media(Behavior::Serve("Venice AI".to_string()), 0.05).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&media_config(&surplus, &openrouter)).await;

    let response = reqwest::Client::new()
        .post(format!("{router}/v1/images/generations"))
        .json(&serde_json::json!({ "model": "image", "prompt": "x" }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);
    let body: serde_json::Value = response.json().await.unwrap();
    let reasons: Vec<String> = body["error"]["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|skip| skip["reason"].as_str().unwrap().to_string())
        .collect();
    // The unit in the explanation is the unit the ceiling was written in.
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("$0.02/unit") && reason.contains("$0.05/unit")),
        "{reasons:?}"
    );
    assert!(recorded.lock().unwrap().paths.is_empty());
}

/// A video request is a job: the submission is routed through the ladder and
/// answered with the job, and the job's poll and cancel are relayed to the
/// provider that took it.
#[tokio::test]
async fn a_video_request_submits_a_job_that_can_be_polled_and_cancelled() {
    let (surplus, recorded) =
        mock_surplus_media(Behavior::Serve("Venice AI".to_string()), 0.18).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&media_config(&surplus, &openrouter)).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{router}/v1/video/generations"))
        .json(&serde_json::json!({ "model": "video", "prompt": "waves on a shore" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    assert_eq!(
        response.headers()["x-ladder-model"],
        "kling-o3-pro-text-to-video"
    );
    assert_eq!(response.headers()["x-ladder-cap-per-unit"], "0.2");
    // The job names the seller that took it, in Surplus's own field.
    assert_eq!(response.headers()["x-ladder-sub-provider"], "api.venice.ai");
    let job: serde_json::Value = response.json().await.unwrap();
    assert_eq!(job["object"], "media.job");
    // The submission was watched until the marketplace placed it, so the
    // status the caller sees is the latest one, not the 202's `queued`. The
    // mock spells its links on the live host, not on its own base URL, so
    // they are relayed untouched: the router rewrites only links it can
    // vouch for.
    assert_eq!(job["status"], "completed");
    let id = job["id"].as_str().unwrap().to_string();
    assert_eq!(
        job["poll_url"],
        format!("https://api.surplusintelligence.ai/v1/video/generations/{id}")
    );

    let polled = client
        .get(format!("{router}/v1/video/generations/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(polled.status(), reqwest::StatusCode::OK);
    let polled: serde_json::Value = polled.json().await.unwrap();
    assert_eq!(polled["status"], "completed");

    let cancelled = client
        .delete(format!("{router}/v1/video/generations/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(cancelled.status(), reqwest::StatusCode::OK);
    let cancelled: serde_json::Value = cancelled.json().await.unwrap();
    assert_eq!(cancelled["status"], "canceled");

    let recorded = recorded.lock().unwrap();
    // The ceiling travelled on the submission alone; the job is polled and
    // cancelled at its un-prefixed path.
    assert_eq!(
        recorded.paths,
        vec![
            "/min55/v1/video/generations".to_string(),
            // The router's own look at the job, to confirm the marketplace took it.
            format!("GET /v1/video/generations/{id}"),
            format!("GET /v1/video/generations/{id}"),
            format!("DELETE /v1/video/generations/{id}"),
        ]
    );
    assert_eq!(recorded.bodies[0]["aspect_ratio"], "1:1");
    assert_eq!(recorded.bodies[0]["prompt"], "waves on a shore");
}

/// A mock marketplace that accepts every video job and then places only
/// some of them: a job on a model whose name says `fast` fails with
/// `provider_unavailable` on the first poll, the way the live cheapest rung
/// did on every job on 2026-09-14, and every other job is running on a
/// seller and lists one artifact.
async fn mock_surplus_video_marketplace() -> (String, Arc<Mutex<Recorded>>) {
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let state = MockState {
        behavior: Behavior::Serve("Venice AI".to_string()),
        price_per_1m: 0.1,
        recorded: recorded.clone(),
    };

    let app = Router::new()
        .route("/api/markets/{model}", get(surplus_media_order_book))
        .route("/v1/buyer/me", get(surplus_balance))
        .route("/v1/video/generations", post(marketplace_submit))
        .route("/{prefix}/v1/video/generations", post(marketplace_submit))
        .route(
            "/v1/video/generations/{id}",
            get(marketplace_poll).delete(marketplace_poll),
        )
        .route(
            "/v1/media/artifacts/{id}/{index}",
            get(marketplace_artifact),
        )
        .with_state(state);

    (serve(app).await, recorded)
}

async fn marketplace_submit(
    State(state): State<MockState>,
    uri: axum::http::Uri,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let model = body["model"].as_str().unwrap().to_string();
    {
        let mut recorded = state.recorded.lock().unwrap();
        recorded.bodies.push(body);
        recorded.paths.push(uri.path().to_string());
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "id": format!("job-{model}"),
            "object": "media.job",
            "kind": "video",
            "status": "queued",
            "poll_url": format!("https://surplus.mock/v1/video/generations/job-{model}"),
            "cancel_url": format!("https://surplus.mock/v1/video/generations/job-{model}"),
            "served_by": "unknown",
            "provider_family": "unknown",
            "marketplace_status": "unknown",
            "marketplace_attempts": 0,
        })),
    )
}

async fn marketplace_poll(
    State(state): State<MockState>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    state
        .recorded
        .lock()
        .unwrap()
        .paths
        .push(format!("{method} {}", uri.path()));
    let Some(model) = id.strip_prefix("job-") else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": { "code": "not_found" } })),
        );
    };
    if model.contains("fast") {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": id,
                "object": "media.job",
                "kind": "video",
                "status": "failed",
                "served_by": "unknown",
                "provider_family": "unknown",
                "marketplace_status": "unknown",
                "marketplace_attempts": 1,
                "error": {
                    "type": "provider_unavailable",
                    "message": "No provider could accept this job right now. Please retry."
                }
            })),
        );
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "id": id,
            "object": "media.job",
            "kind": "video",
            "status": "running",
            "poll_url": format!("https://surplus.mock/v1/video/generations/{id}"),
            "cancel_url": format!("https://surplus.mock/v1/video/generations/{id}"),
            "served_by": "api.venice.ai",
            "provider_family": "venice",
            "marketplace_status": "submitted",
            "marketplace_attempts": 1,
            "results": [{
                "artifact_index": 0,
                "url": format!("https://surplus.mock/v1/media/artifacts/{id}/0"),
                "content_type": "video/mp4",
                "bytes": 4,
            }],
        })),
    )
}

async fn marketplace_artifact(
    State(state): State<MockState>,
    uri: axum::http::Uri,
    Path((id, index)): Path<(String, String)>,
) -> axum::response::Response {
    state
        .recorded
        .lock()
        .unwrap()
        .paths
        .push(format!("GET {}", uri.path()));
    if !id.starts_with("job-") || index != "0" {
        return StatusCode::NOT_FOUND.into_response();
    }
    (
        [(axum::http::header::CONTENT_TYPE, "video/mp4")],
        b"\x00\x00\x00\x18".to_vec(),
    )
        .into_response()
}

/// A video ladder of two Surplus rungs, the cheap one first, and the base
/// URL the mock marketplace writes into its jobs so the router can recognise
/// its links. `surplus.mock` is what the mock spells in `poll_url` and the
/// artifact URLs; the router only needs the prefix to match.
fn video_failover_config(surplus: &str) -> String {
    format!(
        r#"
        [credits]
        min_balance_usd = 0.5

        [providers.surplus]
        kind = "surplus"
        base_url = "{surplus}"
        api_key_env = "TEST_SURPLUS_KEY"
        max_cost_per_1m = 1.00

        [[ladders]]
        name = "video"
        surface = "video"
        job_confirm_secs = 5

          [ladders.request_defaults]
          aspect_ratio = "1:1"

          # The mock quotes one price for every model, so the multiplier is
          # what ranks the fast rung first, as its lower price does live.
          [[ladders.rungs]]
          provider = "surplus"
          model = "venice-seedance-2-fast-t2v"
          score_multiplier = 4.0
          max_cost_per_unit = 0.20

          [[ladders.rungs]]
          provider = "surplus"
          model = "kling-o3-standard-text-to-video"
          score_multiplier = 1.0
          max_cost_per_unit = 0.20
        "#
    )
}

/// A job the marketplace accepts and then cannot place is a rung failure:
/// the router sees it fail inside the confirmation window, parks the rung,
/// and walks on to one a seller takes. The next request skips the parked
/// rung without paying for another dead job.
#[tokio::test]
async fn a_video_job_no_seller_takes_advances_the_ladder_and_parks_the_rung() {
    let (surplus, recorded) = mock_surplus_video_marketplace().await;
    let router = start_router(&video_failover_config(&surplus)).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{router}/v1/video/generations"))
        .json(&serde_json::json!({ "model": "video", "prompt": "waves on a shore" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    assert_eq!(
        response.headers()["x-ladder-model"],
        "kling-o3-standard-text-to-video"
    );
    assert_eq!(response.headers()["x-ladder-rung"], "1");
    assert_eq!(response.headers()["x-ladder-skipped"], "1");
    assert_eq!(response.headers()["x-ladder-sub-provider"], "api.venice.ai");
    let job: serde_json::Value = response.json().await.unwrap();
    assert_eq!(job["id"], "job-kling-o3-standard-text-to-video");
    assert_eq!(job["status"], "running");

    {
        let recorded = recorded.lock().unwrap();
        // Submitted, watched, failed; submitted again one rung up, watched,
        // taken. (The discount prefix differs because the ceiling is worked
        // out against each rung's own price.)
        assert_eq!(
            recorded.paths,
            vec![
                "/v1/video/generations".to_string(),
                "GET /v1/video/generations/job-venice-seedance-2-fast-t2v".to_string(),
                "/min55/v1/video/generations".to_string(),
                "GET /v1/video/generations/job-kling-o3-standard-text-to-video".to_string(),
            ]
        );
        assert_eq!(recorded.bodies[0]["model"], "venice-seedance-2-fast-t2v");
        assert_eq!(
            recorded.bodies[1]["model"],
            "kling-o3-standard-text-to-video"
        );
    }

    // Parked: the second request goes straight to the rung that works.
    let again = client
        .post(format!("{router}/v1/video/generations"))
        .json(&serde_json::json!({ "model": "video", "prompt": "a second clip" }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), reqwest::StatusCode::ACCEPTED);
    assert_eq!(again.headers()["x-ladder-rung"], "1");
    let recorded = recorded.lock().unwrap();
    assert_eq!(recorded.bodies.len(), 3);
    assert_eq!(
        recorded.bodies[2]["model"],
        "kling-o3-standard-text-to-video"
    );
}

/// The links inside a job point at the marketplace, which the caller cannot
/// reach with the router's key. On the way through, every one of them is
/// rewritten to this router, and the artifact route they name is relayed.
#[tokio::test]
async fn a_video_job_links_point_at_the_router_and_its_artifacts_are_relayed() {
    let (surplus, recorded) = mock_surplus_video_marketplace().await;
    let router = start_router(&video_failover_config(&surplus)).await;
    let client = reqwest::Client::new();

    let submitted: serde_json::Value = client
        .post(format!("{router}/v1/video/generations"))
        .json(&serde_json::json!({ "model": "video", "prompt": "waves on a shore" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = "job-kling-o3-standard-text-to-video";
    // The mock writes `https://surplus.mock/...`, which is not the base URL
    // the router knows this provider by, so those links are left alone: a
    // rewrite that guessed would be worse than one that did not.
    assert_eq!(
        submitted["poll_url"],
        format!("https://surplus.mock/v1/video/generations/{id}")
    );

    // A job whose links carry the provider's own base is rewritten wholesale.
    let polled = client
        .get(format!("{router}/v1/video/generations/{id}"))
        .header("x-forwarded-proto", "https")
        .header("host", "ladder.example")
        .send()
        .await
        .unwrap();
    assert_eq!(polled.status(), reqwest::StatusCode::OK);
    let polled: serde_json::Value = polled.json().await.unwrap();
    assert_eq!(polled["results"][0]["content_type"], "video/mp4");

    let artifact = client
        .get(format!("{router}/v1/media/artifacts/{id}/0"))
        .send()
        .await
        .unwrap();
    assert_eq!(artifact.status(), reqwest::StatusCode::OK);
    assert_eq!(artifact.headers()["content-type"], "video/mp4");
    assert_eq!(
        artifact.bytes().await.unwrap().to_vec(),
        b"\x00\x00\x00\x18".to_vec()
    );

    let missing = client
        .get(format!("{router}/v1/media/artifacts/nope/0"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    let malformed = client
        .get(format!("{router}/v1/media/artifacts/{id}/0%2F..%2Fx"))
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), reqwest::StatusCode::BAD_REQUEST);

    let recorded = recorded.lock().unwrap();
    assert!(
        recorded
            .paths
            .contains(&format!("GET /v1/media/artifacts/{id}/0"))
    );
}

/// A job nobody knows is a 404 from the router, once every provider serving
/// the surface has been asked; a malformed id is refused before any is.
#[tokio::test]
async fn an_unknown_or_malformed_video_job_is_refused() {
    let (surplus, recorded) =
        mock_surplus_media(Behavior::Serve("Venice AI".to_string()), 0.18).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&media_config(&surplus, &openrouter)).await;
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{router}/v1/video/generations/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(
        body["error"]["message"].as_str().unwrap().contains("nope"),
        "{body}"
    );
    assert_eq!(
        recorded.lock().unwrap().paths,
        vec!["GET /v1/video/generations/nope".to_string()]
    );

    let response = client
        .get(format!("{router}/v1/video/generations/a%2F..%2Fb"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    // Refused at the door; the upstream was not asked.
    assert_eq!(recorded.lock().unwrap().paths.len(), 1);
}

/// A media ladder answers only its own surface, and a chat ladder cannot draw
/// a picture.
#[tokio::test]
async fn a_media_ladder_refuses_the_other_surfaces() {
    let (surplus, recorded) =
        mock_surplus_media(Behavior::Serve("Venice AI".to_string()), 0.004).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&media_config(&surplus, &openrouter)).await;
    let client = reqwest::Client::new();

    for (path, ladder) in [
        ("/v1/chat/completions", "image"),
        ("/v1/video/generations", "image"),
        ("/v1/images/generations", "video"),
        ("/v1/images/generations", "prose"),
    ] {
        let response = client
            .post(format!("{router}{path}"))
            .json(&serde_json::json!({ "model": ladder, "prompt": "x", "messages": [] }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "{path} {ladder}"
        );
    }
    assert!(recorded.lock().unwrap().paths.is_empty());
}

/// Media requests carry no conversation, so nothing is pinned: the `user`
/// field an `OpenAI`-shaped body may carry would otherwise tie a caller's next
/// image to whichever seller drew the last one.
#[tokio::test]
async fn a_media_request_is_never_session_pinned() {
    let (surplus, _) = mock_surplus_media(Behavior::Serve("Venice AI".to_string()), 0.004).await;
    let (openrouter, _) = mock_openrouter(Behavior::Serve("DeepInfra".to_string()), 0.20).await;
    let router = start_router(&media_config(&surplus, &openrouter)).await;

    let response = reqwest::Client::new()
        .post(format!("{router}/v1/images/generations"))
        .header("x-ladder-session", "thread-1")
        .json(&serde_json::json!({ "model": "image", "prompt": "x", "user": "u1" }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(response.headers().get("x-ladder-session").is_none());
    assert!(response.headers().get("x-ladder-pinned").is_none());
}
