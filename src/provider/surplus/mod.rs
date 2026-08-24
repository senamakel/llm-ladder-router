//! The Surplus Intelligence dialect.
//!
//! Surplus publishes a full order book without authentication, which makes
//! local admission checks cheap and accurate. What it does not offer is a
//! working absolute price ceiling: `max_price_per_1m` and `X-Max-Price-Per-1M`
//! are both documented and both observably inert. The mechanism that does bind
//! is the `/min{N}/` path prefix, which filters sellers by their discount
//! against the model's direct price — so a rung's dollar ceiling is restated as
//! the equivalent discount before dispatch. See `docs/specs/marketplace-apis.md`.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::ladder::Chosen;
use crate::pricing::{ModelPrices, Offer};
use crate::provider::types::{Disposition, Wire};

/// Surplus quotes money as an integer number of micro-USD, as a string.
const MICRO_USD: f64 = 1_000_000.0;

/// `GET /api/markets/{model}`.
#[derive(Debug, Deserialize)]
struct OrderBook {
    offers: Vec<MarketOffer>,
}

#[derive(Debug, Deserialize)]
struct MarketOffer {
    provider: Option<String>,
    /// Micro-USD per million tokens.
    price_input_per_1m: Option<f64>,
    /// Micro-USD per million tokens.
    price_output_per_1m: Option<f64>,
    /// The undiscounted price the offer is quoted against, micro-USD per
    /// million tokens.
    direct_output_per_1m: Option<f64>,
    /// Micro-USD per [`MarketOffer::media_unit`], for a model that is not
    /// billed per prompt and completion token.
    media_unit_price: Option<f64>,
    /// The undiscounted counterpart of [`MarketOffer::media_unit_price`].
    direct_media_unit_price: Option<f64>,
    /// What a media unit is. Only `1M tokens` is read; see
    /// [`MarketOffer::per_1m`].
    media_unit: Option<String>,
    #[serde(default)]
    available: bool,
    #[serde(default)]
    healthy: bool,
}

/// The media unit that is denominated in the same thing the rest of the table
/// is, and so the only one whose price can be compared with a token price.
const MEDIA_UNIT_PER_1M: &str = "1M tokens";

impl MarketOffer {
    /// One of this offer's prices, in micro-USD per million tokens, falling
    /// back to the media-unit price when the per-token fields are zero.
    ///
    /// An embedding model is billed per unit of *input* rather than per prompt
    /// and completion token, so Surplus quotes it in `media_unit_price` and
    /// leaves `price_input_per_1m` and `price_output_per_1m` at zero — measured
    /// across all 175 offers on `venice-embed-1`. Read naively that is a market
    /// full of free sellers, which would give an embeddings rung a floor of
    /// zero and rank it ahead of every honestly-priced rung it competes with.
    ///
    /// The unit is checked rather than assumed: the same field prices an image
    /// model per image, and a per-image price compared against a per-token one
    /// is a worse answer than no price at all. An offer that publishes neither
    /// form is left at zero, exactly as a chat offer with missing fields is
    /// today — the marketplace does carry genuinely free models.
    ///
    /// The caller decides what `media` is, which is how a seller quoting no
    /// price of its own falls back to the undiscounted one rather than to zero:
    /// five of the 175 live offers on `venice-embed-1` are in that state, and
    /// one usable seller reading as free is enough to make the whole rung's
    /// floor zero and rank it ahead of every priced rung it competes with. The
    /// direct price is the honest reading of "this seller published no
    /// discount", and it errs upward, which is the safe direction for a number
    /// a ceiling is compared against.
    fn per_1m(&self, per_token: Option<f64>, media: Option<f64>) -> f64 {
        let per_token = per_token.unwrap_or(0.0);
        if per_token > 0.0 {
            return per_token;
        }
        if self.media_unit.as_deref() == Some(MEDIA_UNIT_PER_1M) {
            return media.unwrap_or(0.0);
        }
        per_token
    }
}

/// `GET /v1/buyer/me`.
#[derive(Debug, Deserialize)]
struct BuyerProfile {
    /// Micro-USD, as a string.
    balance_usdc: Option<String>,
    /// Micro-USD, as a string. A spending allowance below the balance is the
    /// real limit.
    allowance_usdc: Option<String>,
}

