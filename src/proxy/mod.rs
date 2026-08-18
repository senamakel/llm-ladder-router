//! The `OpenAI`-compatible HTTP surface and the failover loop.
//!
//! A request names a ladder in its `model` field. The router walks that ladder,
//! dispatches to the first rung that can serve, and falls through on any
//! failure the upstream owns. A failure the caller owns is returned unchanged:
//! replaying a malformed request at every rung would charge for it repeatedly
//! and still fail.

mod refresh;
mod types;

pub use refresh::{refresh_credits_once, refresh_prices_once};
pub use types::{
    HEADER_CAP, HEADER_EFFORT, HEADER_LADDER, HEADER_MODEL, HEADER_PINNED, HEADER_PROVIDER,
    HEADER_RUNG, HEADER_SESSION, HEADER_SKIPPED, HEADER_SUB_PROVIDER, State,
};

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Json, State as AxumState};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tokio::sync::RwLock;

use crate::config::Config;
use crate::credits::CreditState;
use crate::error::{Error, Result};
use crate::ladder::{self, Chosen, Skipped};
use crate::pricing::PriceTable;
use crate::provider::{Client, Disposition, Wire};
use crate::session::{Pin, SessionPins};

/// Builds the router's HTTP application and shared state.
///
/// # Errors
///
/// Returns an error only if a provider's configuration cannot be turned into a
/// client.
pub fn build(config: Config) -> Result<(axum::Router, State)> {
    build_with_credentials(config, &BTreeMap::new())
}

/// Builds the application using credentials the caller already holds.
///
/// Any provider absent from `credentials` falls back to reading its configured
/// environment variable, so this is an override rather than a replacement.
/// Useful when secrets come from a secret manager instead of the environment.
///
/// # Errors
///
/// As [`build`].
pub fn build_with_credentials(
    config: Config,
    credentials: &BTreeMap<String, String>,
) -> Result<(axum::Router, State)> {
    let http = reqwest::Client::builder()
        .timeout(config.server.request_timeout)
        .build()
        .map_err(|source| Error::Upstream {
            provider: "router".to_string(),
            source,
        })?;

    let clients: BTreeMap<String, Client> = config
        .providers
        .iter()
        .map(|(name, provider)| {
            let client = match credentials.get(name) {
                Some(api_key) => Client::with_credential(
                    name.clone(),
                    provider.clone(),
                    http.clone(),
                    Some(api_key.clone()),
                ),
                None => Client::new(name.clone(), provider.clone(), http.clone()),
            };
            (name.clone(), client)
        })
        .collect();

    let config_sessions = config.sessions.clone();
    let state = State {
        config: Arc::new(config),
        clients: Arc::new(clients),
        prices: Arc::new(RwLock::new(PriceTable::new())),
        credits: Arc::new(RwLock::new(CreditState::new())),
        sessions: Arc::new(RwLock::new(SessionPins::new(
            config_sessions.ttl,
            config_sessions.max_entries,
        ))),
    };

    let app = axum::Router::new()
        // The OpenAI surface, and the Anthropic Messages surface. Both are
        // relayed to the marketplaces' own native endpoints for that format
        // rather than translated, so no field is lost in either direction.
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/v1/models", get(list_models))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state.clone());

    Ok((app, state))
}

/// Serves until the process is asked to stop.
///
/// # Errors
///
/// Returns [`Error::Bind`] if the configured address cannot be bound, and
/// [`Error::Serve`] if the server stops with an error.
pub async fn serve(config: Config) -> Result<()> {
    serve_with_credentials(config, &BTreeMap::new()).await
}

