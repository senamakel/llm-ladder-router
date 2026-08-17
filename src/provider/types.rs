//! Types shared by both marketplace dialects.

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
