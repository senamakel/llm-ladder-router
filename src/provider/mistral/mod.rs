//! Mistral's own API, reached directly rather than through a reseller.
//!
//! The other two modules here speak to marketplaces: many sellers, prices that
//! move, a ceiling to enforce, and a discount filter to express it with. None
//! of that exists here. There is one seller at one price, so a rung on this
//! provider is a choice of *model* — which is exactly what it is for, since the
//! models worth reaching this way are the ones no marketplace carries.
//!
//! What that leaves is small on purpose: rewrite the model, know the path, and
//! read a status code.

use crate::ladder::Chosen;

use super::{Disposition, Wire};

/// The inference path for a wire format, relative to the base URL.
///
/// Only the `OpenAI` chat-completions and embeddings surfaces exist; a request
/// on any other wire is refused before it is sent rather than translated. See
/// [`serves`].
#[must_use]
pub fn inference_path(wire: Wire) -> &'static str {
    match wire {
        Wire::Anthropic | Wire::OpenAi | Wire::Responses => "/v1/chat/completions",
        Wire::Embeddings => "/v1/embeddings",
    }
}

/// Whether this provider serves a wire format at all.
///
/// Mistral publishes neither an Anthropic Messages surface nor an `OpenAI`
/// Responses one — `/v1/responses` answers 404. Relaying either body to the
/// chat-completions endpoint would be a 400 the caller cannot act on and a
/// round trip nobody needed, so the rung declines instead — which the failover
/// loop treats as this rung's failure and takes the next one. It does publish
/// embeddings, so that surface is served.
#[must_use]
pub fn serves(wire: Wire) -> bool {
    match wire {
        Wire::OpenAi | Wire::Embeddings => true,
        Wire::Anthropic | Wire::Responses => false,
    }
}

/// Applies a chosen rung to an outgoing request body.
///
/// Only the model, because there is no ceiling to send: the caller named a
/// ladder, and this is the model that ladder resolved to.
pub fn apply_routing(body: &mut serde_json::Value, chosen: &Chosen) {
    if let Some(object) = body.as_object_mut() {
        object.insert("model".to_string(), chosen.model.clone().into());
    }
}

/// Whether an error response means this rung cannot serve, rather than that the
/// caller got the request wrong.
///
/// Nothing marketplace-specific to add: a direct endpoint has no discount
/// filter to miss and no seller to be out of stock, so the shared status rules
/// are the whole policy.
#[must_use]
pub fn classify(status: reqwest::StatusCode, _body: &[u8]) -> Disposition {
    super::types::classify_status(status)
}

#[cfg(test)]
mod test;
