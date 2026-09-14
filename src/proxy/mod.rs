//! The `OpenAI`- and Anthropic-compatible HTTP surfaces, the media surfaces,
//! and the failover loop.
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

pub mod jobs;
mod refresh;
mod types;

pub use refresh::{refresh_credits_once, refresh_prices_once};
pub use types::{
    HEADER_CAP, HEADER_CAP_PER_UNIT, HEADER_EFFORT, HEADER_LADDER, HEADER_MODEL, HEADER_PINNED,
    HEADER_PROVIDER, HEADER_RUNG, HEADER_SCORE, HEADER_SESSION, HEADER_SKIPPED,
    HEADER_SUB_PROVIDER, State,
};

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Json, Path, State as AxumState};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tokio::sync::RwLock;

use crate::config::{Config, Surface};
use crate::credits::CreditState;
use crate::error::{Error, Result};
use crate::ladder::{self, Chosen, Skipped};
use crate::pricing::PriceTable;
use crate::provider::{Client, Disposition, Wire, surplus};
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
        jobs: Arc::new(RwLock::new(jobs::RecentJobs::new(
            RECENT_JOB_TTL,
            RECENT_JOB_CAP,
        ))),
    };

    let app = axum::Router::new()
        // The three OpenAI surfaces, and the Anthropic Messages surface. All
        // four are relayed to the marketplaces' own native endpoints for that
        // format rather than translated, so no field is lost in any direction.
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .route("/v1/embeddings", post(embeddings))
        // The two media surfaces. Video is a job upstream, so its path also
        // answers `GET` and `DELETE` for polling and cancelling; both are
        // relayed to whichever provider knows the job.
        .route("/v1/images/generations", post(images))
        .route("/v1/video/generations", post(video))
        .route(
            "/v1/video/generations/{id}",
            get(video_job_poll).delete(video_job_cancel),
        )
        // A finished job names its clip at the marketplace's own artifact
        // route, which the caller's router key cannot fetch; the router
        // fetches it on their behalf, as it polls on their behalf.
        .route("/v1/media/artifacts/{id}/{index}", get(video_artifact))
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
                // The other names this ladder answers to, so a client pinned to
                // one of them can see it is served rather than concluding the
                // model is gone.
                "aliases": ladder.aliases,
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

/// The `OpenAI`-compatible image-generation entry point.
///
/// Prompt in, image out, on a body the router looks into only to fill in the
/// ladder's `request_defaults` — which is how an images ladder is square by
/// policy rather than by every caller remembering `size`.
async fn images(
    AxumState(state): AxumState<State>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    route(state, &headers, body, Wire::Images).await
}

/// The video-generation entry point.
///
/// The response is a job, not a clip: the upstream answers as soon as the job
/// is queued, and the caller polls [`video_job_poll`] until it finishes. The
/// ladder is walked once, here, when the job is submitted; the poll goes to
/// the provider that took it.
async fn video(
    AxumState(state): AxumState<State>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    route(state, &headers, body, Wire::Video).await
}

/// Polls a video job.
async fn video_job_poll(
    AxumState(state): AxumState<State>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    relay_video_job(&state, &headers, reqwest::Method::GET, &id).await
}

/// Cancels a video job.
async fn video_job_cancel(
    AxumState(state): AxumState<State>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    relay_video_job(&state, &headers, reqwest::Method::DELETE, &id).await
}

/// Fetches one artifact of a finished video job.
async fn video_artifact(
    AxumState(state): AxumState<State>,
    headers: HeaderMap,
    Path((id, index)): Path<(String, String)>,
) -> Response {
    if !is_job_id(&index) {
        return problem(StatusCode::BAD_REQUEST, "malformed artifact index", &[]);
    }
    relay_video_path(
        &state,
        &headers,
        reqwest::Method::GET,
        &id,
        &surplus::video_artifact_path(&id, &index),
    )
    .await
}

/// Relays a video job's poll or cancel to the provider that holds it.
///
/// The router keeps no table of jobs. The marketplace owns the job and names
/// its own `poll_url`, so the only question is *which* provider, and the
/// answer is found by asking each one that serves the video surface, in
/// configuration order, and returning the first answer that is not a 404. With
/// one such provider that is one round trip; a router restarted between submit
/// and poll loses nothing.
///
/// A job id is a path segment the upstream will interpret, so anything that
/// could escape the job's own path — a slash, a query, a dot-segment — is
/// refused here rather than relayed.
async fn relay_video_job(
    state: &State,
    headers: &HeaderMap,
    method: reqwest::Method,
    id: &str,
) -> Response {
    relay_video_path(state, headers, method, id, &surplus::video_job_path(id)).await
}

