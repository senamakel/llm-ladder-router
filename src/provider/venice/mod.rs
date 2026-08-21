//! Venice's own API, reached directly rather than through a reseller.
//!
//! Like [`super::mistral`] this is a direct endpoint — one seller, one price,
//! no order book to read and nothing for a ceiling to bind against. It exists
//! for the same reason: to reach a model whose availability on a marketplace
//! cannot be assumed. Here that model is the uncensored one, and Venice is the
//! house that publishes it, so a rung here is the floor under a tier that would
//! otherwise be at the mercy of whoever happens to be reselling today.
//!
//! One thing does differ from Mistral, and it is the whole reason a caller
//! picks this provider: Venice prepends a system prompt of its own to every
//! request unless told not to. A tier that asked for an unmoderated model and
//! silently got Venice's framing on top of it is not the tier that was asked
//! for, so [`apply_routing`] turns that off — while leaving a caller who set
//! `venice_parameters` themselves in charge of their own request.

use crate::ladder::Chosen;

use super::{Disposition, Wire};

/// The inference path, relative to the base URL.
///
/// Venice roots its `OpenAI`-compatible surface at `/api/v1` rather than at
/// `/v1`, so the base URL is the bare host and the version lives here.
#[must_use]
pub fn inference_path() -> &'static str {
    "/api/v1/chat/completions"
}

/// Whether this provider serves a wire format at all.
///
/// Venice publishes no Anthropic Messages surface, so an Anthropic-wire request
/// is declined before the round trip rather than relayed to an endpoint that
/// would answer it with a 400. The failover loop reads the refusal as this
/// rung's own failure and takes the next one.
#[must_use]
pub fn serves(wire: Wire) -> bool {
    wire == Wire::OpenAi
}

/// Applies a chosen rung to an outgoing request body.
///
/// Two rewrites. The model, because that is what a rung on a direct endpoint
/// is. And `venice_parameters.include_venice_system_prompt`, set to `false`,
/// because Venice otherwise prepends its own system prompt to the conversation
/// — which on the tier this provider exists to serve would quietly reintroduce
/// the framing the caller chose the model to avoid.
///
/// A caller who sent their own `venice_parameters` object keeps every key they
/// set, including this one: an explicit request beats a default, and the
/// default is only here for the callers who never heard of the field.
pub fn apply_routing(body: &mut serde_json::Value, chosen: &Chosen) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    object.insert("model".to_string(), chosen.model.clone().into());

    let parameters = object
        .entry("venice_parameters")
        .or_insert_with(|| serde_json::json!({}));
    // A caller who sent a non-object here has sent Venice something Venice will
    // reject; overwriting it would hide their mistake behind a 200 from a
    // request they did not make.
    if let Some(parameters) = parameters.as_object_mut()
        && !parameters.contains_key("include_venice_system_prompt")
    {
        parameters.insert("include_venice_system_prompt".to_string(), false.into());
    }
}

/// Whether an error response means this rung cannot serve, rather than that the
/// caller got the request wrong.
///
/// A direct endpoint has no discount filter to miss and no seller to be out of
/// stock, so the shared status rules are the whole policy.
#[must_use]
pub fn classify(status: reqwest::StatusCode, _body: &[u8]) -> Disposition {
    super::types::classify_status(status)
}

#[cfg(test)]
mod test;
