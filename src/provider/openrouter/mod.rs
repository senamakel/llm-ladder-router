//! The `OpenRouter` dialect.
//!
//! `OpenRouter` publishes a per-sub-provider price list and enforces a price
//! ceiling directly, so a rung's dollar cap survives all the way to the wire.
//! An unsatisfiable cap comes back as a 404 naming the max price, which is a
//! reason to advance the ladder rather than a caller error.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::ladder::Chosen;
use crate::pricing::{ModelPrices, Offer};
use crate::provider::types::{Disposition, Wire};

/// `GET /models/{model}/endpoints`.
#[derive(Debug, Deserialize)]
struct EndpointsEnvelope {
    data: EndpointsData,
}

#[derive(Debug, Deserialize)]
struct EndpointsData {
    endpoints: Vec<Endpoint>,
}

#[derive(Debug, Deserialize)]
struct Endpoint {
    provider_name: String,
    /// The slug `provider.order` expects. It is not simply the lowercased
    /// display name — "`DigitalOcean`" is tagged `digitalocean`, but a quantized
    /// endpoint is tagged `deepinfra/fp8` — so it must be read, not derived.
    tag: Option<String>,
    pricing: EndpointPricing,
    /// Negative values mark a deranked or disabled endpoint.
    #[serde(default)]
    status: i64,
}

#[derive(Debug, Deserialize)]
struct EndpointPricing {
    prompt: String,
    completion: String,
}

/// `GET /credits`.
#[derive(Debug, Deserialize)]
struct CreditsEnvelope {
    data: CreditsData,
}

#[derive(Debug, Deserialize)]
struct CreditsData {
    total_credits: f64,
    total_usage: f64,
}

/// Parses an endpoints payload into normalized offers.
///
/// `OpenRouter` quotes USD per token, so every price is scaled to USD per million
/// tokens here and nowhere else.
///
/// # Errors
///
/// Returns [`Error::UnreadablePayload`] if the body does not match the schema.
pub fn parse_endpoints(body: &[u8]) -> Result<ModelPrices> {
    let envelope: EndpointsEnvelope =
        serde_json::from_slice(body).map_err(|_| Error::UnreadablePayload {
            provider: "openrouter".to_string(),
            what: "endpoints".to_string(),
        })?;

    let offers = envelope
        .data
        .endpoints
        .into_iter()
        .map(|endpoint| Offer {
            provider: endpoint.provider_name,
            tag: endpoint.tag,
            prompt_per_1m: per_million(&endpoint.pricing.prompt),
            completion_per_1m: per_million(&endpoint.pricing.completion),
            direct_completion_per_1m: None,
            usable: endpoint.status >= 0,
        })
        .collect();

    Ok(ModelPrices::new(offers))
}

/// Parses a credits payload into the remaining balance in USD.
///
/// # Errors
///
/// Returns [`Error::UnreadablePayload`] if the body does not match the schema.
pub fn parse_credits(body: &[u8]) -> Result<f64> {
    let envelope: CreditsEnvelope =
        serde_json::from_slice(body).map_err(|_| Error::UnreadablePayload {
            provider: "openrouter".to_string(),
            what: "credits".to_string(),
        })?;
    Ok(envelope.data.total_credits - envelope.data.total_usage)
}

/// Prices are quoted per token; everything downstream works per million.
fn per_million(price: &str) -> f64 {
    price.parse::<f64>().unwrap_or(0.0) * 1_000_000.0
}

/// The path a model's endpoint listing lives at, relative to the base URL.
#[must_use]
pub fn endpoints_path(model: &str) -> String {
    format!("/models/{model}/endpoints")
}

/// The inference path for a given wire format, relative to the base URL.
///
/// The ceiling travels in the body on both surfaces, so unlike Surplus the path
/// does not vary with the rung.
#[must_use]
pub fn inference_path(wire: Wire) -> &'static str {
    match wire {
        Wire::OpenAi => "/chat/completions",
        Wire::Anthropic => "/messages",
        Wire::Responses => "/responses",
    }
}

/// Applies a chosen rung's ceiling and preferences to an outgoing request body.
///
/// The ceiling is sent even though the router already filtered locally: the
/// local check skips a doomed rung for free, while this is what actually binds
/// if the cached prices have moved since the last refresh.
pub fn apply_routing(body: &mut serde_json::Value, chosen: &Chosen) {
    let Some(object) = body.as_object_mut() else {
        return;
    };

    object.insert("model".to_string(), chosen.model.clone().into());

    let mut provider = serde_json::Map::new();
    if let Some(cap) = chosen.cap_per_1m {
        provider.insert(
            "max_price".to_string(),
            serde_json::json!({ "completion": cap }),
        );
    }
    if !chosen.prefer.is_empty() {
        provider.insert("order".to_string(), chosen.prefer.clone().into());
        // Never pin exclusively: an exclusive pin has been observed to leave
        // requests hanging while idle sub-providers sat unused.
        provider.insert("allow_fallbacks".to_string(), true.into());
    }
    if !provider.is_empty() {
        object.insert("provider".to_string(), provider.into());
    }
}

/// Whether an error response is `OpenRouter`'s way of saying the rung cannot be
/// served, rather than the caller's mistake.
#[must_use]
pub fn classify(status: reqwest::StatusCode, body: &[u8]) -> Disposition {
    let text = String::from_utf8_lossy(body);

    // An unsatisfiable price ceiling arrives as a 404, which would otherwise
    // read as a caller error.
    if status == reqwest::StatusCode::NOT_FOUND && text.contains("satisfy the max price") {
        return Disposition::Advance;
    }
    // An upstream sub-provider failure is reported as a 400, which would
    // otherwise stop the ladder.
    if status == reqwest::StatusCode::BAD_REQUEST && text.contains("Provider returned error") {
        return Disposition::Advance;
    }

    super::types::classify_status(status)
}

#[cfg(test)]
mod test;
