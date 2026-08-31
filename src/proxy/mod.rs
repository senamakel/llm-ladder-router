//! The `OpenAI`- and Anthropic-compatible HTTP surfaces, and the failover loop.
//!
//! A request names a ladder in its `model` field. The router ranks that
//! ladder's rungs, dispatches to the best one that can serve, and on any
//! failure the upstream owns re-ranks what is left and takes the next. A
//! failure the caller owns is returned unchanged: replaying a malformed request
//! at every rung would charge for it repeatedly and still fail.
//!
//! One upstream failure is remembered past the request that met it. A 429 parks
//! its rung for a cooldown, because the upstream refusing on purpose is a fact
//! about the next few seconds rather than about this one request.

mod refresh;
mod types;

pub use refresh::{refresh_credits_once, refresh_prices_once};
pub use types::{
    HEADER_CAP, HEADER_EFFORT, HEADER_LADDER, HEADER_MODEL, HEADER_PINNED, HEADER_PROVIDER,
    HEADER_RUNG, HEADER_SCORE, HEADER_SESSION, HEADER_SKIPPED, HEADER_SUB_PROVIDER, State,
};

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Json, State as AxumState};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tokio::sync::RwLock;

use crate::config::{Config, Surface};
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
        cooldowns: Arc::new(RwLock::new(crate::cooldown::Cooldowns::new())),
    };

    let app = axum::Router::new()
        // The three OpenAI surfaces, and the Anthropic Messages surface. All
        // four are relayed to the marketplaces' own native endpoints for that
        // format rather than translated, so no field is lost in any direction.
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .route("/v1/embeddings", post(embeddings))
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
                // So a client discovering ladders can tell which endpoint each
                // one answers on without reading the router's configuration.
                "surface": surface_name(ladder.surface),
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

/// The `OpenAI` Responses-compatible entry point.
///
/// Its own route rather than a variant of [`chat_completions`], because the two
/// are different APIs that happen to share a vendor: the request names its
/// prompt in `input` rather than `messages`, the response is a `response`
/// object rather than a `chat.completion`, and reasoning depth is spelled
/// differently in both.
async fn responses(
    AxumState(state): AxumState<State>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    route(state, &headers, body, Wire::Responses).await
}