/// Relays one bodiless request about a video job -- its poll, its cancel,
/// or one of its artifacts -- to the provider that holds the job, and points
/// every marketplace URL in the answer back at this router.
async fn relay_video_path(
    state: &State,
    headers: &HeaderMap,
    method: reqwest::Method,
    id: &str,
    path: &str,
) -> Response {
    if !authorized(state, headers) {
        return problem(
            StatusCode::UNAUTHORIZED,
            "missing or invalid api key; send it as Authorization: Bearer <key> or x-api-key",
            &[],
        );
    }
    if !is_job_id(id) {
        return problem(StatusCode::BAD_REQUEST, "malformed video job id", &[]);
    }

    let origin = origin_of(headers);
    let mut asked = 0_usize;
    for client in state
        .clients
        .values()
        .filter(|client| client.serves(Wire::Video) && client.has_credential())
    {
        asked += 1;
        match client.relay(method.clone(), path).await {
            Ok(dispatched) if dispatched.status == StatusCode::NOT_FOUND => {}
            Ok(mut dispatched) => {
                if method == reqwest::Method::GET && dispatched.status == StatusCode::OK {
                    settle_job(state, id, &dispatched.body).await;
                }
                rewrite_job_urls(&mut dispatched, client.base_url(), origin.as_deref());
                return relayed(&dispatched, |_| {
                    problem(
                        StatusCode::BAD_GATEWAY,
                        "upstream response could not be relayed",
                        &[],
                    )
                });
            }
            Err(error) => {
                tracing::warn!(provider = client.name(), error = %error, "video job relay failed");
            }
        }
    }

    if asked == 0 {
        return problem(
            StatusCode::BAD_GATEWAY,
            "no configured provider serves the video surface",
            &[],
        );
    }
    problem(
        StatusCode::NOT_FOUND,
        &format!("no provider knows video job {id}"),
        &[],
    )
}

/// Reads a relayed poll for the job's fate, and parks the rung that
/// submitted it when the marketplace reports it failed.
///
/// A job that fails after the confirmation window -- a seller took it and
/// broke two minutes into the render -- cannot be walked past for the caller
/// who submitted it; the job is theirs and the marketplace's. What can be
/// done is for the next submission, theirs or anybody's, not to land on the
/// same rung, which is what a cooldown is for. A finished job is forgotten
/// either way.
async fn settle_job(state: &State, id: &str, body: &[u8]) {
    let Ok(job) = serde_json::from_slice::<serde_json::Value>(body) else {
        return;
    };
    match surplus::job_progress(&job) {
        surplus::JobProgress::Waiting | surplus::JobProgress::Taken
            if !surplus::job_is_finished(&job) => {}
        progress => {
            let owner = state.jobs.write().await.owner(id).cloned();
            if let Some(owner) = owner {
                state.jobs.write().await.forget(id);
                if let surplus::JobProgress::Failed(detail) = progress {
                    let cooled = state.config.rate_limits.cooldown_for(None);
                    state
                        .cooldowns
                        .write()
                        .await
                        .cool(&owner.provider, &owner.model, cooled.duration);
                    tracing::warn!(
                        ladder = %owner.ladder,
                        provider = %owner.provider,
                        model = %owner.model,
                        job = id,
                        detail = %detail,
                        cooldown_secs = cooled.duration.as_secs(),
                        "video job failed after handover, rung parked"
                    );
                }
            }
        }
    }
}

/// Where the caller reached this router, as the scheme and host a URL back
/// to it should carry: `X-Forwarded-Proto` and `Host` when a proxy set them,
/// the bare `Host` otherwise, and nothing when even that is missing.
fn origin_of(headers: &HeaderMap) -> Option<String> {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())?
        .trim();
    if host.is_empty() {
        return None;
    }
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| *value == "https" || *value == "http")
        .unwrap_or("http");
    Some(format!("{scheme}://{host}"))
}

