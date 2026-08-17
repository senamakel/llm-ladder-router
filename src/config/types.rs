//! Configuration types deserialized from `config.toml`.
//!
//! Credentials are named by environment variable rather than inlined, so a
//! `config.toml` stays safe to commit while `.env` holds the secret.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

/// The whole router configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where the proxy listens.
    #[serde(default)]
    pub server: Server,
    /// Marketplaces the ladders may draw on, keyed by the name rungs refer to.
    pub providers: BTreeMap<String, Provider>,
    /// How often price data is refreshed and when it goes stale.
    #[serde(default)]
    pub pricing: Pricing,
    /// How often account balances are polled, and the floor below which a
    /// provider is considered spent.
    #[serde(default)]
    pub credits: Credits,
    /// The ladders, in no particular order; requests select one by name.
    pub ladders: Vec<Ladder>,
}

/// Proxy listener settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// The socket address to bind.
    #[serde(default = "Server::default_bind")]
    pub bind: String,
    /// How long a single upstream attempt may take before the rung is
    /// considered failed and the ladder advances.
    #[serde(default = "Server::default_request_timeout", with = "humantime_serde")]
    pub request_timeout: Duration,
    /// The key callers must present to use this router.
    ///
    /// Accepted as `Authorization: Bearer <key>` or, for the Anthropic surface,
    /// `x-api-key: <key>`. When unset the router accepts every caller, which is
    /// only appropriate on a loopback bind.
    pub api_key: Option<String>,
    /// An environment variable to read the caller key from instead of writing
    /// it into the file.
    ///
    /// Takes precedence over [`Server::api_key`] when the variable is set and
    /// non-empty.
    pub api_key_env: Option<String>,
}

impl Server {
    /// The key callers must present, resolved from the environment first and
    /// the configuration file second.
    ///
    /// Returns `None` when neither is set, meaning the router is unauthenticated.
    #[must_use]
    pub fn resolved_api_key(&self) -> Option<String> {
        self.api_key_env
            .as_ref()
            .and_then(|variable| std::env::var(variable).ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                self.api_key
                    .as_ref()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
    }
}

impl Server {
    fn default_bind() -> String {
        "127.0.0.1:6969".to_string()
    }

    fn default_request_timeout() -> Duration {
        Duration::from_secs(120)
    }
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: Self::default_bind(),
            request_timeout: Self::default_request_timeout(),
            api_key: None,
            api_key_env: None,
        }
    }
}

/// Which marketplace dialect a provider speaks.
///
/// This is not cosmetic: each marketplace has its own price-cap mechanism, its
/// own order-book endpoint, and its own set of errors that mean "advance the
/// ladder". Sending one dialect's request shape to the other is a bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenRouter, which enforces `provider.max_price` directly.
    ///
    /// Spelled as one word, which is how the marketplace spells itself;
    /// `open_router` is accepted too so a snake_case habit is not punished.
    #[serde(rename = "openrouter", alias = "open_router")]
    OpenRouter,
    /// Surplus Intelligence, whose cap is expressed as a minimum discount.
    Surplus,
}

/// One marketplace the router can dispatch to.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// The dialect this provider speaks.
    pub kind: ProviderKind,
    /// The API root, without a trailing slash.
    pub base_url: String,
    /// The environment variable holding this provider's bearer token.
    pub api_key_env: String,
    /// The most any rung on this provider may pay, in USD per million tokens.
    ///
    /// Both marketplaces resell many sub-providers at prices that move
    /// independently, so this is the ceiling that holds regardless of which
    /// ladder a request took. It combines with [`Rung::max_cost_per_1m`] by
    /// taking the tighter of the two.
    pub max_cost_per_1m: Option<f64>,
    /// Extra headers sent on every request, such as OpenRouter attribution.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// Price-refresh policy.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    /// How often to re-read every referenced model's price data.
    #[serde(default = "Pricing::default_refresh", with = "humantime_serde")]
    pub refresh: Duration,
    /// How old price data may be before a capped rung is skipped rather than
    /// admitted on a guess.
    #[serde(default = "Pricing::default_stale_after", with = "humantime_serde")]
    pub stale_after: Duration,
}

impl Pricing {
    fn default_refresh() -> Duration {
        Duration::from_secs(15 * 60)
    }

    fn default_stale_after() -> Duration {
        Duration::from_secs(60 * 60)
    }
}

impl Default for Pricing {
    fn default() -> Self {
        Self {
            refresh: Self::default_refresh(),
            stale_after: Self::default_stale_after(),
        }
    }
}

/// Balance-polling policy.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credits {
    /// How often to re-read every provider's remaining balance.
    #[serde(default = "Credits::default_refresh", with = "humantime_serde")]
    pub refresh: Duration,
    /// A provider whose remaining balance is below this is skipped entirely.
    #[serde(default)]
    pub min_balance_usd: f64,
}

impl Credits {
    fn default_refresh() -> Duration {
        Duration::from_secs(5 * 60)
    }
}

impl Default for Credits {
    fn default() -> Self {
        Self {
            refresh: Self::default_refresh(),
            min_balance_usd: 0.0,
        }
    }
}

/// Which token price a rung's ceiling applies to.
///
/// Output tokens dominate the bill for most workloads, so that is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    /// Compare against the completion (output) price alone.
    #[default]
    Completion,
    /// Compare against the prompt (input) price alone.
    Prompt,
    /// Compare against the mean of prompt and completion prices.
    Blended,
}

/// An ordered list of rungs tried in sequence.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ladder {
    /// The name a request uses to select this ladder.
    pub name: String,
    /// Which price the rung ceilings apply to.
    #[serde(default)]
    pub cost_basis: CostBasis,
    /// The rungs, tried first to last.
    pub rungs: Vec<Rung>,
}

/// One (provider, model, ceiling) step of a ladder.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rung {
    /// The key into [`Config::providers`].
    pub provider: String,
    /// The model slug, in that provider's own naming convention.
    pub model: String,
    /// The most this rung may pay, in USD per million tokens. A rung with no
    /// ceiling of its own inherits the provider's, and a rung with neither
    /// admits any seller and is the quality preference.
    pub max_cost_per_1m: Option<f64>,
    /// Sub-providers to prefer when the marketplace supports steering.
    #[serde(default)]
    pub prefer: Vec<String>,
}

impl Rung {
    /// The ceiling that actually applies to this rung, in USD per million
    /// tokens.
    ///
    /// A rung ceiling and a provider ceiling are both upper bounds, so the
    /// tighter one wins. `None` means neither was set and any seller is
    /// admitted.
    #[must_use]
    pub fn effective_cap(&self, provider_cap: Option<f64>) -> Option<f64> {
        match (self.max_cost_per_1m, provider_cap) {
            (Some(rung), Some(provider)) => Some(rung.min(provider)),
            (rung, None) => rung,
            (None, provider) => provider,
        }
    }
}