/// Serves using credentials the caller already holds.
///
/// The counterpart to [`build_with_credentials`], for deployments whose secrets
/// come from somewhere other than the environment.
///
/// # Errors
///
/// As [`serve`].
pub async fn serve_with_credentials(
    config: Config,
    credentials: &BTreeMap<String, String>,
) -> Result<()> {
    let bind = config.server.bind.clone();
    let (app, state) = build_with_credentials(config, credentials)?;

    // Load prices and balances before accepting traffic. Serving first would
    // open a cold-start window in which every capped rung is skipped for
    // having no price data and every request fails with an exhausted ladder.
    tracing::info!("loading prices and balances before accepting traffic");
    refresh_credits_once(&state).await;
    refresh_prices_once(&state).await;
    // Read the count into a local first: an `.await` inside a `tracing` macro
    // argument holds a non-`Send` temporary across it, which would make this
    // whole future non-`Send` and so impossible for a caller to spawn.
    let models = state.prices.read().await.len();
    tracing::info!(models, "initial refresh complete");

    refresh::spawn(state.clone());

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|source| Error::Bind {
            address: bind.clone(),
            source,
        })?;

    tracing::info!(address = %bind, ladders = state.config.ladders.len(), "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(Error::Serve)
}

/// Advertises each ladder as if it were a model, so existing clients can
/// discover them with no special support.
async fn list_models(AxumState(state): AxumState<State>) -> Json<serde_json::Value> {
    let data: Vec<serde_json::Value> = state
        .config
        .ladders
        .iter()
        .map(|ladder| {
            serde_json::json!({
                "id": ladder.name,
                "object": "model",
                "owned_by": "llm-ladder-router",
                "rungs": ladder.rungs.len(),
            })
        })
        .collect();
    Json(serde_json::json!({ "object": "list", "data": data }))
}

/// The `OpenAI`-compatible entry point.
async fn chat_completions(
    AxumState(state): AxumState<State>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    route(state, &headers, body, Wire::OpenAi).await
}

/// The Anthropic Messages-compatible entry point.
async fn messages(
    AxumState(state): AxumState<State>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    route(state, &headers, body, Wire::Anthropic).await
}

/// Checks the caller's key against the configured one.
///
/// Both header spellings are accepted on both surfaces, so a client configured
/// for either vendor works without special-casing.
fn authorized(state: &State, headers: &HeaderMap) -> bool {
    let Some(expected) = state.config.server.resolved_api_key() else {
        // No key configured means the router is deliberately open.
        return true;
    };

    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim);

    // A plain equality check is fine here: the comparison is against a
    // configured shared secret over a connection the operator controls, and
    // both candidate values are already in memory.
    bearer == Some(expected.as_str()) || x_api_key == Some(expected.as_str())
}

/// Walks a ladder for one request on one wire format.
async fn route(state: State, headers: &HeaderMap, body: serde_json::Value, wire: Wire) -> Response {
    if !authorized(&state, headers) {
        return problem(
            StatusCode::UNAUTHORIZED,
            "missing or invalid api key; send it as Authorization: Bearer <key> or x-api-key",
            &[],
        );
    }

    let Some(name) = body.get("model").and_then(serde_json::Value::as_str) else {
        return problem(
            StatusCode::BAD_REQUEST,
            "request must name a ladder in its model field",
            &[],
        );
    };
    let name = name.to_string();

    let Some(ladder_config) = state.config.ladder(&name) else {
        let known: Vec<&str> = state
            .config
            .ladders
            .iter()
            .map(|ladder| ladder.name.as_str())
            .collect();
        return problem(
            StatusCode::BAD_REQUEST,
            &format!(
                "unknown ladder {name}; known ladders are {}",
                known.join(", ")
            ),
            &[],
        );
    };

    let session = session_of(&state, headers, &body);
    walk(&state, ladder_config, &name, session, body, wire).await
}