/// Points a job's marketplace URLs -- `poll_url`, `cancel_url` and every
/// artifact `results[].url` -- at this router instead.
///
/// The marketplace writes its own host into the job, and a caller who follows
/// one of those links arrives at Surplus carrying the router's key, which
/// Surplus does not know. The paths are the same on both hosts, since the
/// router relays them, so a URL that starts with the provider's base is
/// rewritten to start with the router's origin. Anything else in the job is
/// left exactly as it came.
fn rewrite_job_urls(
    dispatched: &mut crate::provider::Dispatched,
    provider_base: &str,
    origin: Option<&str>,
) {
    let Some(origin) = origin else { return };
    let Ok(mut job) = serde_json::from_slice::<serde_json::Value>(&dispatched.body) else {
        return;
    };
    if job.get("object").and_then(serde_json::Value::as_str) != Some("media.job") {
        return;
    }
    let base = provider_base.trim_end_matches('/');
    let mut changed = false;
    let mut rewrite = |value: &mut serde_json::Value| {
        if let Some(url) = value.as_str()
            && let Some(path) = url.strip_prefix(base)
            && path.starts_with('/')
        {
            *value = serde_json::Value::String(format!("{origin}{path}"));
            changed = true;
        }
    };
    if let Some(object) = job.as_object_mut() {
        for key in ["poll_url", "cancel_url"] {
            if let Some(value) = object.get_mut(key) {
                rewrite(value);
            }
        }
        if let Some(results) = object
            .get_mut("results")
            .and_then(serde_json::Value::as_array_mut)
        {
            for result in results {
                if let Some(value) = result.get_mut("url") {
                    rewrite(value);
                }
            }
        }
    }
    if changed && let Ok(body) = serde_json::to_vec(&job) {
        dispatched.body = body;
    }
}

/// Whether a caller-supplied job id is safe to place in an upstream path.
///
/// Surplus ids are ULIDs, but the check is looser than that on purpose: any
/// run of URL-safe characters that cannot begin a new path segment or a query
/// is fine, and a stricter rule would only refuse a valid id from a marketplace
/// that spells them differently.
fn is_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
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
        Surface::Images => wire == Wire::Images,
        Surface::Video => wire == Wire::Video,
    }
}

/// What to call a surface in a message to a caller, and in `/v1/models`.
fn surface_name(surface: Surface) -> &'static str {
    match surface {
        Surface::Chat => "chat",
        Surface::Embeddings => "embeddings",
        Surface::Images => "images",
        Surface::Video => "video",
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
        // Aliases are listed beside the names, because a caller reading this
        // wants every spelling that would have worked, not only the canonical
        // one.
        let known: Vec<String> = state
            .config
            .ladders
            .iter()
            .map(|ladder| {
                if ladder.aliases.is_empty() {
                    ladder.name.clone()
                } else {
                    format!("{} (also {})", ladder.name, ladder.aliases.join(", "))
                }
            })
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

    // From here on the ladder is known by its own name rather than by the
    // spelling that arrived. A caller reaching one ladder under three names
    // would otherwise split its log lines, response headers and session pins
    // three ways, and a pin recorded under an alias would be dropped as
    // belonging to a different ladder on the next request that spelled it
    // differently.
    let name = ladder_config.name.clone();
    // A media request has no prompt cache to keep warm, and its `user` field —
    // the one OpenAI-shaped identifier it might carry — would otherwise pin the
    // caller's next image to whichever seller drew their last one.
    let session = if wire.is_media() {
        None
    } else {
        session_of(&state, headers, &body)
    };
    let mut body = body;
    ladder_config.apply_request_defaults(&mut body);
    let origin = origin_of(headers);
    walk(
        &state,
        ladder_config,
        &name,
        session,
        body,
        wire,
        origin.as_deref(),
    )
    .await
}