/// The `OpenAI`-compatible embeddings entry point.
///
/// The same ladder machinery, on a body the router does not otherwise look
/// into: a rung is chosen and failed over exactly as it is for a chat request,
/// and the caller's `input` is relayed untouched.
async fn embeddings(
    AxumState(state): AxumState<State>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    route(state, &headers, body, Wire::Embeddings).await
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

/// Whether a ladder declared for one surface may answer a request on a given
/// wire format.
fn serves(surface: Surface, wire: Wire) -> bool {
    match surface {
        Surface::Chat => matches!(wire, Wire::OpenAi | Wire::Anthropic | Wire::Responses),
        Surface::Embeddings => wire == Wire::Embeddings,
    }
}

/// What to call a surface in a message to a caller, and in `/v1/models`.
fn surface_name(surface: Surface) -> &'static str {
    match surface {
        Surface::Chat => "chat",
        Surface::Embeddings => "embeddings",
    }
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

    if !serves(ladder_config.surface, wire) {
        // An embedding model cannot answer a chat request and a chat model
        // cannot answer an embeddings one, so a mismatch is a 400 rather than a
        // ladder walk that fails identically at every rung and bills for the
        // attempts.
        return problem(
            StatusCode::BAD_REQUEST,
            &format!(
                "ladder {name} serves the {} surface, not {}",
                surface_name(ladder_config.surface),
                wire.api_name()
            ),
            &[],
        );
    }

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

    // Each iteration consumes one normal rung or the optional ultimate
    // fallback: either it serves, or it is recorded in `tried` and excluded
    // from the next selection. The loop is therefore bounded and cannot spin.
    for _ in 0..(ladder_config.rungs.len() + usize::from(ladder_config.fallback.is_some())) {
        let selection = choose(state, ladder_config, session.as_deref(), &tried).await;

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
                    score = ?chosen.score,
                    score_multiplier = chosen.score_multiplier,
                    min_discount_pct = ?chosen.min_discount_pct,
                    skipped = passed.len(),
                    session = session.as_deref().unwrap_or("-"),
                    pinned,
                    "rung served"
                );

                let served_by = sub_provider_of(&response);
                if chosen.rung < ladder_config.rungs.len() {
                    remember(state, session.as_deref(), name, &chosen, served_by).await;
                }

                return with_routing_headers(
                    response,
                    name,
                    &chosen,
                    passed.len(),
                    session.as_deref(),
                    pinned,
                );
            }
            Attempt::Advance { detail, kind } => {
                match kind {
                    Failure::RateLimited(retry_after) => {
                        park(state, name, &chosen, retry_after, "rate limited").await;
                    }
                    Failure::Refused => {
                        park(state, name, &chosen, None, "refused this router").await;
                    }
                    Failure::Broke => {}
                }
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

/// Reads the four pieces of live state under their locks and ranks the ladder.
///
/// Split out so the locks are held for exactly the length of the decision and
/// released before the round trip: an upstream that takes ninety seconds must
/// not be holding the price table shut against every other request.
async fn choose(
    state: &State,
    ladder_config: &crate::config::Ladder,
    session: Option<&str>,
    tried: &[usize],
) -> ladder::Selection {
    let prices = state.prices.read().await;
    let credits = state.credits.read().await;
    let sessions = state.sessions.read().await;
    let cooldowns = state.cooldowns.read().await;
    let pin = session.and_then(|session| sessions.get(session));
    ladder::select_pinned(
        &state.config,
        ladder_config,
        &prices,
        &credits,
        &cooldowns,
        tried,
        pin,
    )
}

/// Takes a rate-limited rung out of service for a while.
///
/// A 429 is the upstream saying "not now", which is true of the next request
/// too. Parking the rung is what keeps a throttled provider from costing one
/// wasted round trip per request until the limit lifts.
async fn park(
    state: &State,
    ladder: &str,
    chosen: &Chosen,
    retry_after: Option<std::time::Duration>,
    why: &str,
) {
    let cooled = state.config.rate_limits.cooldown_for(retry_after);
    state
        .cooldowns
        .write()
        .await
        .cool(&chosen.provider, &chosen.model, cooled.duration);
    tracing::warn!(
        ladder = %ladder,
        rung = chosen.rung,
        provider = %chosen.provider,
        model = %chosen.model,
        cooldown_secs = cooled.duration.as_secs(),
        upstream_asked = cooled.requested,
        why,
        "rung parked, cooling down"
    );
}

/// Why a rung did not serve, when the fault was the upstream's.
///
/// The distinction decides whether the rung is parked: a 500 or a timeout says
/// the upstream broke, which the next request has every reason to re-test,
/// while a 429 or a 403 says it is deliberately refusing and will keep saying
/// so until something changes at its end.
#[derive(Debug, Clone, Copy)]
enum Failure {
    /// Rate limited, carrying the backoff the upstream asked for if it named
    /// one.
    RateLimited(Option<std::time::Duration>),
    /// Refusing to authenticate this router — 401, 403 or 407.
    ///
    /// Parked on the same argument as a rate limit, and it is the same waste:
    /// a marketplace whose edge is refusing does so for minutes, and without a
    /// cooldown every request in that window pays a failed round trip to
    /// rediscover it. The measured case was a fifteen-minute Surplus outage.
    /// Nothing is asked of the upstream here — a refusal carries no
    /// `Retry-After` — so the configured default applies.
    Refused,
    /// Anything else the upstream owns.
    Broke,
}

/// What one rung's dispatch produced.
enum Attempt {
    Served(Response),
    /// The upstream failed on its own account, and whether it was refusing on
    /// purpose or simply broken.
    Advance {
        detail: String,
        kind: Failure,
    },
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
        Err(error) => {
            return Attempt::Advance {
                detail: error.to_string(),
                kind: Failure::Broke,
            };
        }
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
        Disposition::Advance => Attempt::Advance {
            detail: format!(
                "{} {}",
                dispatched.status,
                String::from_utf8_lossy(&dispatched.body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            ),
            kind: match dispatched.status {
                StatusCode::TOO_MANY_REQUESTS => Failure::RateLimited(dispatched.retry_after),
                StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::PROXY_AUTHENTICATION_REQUIRED => Failure::Refused,
                _ => Failure::Broke,
            },
        },
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
    if let Some(score) = chosen.score {
        set(headers, types::HEADER_SCORE, &score.to_string());
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
