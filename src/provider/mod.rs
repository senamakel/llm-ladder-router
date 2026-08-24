//! Talking to the marketplaces.
//!
//! Each marketplace gets its own submodule because their dialects genuinely
//! differ: where the price ceiling travels, which errors mean "try the next
//! rung", and what shape the price data arrives in. Sending one dialect's
//! request shape to the other is a bug, not a cosmetic difference, so the
//! dispatch below always branches on [`ProviderKind`].

pub mod mistral;
pub mod openrouter;
pub mod surplus;
mod types;

pub use types::{
    Dispatched, Disposition, Wire, apply_reasoning_effort, classify_status, parse_retry_after,
};

use crate::config::{Provider, ProviderKind};
use crate::error::{Error, Result};
use crate::ladder::Chosen;
use crate::pricing::ModelPrices;

/// An HTTP client bound to one configured marketplace.
#[derive(Debug, Clone)]
pub struct Client {
    name: String,
    provider: Provider,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl Client {
    /// Binds a client to a configured provider, reading its credential from the
    /// environment.
    ///
    /// A missing credential is not an error here: the router reports it as a
    /// skip reason so the remaining rungs still work.
    #[must_use]
    pub fn new(name: impl Into<String>, provider: Provider, http: reqwest::Client) -> Self {
        let api_key = std::env::var(&provider.api_key_env)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        Self::with_credential(name, provider, http, api_key)
    }

    /// Binds a client to a configured provider using a credential the caller
    /// already has.
    ///
    /// Useful when secrets arrive from somewhere other than the environment —
    /// a secret manager, a sealed file, or a test harness.
    #[must_use]
    pub fn with_credential(
        name: impl Into<String>,
        provider: Provider,
        http: reqwest::Client,
        api_key: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            provider,
            api_key: api_key
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            http,
        }
    }

    /// The configured name this provider is known by in ladders.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this provider serves a wire format at all.
    ///
    /// Both marketplaces publish all three surfaces; Mistral publishes no
    /// Anthropic Messages endpoint. A rung on a provider that does not serve
    /// the caller's surface declines before the round trip — see
    /// [`Client::infer`].
    ///
    /// Answering "yes" is not a claim that the *model* named by the rung suits
    /// the surface. `OpenRouter` serves `/embeddings` but lists no embedding
    /// model, so a rung pointing there is a configuration mistake the upstream
    /// reports, not one this router can see.
    #[must_use]
    pub fn serves(&self, wire: Wire) -> bool {
        match self.provider.kind {
            ProviderKind::OpenRouter | ProviderKind::Surplus => true,
            ProviderKind::Mistral => mistral::serves(wire),
        }
    }

    /// Whether this provider's credential was present in the environment.
    #[must_use]
    pub fn has_credential(&self) -> bool {
        self.api_key.is_some()
    }

    /// The environment variable this provider reads its credential from.
    #[must_use]
    pub fn credential_variable(&self) -> &str {
        &self.provider.api_key_env
    }

    /// Fetches and parses the current offers for one model.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Upstream`] if the request fails and
    /// [`Error::UnreadablePayload`] if the response does not match the schema.
    pub async fn fetch_prices(&self, model: &str) -> Result<ModelPrices> {
        let path = match self.provider.kind {
            ProviderKind::OpenRouter => openrouter::endpoints_path(model),
            ProviderKind::Surplus => surplus::order_book_path(model),
            ProviderKind::Mistral => return Err(self.no_market_data("order book")),
        };
        let body = self.get(&path).await?;
        match self.provider.kind {
            ProviderKind::OpenRouter => openrouter::parse_endpoints(&body),
            ProviderKind::Surplus => surplus::parse_order_book(&body),
            ProviderKind::Mistral => Err(self.no_market_data("order book")),
        }
    }

    /// Whether this provider resells many sellers, and so has prices to poll.
    ///
    /// The refreshers ask before calling: a direct endpoint has nothing to
    /// return, and a warning logged every cycle about a provider working
    /// exactly as intended is how a log stops being read.
    #[must_use]
    pub fn is_marketplace(&self) -> bool {
        self.provider.kind.is_marketplace()
    }

    fn no_market_data(&self, what: &str) -> Error {
        Error::NoMarketData {
            provider: self.name.clone(),
            what: what.to_string(),
        }
    }