/// Walks a ladder, dispatching until a rung serves or the rungs run out.
async fn walk(
    state: &State,
    ladder_config: &crate::config::Ladder,
    name: &str,
    session: Option<String>,
    body: serde_json::Value,
    wire: Wire,
    origin: Option<&str>,
) -> Response {
    let confirm = if wire == Wire::Video {
        std::time::Duration::from_secs(ladder_config.job_confirm_secs)
    } else {
        std::time::Duration::ZERO
    };
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

        match dispatch(client, &chosen, wire, &body, confirm, origin).await {
            Attempt::Served(response, job_id) => {
                if let Some(id) = job_id {
                    state.jobs.write().await.insert(
                        &id,
                        jobs::JobOwner {
                            ladder: name.to_string(),
                            provider: chosen.provider.clone(),
                            model: chosen.model.clone(),
                        },
                    );
                }
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
                    ladder_config,
                    &chosen,
                    passed.len(),
                    session.as_deref(),
                    pinned,
                );
            }
            Attempt::Advance { detail, kind } => {
                park_for(state, name, &chosen, kind).await;
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
                    ladder_config,
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

/// Parks a failed rung when its failure says the next request would fail too.
async fn park_for(state: &State, ladder: &str, chosen: &Chosen, kind: Failure) {
    match kind {
        Failure::RateLimited(retry_after) => {
            park(state, ladder, chosen, retry_after, "rate limited").await;
        }
        Failure::Refused => park(state, ladder, chosen, None, "refused this router").await,
        Failure::Unavailable => park(state, ladder, chosen, None, "no seller took the job").await,
        Failure::Broke => {}
    }
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
    /// A video job the marketplace accepted and then could not place with any
    /// seller. Parked like a refusal: the marketplace has just tried every
    /// seller it has for that model and the next job would go the same way.
    Unavailable,
    /// Anything else the upstream owns.
    Broke,
}

/// What one rung's dispatch produced.
enum Attempt {
    /// The rung served; for a video submission, the id of the job it made.
    Served(Response, Option<String>),
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
    confirm: std::time::Duration,
    origin: Option<&str>,
) -> Attempt {
    let mut dispatched = match client.infer(chosen, wire, body).await {
        Ok(dispatched) => dispatched,
        // A transport failure is the upstream's, not the caller's.
        Err(error) => {
            return Attempt::Advance {
                detail: error.to_string(),
                kind: Failure::Broke,
            };
        }
    };

    let mut disposition = client.classify(&dispatched);
    if wire == Wire::Video && disposition == Disposition::Served {
        match confirm_job(client, &dispatched, confirm).await {
            Confirmed::Accepted(job) => dispatched = job,
            Confirmed::Failed(detail) => {
                return Attempt::Advance {
                    detail,
                    kind: Failure::Unavailable,
                };
            }
        }
        rewrite_job_urls(&mut dispatched, client.base_url(), origin);
        disposition = Disposition::Served;
    }
    let job_id = if wire == Wire::Video && disposition == Disposition::Served {
        job_id_of(&dispatched.body)
    } else {
        None
    };
    let built = relayed(&dispatched, |_| {
        problem(
            StatusCode::BAD_GATEWAY,
            "upstream response could not be relayed",
            &[],
        )
    });

    match disposition {
        Disposition::Served => Attempt::Served(built, job_id),
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

/// The id of the `media.job` in a body, when there is one worth relaying.
fn job_id_of(body: &[u8]) -> Option<String> {
    let job = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    if job.get("object").and_then(serde_json::Value::as_str) != Some("media.job") {
        return None;
    }
    let id = job.get("id").and_then(serde_json::Value::as_str)?;
    is_job_id(id).then(|| id.to_string())
}

/// What watching a freshly submitted video job decided.
enum Confirmed {
    /// The job as last seen: picked up by a seller, finished, or still queued
    /// when the window ran out. Handed to the caller in place of the 202 body,
    /// so the status they see is the latest one.
    Accepted(crate::provider::Dispatched),
    /// The marketplace failed the job on its own account.
    Failed(String),
}

/// How long a submitted video job is remembered, which is about as long as a
/// render can take before the marketplace itself expires it (its jobs carry a
/// thirty-minute `expires_at`).
const RECENT_JOB_TTL: std::time::Duration = std::time::Duration::from_secs(45 * 60);
/// How many recent jobs are remembered at most.
const RECENT_JOB_CAP: usize = 4_096;

/// How often a submitted job is looked at inside the confirmation window.
const JOB_CONFIRM_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Watches a just-submitted video job until the marketplace has either placed
/// it with a seller or given up on it, or the window closes.
///
/// A `202` on submit means the job was queued, not that anybody will render
/// it; the marketplace goes looking for a seller afterwards and reports
/// `provider_unavailable` when it finds none. That is a rung failure by any
/// other name, and this is where the ladder gets to treat it as one. The poll
/// carries no body and no ceiling, and the job stays the marketplace's: the
/// router still keeps no table.
///
/// The job is handed back unchanged when the window is zero, when the
/// submission carried no id, or when a poll itself fails -- none of which is
/// evidence against the rung.
async fn confirm_job(
    client: &Client,
    submitted: &crate::provider::Dispatched,
    window: std::time::Duration,
) -> Confirmed {
    if window.is_zero() {
        return Confirmed::Accepted(submitted.clone());
    }
    let Some(id) = serde_json::from_slice::<serde_json::Value>(&submitted.body)
        .ok()
        .and_then(|job| {
            job.get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
    else {
        return Confirmed::Accepted(submitted.clone());
    };
    if !is_job_id(&id) {
        return Confirmed::Accepted(submitted.clone());
    }

    let deadline = tokio::time::Instant::now() + window;
    let path = surplus::video_job_path(&id);
    let mut latest = submitted.clone();
    loop {
        let Ok(polled) = client.relay(reqwest::Method::GET, &path).await else {
            return Confirmed::Accepted(latest);
        };
        if polled.status != StatusCode::OK {
            return Confirmed::Accepted(latest);
        }
        let Ok(job) = serde_json::from_slice::<serde_json::Value>(&polled.body) else {
            return Confirmed::Accepted(latest);
        };
        match surplus::job_progress(&job) {
            surplus::JobProgress::Failed(detail) => {
                return Confirmed::Failed(format!("job {id} {detail}"));
            }
            surplus::JobProgress::Taken => {
                // The seller is the marketplace's choice and only known now.
                latest = crate::provider::Dispatched {
                    status: submitted.status,
                    served_by: polled.served_by.clone().or(latest.served_by),
                    ..polled
                };
                return Confirmed::Accepted(latest);
            }
            surplus::JobProgress::Waiting => {
                latest = crate::provider::Dispatched {
                    status: submitted.status,
                    ..polled
                };
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Confirmed::Accepted(latest);
        }
        tokio::time::sleep(JOB_CONFIRM_POLL).await;
    }
}

/// Turns an upstream answer into the response the caller sees, status, body
/// and content type unchanged, with the sub-provider that served named in a
/// header.
///
/// `or_else` supplies the response for the one failure — a body the framework
/// refuses to carry — so this stays total.
fn relayed(
    dispatched: &crate::provider::Dispatched,
    or_else: impl FnOnce(axum::http::Error) -> Response,
) -> Response {
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
    response
        .body(axum::body::Body::from(dispatched.body.clone()))
        .map_or_else(or_else, IntoResponse::into_response)
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
/// The configured header wins; otherwise native Claude Code and Codex
/// identifiers, then the identifiers the two APIs already carry, are used so
/// an unmodified client still gets sticky routing.
fn session_of(state: &State, headers: &HeaderMap, body: &serde_json::Value) -> Option<String> {
    if !state.config.sessions.enabled {
        return None;
    }

    let from_header = headers
        .get(state.config.sessions.header.as_str())
        .and_then(|value| value.to_str().ok());

    from_header
        .map(str::to_string)
        .or_else(|| header_value(headers, "x-claude-code-session-id"))
        .or_else(|| header_value(headers, "session-id"))
        .or_else(|| header_value(headers, "thread-id"))
        .or_else(|| {
            body.get("prompt_cache_key")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
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

/// Reads one UTF-8 request header as an owned session identifier.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
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
///
/// The ceiling header is named for its unit — per Mtok on a token surface, per
/// image or job on a media one — so a reader never has to know the ladder to
/// know what the number means.
fn with_routing_headers(
    mut response: Response,
    ladder: &crate::config::Ladder,
    chosen: &Chosen,
    skipped: usize,
    session: Option<&str>,
    pinned: bool,
) -> Response {
    let headers = response.headers_mut();
    set(headers, types::HEADER_LADDER, &ladder.name);
    set(headers, types::HEADER_RUNG, &chosen.rung.to_string());
    set(headers, types::HEADER_PROVIDER, &chosen.provider);
    set(headers, types::HEADER_MODEL, &chosen.model);
    set(headers, types::HEADER_SKIPPED, &skipped.to_string());
    if let Some(cap) = chosen.cap_per_1m {
        let header = if ladder.surface.is_media() {
            types::HEADER_CAP_PER_UNIT
        } else {
            types::HEADER_CAP
        };
        set(headers, header, &cap.to_string());
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