/// Parses an order book into normalized offers.
///
/// # Errors
///
/// Returns [`Error::UnreadablePayload`] if the body does not match the schema.
pub fn parse_order_book(body: &[u8]) -> Result<ModelPrices> {
    let book: OrderBook = serde_json::from_slice(body).map_err(|_| Error::UnreadablePayload {
        provider: "surplus".to_string(),
        what: "order book".to_string(),
    })?;

    let offers = book
        .offers
        .into_iter()
        .map(|offer| {
            let quoted = offer.media_unit_price.or(offer.direct_media_unit_price);
            let prompt = offer.per_1m(offer.price_input_per_1m, quoted);
            let completion = offer.per_1m(offer.price_output_per_1m, quoted);
            let direct = offer.per_1m(offer.direct_output_per_1m, offer.direct_media_unit_price);
            Offer {
                provider: offer
                    .provider
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                // Surplus does not expose a steering slug per seller, and its
                // `provider` allow-list names upstream families rather than
                // individual offers, so there is nothing to steer with.
                tag: None,
                prompt_per_1m: prompt / MICRO_USD,
                completion_per_1m: completion / MICRO_USD,
                // Kept as `None` when nothing was published, so
                // `discount_floor_pct` still skips an offer it cannot compute a
                // ratio against rather than dividing by a zero it invented.
                direct_completion_per_1m: (offer.direct_output_per_1m.is_some()
                    || offer.direct_media_unit_price.is_some())
                .then_some(direct / MICRO_USD),
                usable: offer.available && offer.healthy,
            }
        })
        .collect();

    Ok(ModelPrices::new(offers))
}

/// Parses a buyer profile into the spendable balance in USD.
///
/// # Errors
///
/// Returns [`Error::UnreadablePayload`] if the body does not match the schema.
pub fn parse_balance(body: &[u8]) -> Result<f64> {
    let profile: BuyerProfile =
        serde_json::from_slice(body).map_err(|_| Error::UnreadablePayload {
            provider: "surplus".to_string(),
            what: "buyer profile".to_string(),
        })?;

    let micro = |value: Option<String>| {
        value
            .as_deref()
            .and_then(|text| text.parse::<f64>().ok())
            .map(|amount| amount / MICRO_USD)
    };

    // An allowance below the balance is what actually limits spending.
    match (micro(profile.balance_usdc), micro(profile.allowance_usdc)) {
        (Some(balance), Some(allowance)) => Ok(balance.min(allowance)),
        (Some(only), None) | (None, Some(only)) => Ok(only),
        (None, None) => Err(Error::UnreadablePayload {
            provider: "surplus".to_string(),
            what: "buyer profile".to_string(),
        }),
    }
}

/// The path a model's order book lives at, relative to the base URL.
#[must_use]
pub fn order_book_path(model: &str) -> String {
    format!("/api/markets/{model}")
}

/// The path a buyer's profile lives at, relative to the base URL.
#[must_use]
pub fn balance_path() -> &'static str {
    "/v1/buyer/me"
}

/// Whether Surplus can usefully serve this surface for this request.
///
/// Everything except a **streamed** responses request. Surplus serves that
/// surface, and the non-streaming form is complete, but its event stream is
/// not: it emits `response.created`, `response.output_item.added`,
/// `response.output_text.delta` and `response.completed`, and stops there. It
/// never sends `response.output_item.done`, and the `response` on
/// `response.completed` carries no `output` array.
///
/// An agent client assembles its turn from the finished items, so it sees a
/// turn with no output and ends silently — the model's answer arrives in the
/// deltas and is discarded. That is worse than an error: the run looks like it
/// completed and produced nothing. Codex is the case in hand, and it speaks
/// only this surface, so refusing here is what lets the ladder step down to a
/// provider whose stream is complete (`OpenRouter` emits all nine events).
///
/// Refused before the round trip, in the manner of [`super::mistral::serves`],
/// so the failover loop reads it as this rung failing rather than as the
/// caller's mistake.
#[must_use]
pub fn serves(wire: Wire, streaming: bool) -> bool {
    !(wire == Wire::Responses && streaming)
}