    /// Fetches the remaining spendable balance, in USD.
    ///
    /// # Errors
    ///
    /// As [`Client::fetch_prices`].
    pub async fn fetch_balance(&self) -> Result<f64> {
        let path = match self.provider.kind {
            ProviderKind::OpenRouter => "/credits".to_string(),
            ProviderKind::Surplus => surplus::balance_path().to_string(),
            ProviderKind::Mistral => return Err(self.no_market_data("balance")),
        };
        let body = self.get(&path).await?;
        match self.provider.kind {
            ProviderKind::OpenRouter => openrouter::parse_credits(&body),
            ProviderKind::Surplus => surplus::parse_balance(&body),
            ProviderKind::Mistral => Err(self.no_market_data("balance")),
        }
    }

    /// Sends a chat completion for a chosen rung and reports what came back.
    ///
    /// The caller's body is relayed with only the routing fields rewritten, so
    /// parameters this router does not model survive the round trip untouched.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Upstream`] if the request could not be completed.
    pub async fn infer(
        &self,
        chosen: &Chosen,
        wire: Wire,
        body: &serde_json::Value,
    ) -> Result<Dispatched> {
        // Refused before the round trip rather than after it, and asked of
        // every dialect rather than of the one that happens to decline today:
        // the failover loop reads this as the rung's own failure and moves to
        // the next.
        if !self.serves(wire) {
            return Err(Error::UnsupportedWire {
                provider: self.name.clone(),
                wire: wire.name().to_string(),
            });
        }

        let mut body = body.clone();
        // Depth first, then the marketplace's own rewrites: both appliers only
        // add fields, and doing it here rather than inside each dialect keeps
        // the rule one rule.
        types::apply_reasoning_effort(&mut body, chosen, wire);
        let path = match self.provider.kind {
            ProviderKind::OpenRouter => {
                openrouter::apply_routing(&mut body, chosen);
                openrouter::inference_path(wire).to_string()
            }
            ProviderKind::Surplus => {
                surplus::apply_routing(&mut body, chosen);
                surplus::inference_path(chosen, wire)
            }
            ProviderKind::Mistral => {
                mistral::apply_routing(&mut body, chosen);
                mistral::inference_path(wire).to_string()
            }
        };

        let mut request = self.request(reqwest::Method::POST, &path);
        if wire == Wire::Anthropic {
            // Anthropic's API requires an explicit version; both marketplaces
            // reject a Messages request without one.
            request = request.header("anthropic-version", "2023-06-01");
        }

        let response = request
            .json(&body)
            .send()
            .await
            .map_err(|source| Error::Upstream {
                provider: self.name.clone(),
                source,
            })?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let retry_after = types::parse_retry_after(
            response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
        );
        let bytes = response.bytes().await.map_err(|source| Error::Upstream {
            provider: self.name.clone(),
            source,
        })?;
        let body = bytes.to_vec();

        Ok(Dispatched {
            status,
            served_by: served_by(&body),
            content_type,
            retry_after,
            body,
        })
    }

    /// Classifies a completed attempt against this provider's dialect.
    #[must_use]
    pub fn classify(&self, dispatched: &Dispatched) -> Disposition {
        match self.provider.kind {
            ProviderKind::OpenRouter => openrouter::classify(dispatched.status, &dispatched.body),
            ProviderKind::Surplus => surplus::classify(dispatched.status, &dispatched.body),
            ProviderKind::Mistral => mistral::classify(dispatched.status, &dispatched.body),
        }
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let response = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(|source| Error::Upstream {
                provider: self.name.clone(),
                source,
            })?;
        let bytes = response.bytes().await.map_err(|source| Error::Upstream {
            provider: self.name.clone(),
            source,
        })?;
        Ok(bytes.to_vec())
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{path}", self.provider.base_url.trim_end_matches('/'));
        let mut builder = self.http.request(method, url);
        if let Some(api_key) = &self.api_key {
            builder = builder.bearer_auth(api_key);
        }
        for (name, value) in &self.provider.headers {
            builder = builder.header(name, value);
        }
        builder
    }
}

/// The sub-provider that served, as reported in an `OpenAI`-shaped response body.
///
/// Both marketplaces put the terminal upstream in a top-level `provider` field.
fn served_by(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("provider")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod test;