/// Walks a ladder, dispatching until a rung serves or the rungs run out.
async fn walk(
    state: &State,
    ladder_config: &crate::config::Ladder,
    name: &str,
    session: Option<String>,
    body: serde_json::Value,
    wire: Wire,
) -> Response {
    let mut tried: Vec<usize> = Vec::new();
    let mut passed: Vec<Skipped> = Vec::new();

    // Each iteration consumes one rung: either it serves, or it is recorded in
    // `tried` and excluded from the next selection. The loop is therefore
    // bounded by the rung count and cannot spin.
    for _ in 0..ladder_config.rungs.len() {
        let selection = {
            let prices = state.prices.read().await;
            let credits = state.credits.read().await;
            let sessions = state.sessions.read().await;
            let pin = session.as_deref().and_then(|session| sessions.get(session));
            ladder::select_pinned(&state.config, ladder_config, &prices, &credits, &tried, pin)
        };

        if let Some(reason) = &selection.pin_rejected {
            tracing::info!(
                ladder = %name,
                session = session.as_deref().unwrap_or("-"),
                reason = %reason,
                "session pin dropped"
            );
        }

        passed.extend(selection.skipped);
        let pinned = selection.pinned;
        let Some(chosen) = selection.chosen else {
            break;
        };

        let Some(client) = state.clients.get(&chosen.provider) else {
            break;
        };

        match dispatch(client, &chosen, wire, &body).await {
            Attempt::Served(response) => {
                tracing::info!(
                    ladder = %name,
                    rung = chosen.rung,
                    provider = %chosen.provider,
                    model = %chosen.model,
                    cap_per_1m = ?chosen.cap_per_1m,
                    cheapest_per_1m = ?chosen.cheapest_per_1m,
                    min_discount_pct = ?chosen.min_discount_pct,
                    skipped = passed.len(),
                    session = session.as_deref().unwrap_or("-"),
                    pinned,
                    "rung served"
                );

                let served_by = sub_provider_of(&response);
                remember(state, session.as_deref(), name, &chosen, served_by).await;

                return with_routing_headers(
                    response,
                    name,
                    &chosen,
                    passed.len(),
                    session.as_deref(),
                    pinned,
                );
            }
            Attempt::Advance(detail) => {
                tracing::warn!(
                    ladder = %name,
                    rung = chosen.rung,
                    provider = %chosen.provider,
                    model = %chosen.model,
                    detail = %detail,
                    "rung failed, advancing"
                );
                passed.push(Skipped {
                    rung: chosen.rung,
                    provider: chosen.provider.clone(),
                    model: chosen.model.clone(),
                    reason: ladder::SkipReason::UpstreamFailed { detail },
                });
                tried.push(chosen.rung);
            }
            Attempt::CallerError(response) => {
                return with_routing_headers(
                    response,
                    name,
                    &chosen,
                    passed.len(),
                    session.as_deref(),
                    pinned,
                );
            }
        }
    }

    tracing::error!(ladder = %name, skipped = passed.len(), "ladder exhausted");
    problem(
        StatusCode::BAD_GATEWAY,
        &Error::LadderExhausted {
            ladder: name.to_string(),
        }
        .to_string(),
        &passed,
    )
}

/// What one rung's dispatch produced.
enum Attempt {
    Served(Response),
    Advance(String),
    CallerError(Response),
}

async fn dispatch(
    client: &Client,
    chosen: &Chosen,
    wire: Wire,
    body: &serde_json::Value,
) -> Attempt {
    let dispatched = match client.infer(chosen, wire, body).await {
        Ok(dispatched) => dispatched,
        // A transport failure is the upstream's, not the caller's.
        Err(error) => return Attempt::Advance(error.to_string()),
    };

    let disposition = client.classify(&dispatched);
    let mut response = Response::builder().status(dispatched.status);
    if let Some(content_type) = &dispatched.content_type
        && let Ok(value) = HeaderValue::from_str(content_type)
    {
        response = response.header(axum::http::header::CONTENT_TYPE, value);
    }
    if let Some(served_by) = &dispatched.served_by
        && let Ok(value) = HeaderValue::from_str(served_by)
    {
        response = response.header(types::HEADER_SUB_PROVIDER, value);
    }

    let built = response
        .body(axum::body::Body::from(dispatched.body.clone()))
        .map_or_else(
            |_| {
                problem(
                    StatusCode::BAD_GATEWAY,
                    "upstream response could not be relayed",
                    &[],
                )
            },
            IntoResponse::into_response,
        );

    match disposition {
        Disposition::Served => Attempt::Served(built),
        Disposition::CallerError => Attempt::CallerError(built),
        Disposition::Advance => Attempt::Advance(format!(
            "{} {}",
            dispatched.status,
            String::from_utf8_lossy(&dispatched.body)
                .chars()
                .take(200)
                .collect::<String>()
        )),
    }
}

