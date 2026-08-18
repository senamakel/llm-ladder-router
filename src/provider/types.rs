//! Types shared by both marketplace dialects.

use crate::ladder::Chosen;

/// Which request/response format a caller is speaking.
///
/// Both marketplaces serve both formats natively, so the router relays rather
/// than translating: an Anthropic request reaches an Anthropic endpoint
/// unchanged, and its response comes back unchanged. Translating between the
/// two would lose fields on every round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// `OpenAI` chat completions.
    OpenAi,
    /// Anthropic messages.
    Anthropic,
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
/// - **Only on the `OpenAI` surface.** Anthropic spells this as a `thinking`
///   block with a token budget, and inventing one from an effort word would be
///   the router translating between dialects rather than relaying.
/// - **Nothing is inserted when no effort was declared**, so every ladder that
///   predates this field behaves exactly as it did.
pub fn apply_reasoning_effort(body: &mut serde_json::Value, chosen: &Chosen, wire: Wire) {
    if wire != Wire::OpenAi {
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
    object.insert("reasoning_effort".to_string(), effort.clone().into());
}

/// Classifies an upstream response body by the marketplace-independent rules.
///
/// Marketplace-specific codes are handled by each provider module before this
/// is consulted.
#[must_use]
pub fn classify_status(status: reqwest::StatusCode) -> Disposition {
    if status.is_success() {
        return Disposition::Served;
    }
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Disposition::Advance;
    }
    Disposition::CallerError
}