/// The inference path for a chosen rung on a given wire format.
///
/// A rung with a ceiling routes through the `/min{N}/` prefix, which is the
/// only Surplus filter that actually binds. Note the prefix sits in a different
/// place on each surface: `/min{N}/v1/chat/completions` for `OpenAI`, but
/// `/anthropic/min{N}/v1/messages` for Anthropic. Both were verified against
/// the live API; the other orderings 404.
///
/// Embeddings have no discounted form at all. `/v1/embeddings` serves, and
/// every prefixed spelling of it — `/min{N}/v1/embeddings`,
/// `/v1/min{N}/embeddings`, `/embeddings/min{N}` — 404s, measured against the
/// live API on 2026-08-24 while `/min50/v1/chat/completions` answered on the
/// same run. That is why an embeddings ladder carries no ceiling: there is
/// nothing to express one with, and [`crate::config::Surface::is_cappable`]
/// says so at load time rather than letting a number sit in the file looking
/// like a limit.
#[must_use]
pub fn inference_path(chosen: &Chosen, wire: Wire) -> String {
    let discount = match chosen.min_discount_pct {
        Some(pct) if pct > 0 => Some(pct),
        _ => None,
    };
    match (wire, discount) {
        (Wire::OpenAi, Some(pct)) => format!("/min{pct}/v1/chat/completions"),
        (Wire::OpenAi, None) => "/v1/chat/completions".to_string(),
        (Wire::Anthropic, Some(pct)) => format!("/anthropic/min{pct}/v1/messages"),
        (Wire::Anthropic, None) => "/anthropic/v1/messages".to_string(),
        // The responses surface sits under the same root as chat completions,
        // not under the `/anthropic` one, and takes the discount prefix in the
        // same leading position.
        (Wire::Responses, Some(pct)) => format!("/min{pct}/v1/responses"),
        (Wire::Responses, None) => "/v1/responses".to_string(),
        // Embeddings have no discounted spelling at all; see above.
        (Wire::Embeddings, _) => "/v1/embeddings".to_string(),
    }
}

/// Applies a chosen rung to an outgoing request body.
///
/// The model is rewritten - the ceiling travels in the path, not the body -
/// and the `developer` role is folded back to `system`; see
/// `fold_developer_role` for why that one exception exists.
pub fn apply_routing(body: &mut serde_json::Value, chosen: &Chosen) {
    if let Some(object) = body.as_object_mut() {
        object.insert("model".to_string(), chosen.model.clone().into());
    }
    fold_developer_role(body);
}

/// Rewrites every `developer` role in an outgoing body to `system`.
///
/// This is the one place the router edits a caller's content rather than
/// relaying it, and it is here because the alternative is not relaying at all.
/// Surplus's request schema predates the `developer` role and rejects it
/// outright, on every surface:
///
/// ```text
/// 400 Failed to deserialize the JSON body into the target type:
///     messages[1].role: unknown variant `developer`,
///     expected one of `system`, `user`, `assistant`, `tool`
/// ```
///
/// The `codex` harness opens every turn with a `developer` message, so without
/// this no Codex session can reach a Surplus rung - not a step down to the next
/// one either, because a 400 is a statement about the request and
/// [`super::types::classify_status`] deliberately hands those straight back.
/// The role is also not a real distinction here: `developer` is `system`
/// renamed, and Surplus's own schema still spells it the old way. `OpenRouter`
/// accepts either, so nothing else needs this.
///
/// Both the chat shape (`messages`) and the responses shape (`input`) are
/// covered; Surplus converts the latter internally and reports its complaint
/// against `messages` regardless.
fn fold_developer_role(body: &mut serde_json::Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    for key in ["messages", "input"] {
        let Some(items) = object
            .get_mut(key)
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for item in items {
            let Some(role) = item.get_mut("role") else {
                continue;
            };
            if role.as_str() == Some("developer") {
                *role = serde_json::Value::String("system".to_string());
            }
        }
    }
}

/// Whether an error response is Surplus's way of saying the rung cannot be
/// served, rather than the caller's mistake.
#[must_use]
pub fn classify(status: reqwest::StatusCode, body: &[u8]) -> Disposition {
    let text = String::from_utf8_lossy(body);

    // The discount filter matched nothing, no seller carries the model, or the
    // balance ran out. All three mean this rung cannot serve, and all three
    // arrive with a status that would otherwise read as a caller error.
    if text.contains("minimum_discount_not_met")
        || text.contains("no_sellers_for_model")
        || status == reqwest::StatusCode::PAYMENT_REQUIRED
    {
        return Disposition::Advance;
    }

    super::types::classify_status(status)
}

#[cfg(test)]
mod test;