/// Pins a conversation to the rung and sub-provider that just served it.
///
/// Recording the sub-provider is the point: the marketplace picked it, and it
/// is the one holding the warm prompt cache for this thread.
/// `sub_provider` is read from the response before this is called: a `Response`
/// body is not `Sync`, so holding a reference to one across the lock would make
/// the whole request future non-`Send` and unspawnable.
async fn remember(
    state: &State,
    session: Option<&str>,
    ladder: &str,
    chosen: &Chosen,
    sub_provider: Option<String>,
) {
    let Some(session) = session else {
        return;
    };
    state.sessions.write().await.pin(
        session,
        Pin {
            ladder: ladder.to_string(),
            rung: chosen.rung,
            provider: chosen.provider.clone(),
            model: chosen.model.clone(),
            sub_provider,
            cap_per_1m: chosen.cap_per_1m,
            pinned_at: std::time::Instant::now(),
        },
    );
}

/// The conversation this request belongs to, if any.
///
/// The configured header wins; otherwise the identifiers the two APIs already
/// carry are used, so an unmodified client still gets sticky routing. Anthropic
/// puts it in `metadata.user_id` and `OpenAI` in `user`.
fn session_of(state: &State, headers: &HeaderMap, body: &serde_json::Value) -> Option<String> {
    if !state.config.sessions.enabled {
        return None;
    }

    let from_header = headers
        .get(state.config.sessions.header.as_str())
        .and_then(|value| value.to_str().ok());

    from_header
        .map(str::to_string)
        .or_else(|| {
            body.get("metadata")
                .and_then(|metadata| metadata.get("user_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            body.get("user")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .map(|session| session.trim().to_string())
        .filter(|session| !session.is_empty())
}

/// The sub-provider a relayed response reports having served it.
fn sub_provider_of(response: &Response) -> Option<String> {
    response
        .headers()
        .get(types::HEADER_SUB_PROVIDER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Stamps a response with the decision that produced it.
fn with_routing_headers(
    mut response: Response,
    ladder: &str,
    chosen: &Chosen,
    skipped: usize,
    session: Option<&str>,
    pinned: bool,
) -> Response {
    let headers = response.headers_mut();
    set(headers, types::HEADER_LADDER, ladder);
    set(headers, types::HEADER_RUNG, &chosen.rung.to_string());
    set(headers, types::HEADER_PROVIDER, &chosen.provider);
    set(headers, types::HEADER_MODEL, &chosen.model);
    set(headers, types::HEADER_SKIPPED, &skipped.to_string());
    if let Some(cap) = chosen.cap_per_1m {
        set(headers, types::HEADER_CAP, &cap.to_string());
    }
    if let Some(effort) = &chosen.reasoning_effort {
        set(headers, types::HEADER_EFFORT, effort);
    }
    if let Some(session) = session {
        set(headers, types::HEADER_SESSION, session);
        set(
            headers,
            types::HEADER_PINNED,
            if pinned { "true" } else { "false" },
        );
    }
    response
}

fn set(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

/// An error body that explains every rung that was passed over.
///
/// A bare "no rung could serve" is not actionable; the per-rung reasons are the
/// whole point of recording them.
fn problem(status: StatusCode, message: &str, skipped: &[Skipped]) -> Response {
    let rungs: Vec<serde_json::Value> = skipped
        .iter()
        .map(|skip| {
            serde_json::json!({
                "rung": skip.rung,
                "provider": skip.provider,
                "model": skip.model,
                "reason": skip.reason.to_string(),
            })
        })
        .collect();

    (
        status,
        Json(serde_json::json!({
            "error": {
                "message": message,
                "type": "ladder_router_error",
                "skipped": rungs,
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod test;
