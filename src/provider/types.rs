//! Types shared by both marketplace dialects.

use crate::ladder::Chosen;

/// Which request/response format a caller is speaking.
///
/// Both marketplaces serve all three formats natively, so the router relays
/// rather than translating: an Anthropic request reaches an Anthropic endpoint
/// unchanged, and its response comes back unchanged. Translating between them
/// would lose fields on every round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// `OpenAI` chat completions.
    OpenAi,
    /// Anthropic messages.
    Anthropic,
    /// `OpenAI` responses.
    ///
    /// A distinct surface rather than a flavour of [`Wire::OpenAi`]: it has its
    /// own request shape (`input` rather than `messages`), its own response
    /// shape, and its own spelling of reasoning depth. It is also the only
    /// surface some agent harnesses speak — the `codex` harness posts to
    /// `/responses` and nothing else, so without this it cannot reach the
    /// router at all.
    Responses,
}

impl Wire {
    /// The surface's name, for error messages a caller has to act on.
    #[must_use]
    pub fn api_name(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI Chat Completions",
            Self::Anthropic => "Anthropic Messages",
            Self::Responses => "OpenAI Responses",
        }
    }
}

/// What a dispatched rung produced.
#[derive(Debug)]
pub struct Dispatched {
    /// The upstream status code.
    pub status: reqwest::StatusCode,
    /// The upstream response body.
    pub body: Vec<u8>,
    /// The sub-provider that actually served, when the upstream names one.
    pub served_by: Option<String>,
    /// The upstream content type, relayed unchanged so a streaming response
    /// stays a streaming response.
    pub content_type: Option<String>,
    /// The backoff the upstream asked for on a rate limit, when it named one.
    ///
    /// Only the delta-seconds form is read. The HTTP-date form is legal and
    /// nothing observed here sends it; guessing at a clock skew to convert one
    /// would be a worse answer than falling back to the configured default.
    pub retry_after: Option<std::time::Duration>,
}

/// Reads a `Retry-After` header in its delta-seconds form.
///
/// A zero is `None` rather than a zero-length cooldown: an upstream saying
/// "retry immediately" is not asking to be parked, and parking it for no time
/// at all would only add a map entry.
#[must_use]
pub fn parse_retry_after(value: Option<&str>) -> Option<std::time::Duration> {
    let seconds: u64 = value?.trim().parse().ok()?;
    (seconds > 0).then(|| std::time::Duration::from_secs(seconds))
}

/// Whether a failed attempt should advance the ladder or be returned as-is.
///
/// The distinction is the load-bearing routing policy: an upstream that failed
/// on its own account is worth retrying elsewhere, while a request the caller
/// got wrong would fail identically at every rung and must not be replayed and
/// charged again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// The upstream served the request; relay the response.
    Served,
    /// The upstream failed on its own account; try the next rung.
    Advance,
    /// The caller's request is at fault; return the response unchanged.
    CallerError,
}

/// Applies a chosen rung's reasoning depth to an outgoing request body.
///
/// Three rules, and each one is a failure it prevents:
///
/// - **The caller always wins.** A body that already carries `reasoning_effort`
///   or `reasoning` is left alone, so a request asking for a shallow answer is
///   not silently made expensive by the ladder it happened to select.
/// - **Only on the two `OpenAI` surfaces.** Anthropic spells this as a
///   `thinking` block with a token budget, and inventing one from an effort word
///   would be the router translating between dialects rather than relaying.
/// - **Nothing is inserted when no effort was declared**, so every ladder that
///   predates this field behaves exactly as it did.
///
/// The two `OpenAI` surfaces spell the same idea differently, and each is given
/// its own spelling rather than one being sent to both: chat completions take a
/// top-level `reasoning_effort` string, while responses take a `reasoning`
/// object with an `effort` member. Sending the chat spelling to `/responses`
/// puts an unknown top-level key in the body, which is the sort of thing an
/// upstream is entitled to reject — and the depth the ladder paid for would
/// silently not be bought.
pub fn apply_reasoning_effort(body: &mut serde_json::Value, chosen: &Chosen, wire: Wire) {
    if wire == Wire::Anthropic {
        return;
    }
    let Some(effort) = chosen.reasoning_effort.as_ref() else {
        return;
    };
    let Some(object) = body.as_object_mut() else {
        return;
    };
    if object.contains_key("reasoning_effort") || object.contains_key("reasoning") {
        return;
    }
    match wire {
        Wire::Responses => {
            object.insert(
                "reasoning".to_string(),
                serde_json::json!({ "effort": effort.clone() }),
            );
        }
        _ => {
            object.insert("reasoning_effort".to_string(), effort.clone().into());
        }
    }
}

/// Whether an outgoing request asked for a streamed response.
///
/// A missing or non-boolean `stream` is not streaming, which matches every
/// surface's own default.
#[must_use]
pub fn is_streaming(body: &serde_json::Value) -> bool {
    body.get("stream").and_then(serde_json::Value::as_bool) == Some(true)
}

/// Classifies an upstream response body by the marketplace-independent rules.
///
/// Marketplace-specific codes are handled by each provider module before this
/// is consulted.
///
/// The question this answers is *whose fault was it*, and the answer decides
/// whether the ladder steps down or hands the status back. Two families
/// advance:
///
/// - **The upstream broke**: any 5xx, plus a 408, plus a 429 it chose to send.
/// - **The upstream refused *us***: 401, 403 and 407. These read as caller
///   errors and are not, because there are two authentications in play and
///   they are not the same one. The caller authenticates to this router; the
///   router authenticates to the marketplace with a credential the caller has
///   never seen and cannot fix. So a 401 or 403 from upstream means "this
///   provider will not serve this router", which is the definition of a rung
///   that cannot serve — exactly what the next rung exists for.
///
/// That distinction was learned from an outage rather than reasoned out.
/// Surplus spent about fifteen minutes answering `403 Forbidden` as an HTML
/// page from its own edge. Every ladder passed it straight back, five long
/// agent runs died inside the same minute, and the rungs on a second provider
/// sitting directly below were never tried.
///
/// What deliberately does **not** advance is the rest of 4xx — 400, 404, 413,
/// 422 and their neighbours. Those are statements about the request, and the
/// request is the same at every rung, so walking the ladder would produce the
/// identical refusal N times and report the last one. A provider module may
/// still override a specific code it knows to mean something else, which is
/// what `openrouter::classify` does with a 404 that is really a price ceiling.
#[must_use]
pub fn classify_status(status: reqwest::StatusCode) -> Disposition {
    if status.is_success() {
        return Disposition::Served;
    }
    if status.is_server_error() {
        return Disposition::Advance;
    }
    match status {
        reqwest::StatusCode::TOO_MANY_REQUESTS
        | reqwest::StatusCode::REQUEST_TIMEOUT
        | reqwest::StatusCode::UNAUTHORIZED
        | reqwest::StatusCode::FORBIDDEN
        | reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED => Disposition::Advance,
        _ => Disposition::CallerError,
    }
}
