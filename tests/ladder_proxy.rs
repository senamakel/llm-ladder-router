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
