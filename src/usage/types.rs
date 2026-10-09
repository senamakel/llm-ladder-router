//! Configuration for the usage feed, and the figures and records it carries.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The most retries a batch may be given. Backoff doubles per retry, so ten
/// already reaches a delay of over eight minutes at the default base, and a
/// larger number is a typo rather than a policy.
const MAX_RETRIES: u32 = 10;

/// `[usage_sink]` — where per-call cost records are posted, if anywhere.
///
/// ```toml
/// [usage_sink]
/// url = "https://backend.example/internal/memory/model-usage"
/// token_env = "LADDER_USAGE_FEED_TOKEN"   # bearer token, read from the environment
/// flush_interval = "5s"    # post whatever is queued at least this often
/// max_batch = 100          # and as soon as this many records are waiting
/// queue_size = 10000       # records held before new ones are dropped
/// max_retries = 3          # a failed post is retried this many times
/// retry_backoff = "1s"     # first retry delay, doubled per retry
/// timeout = "10s"          # one post may take this long
/// ```
///
/// Absent, nothing is posted. Every served call is still logged with its
/// figures either way; the sink only decides whether they also leave the
/// process.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSink {
    /// The endpoint batches are posted to.
    pub url: String,
    /// The environment variable holding the bearer token sent with each post.
    ///
    /// Named rather than inlined, as every other credential in this file is.
    pub token_env: String,
    /// The longest a record waits in the queue before its batch is posted.
    #[serde(
        default = "UsageSink::default_flush_interval",
        with = "humantime_serde"
    )]
    pub flush_interval: Duration,
    /// A batch is posted as soon as it holds this many records.
    #[serde(default = "UsageSink::default_max_batch")]
    pub max_batch: usize,
    /// How many records may wait for a post. A record arriving at a full
    /// queue is dropped and counted rather than slowing the request that made
    /// it.
    #[serde(default = "UsageSink::default_queue_size")]
    pub queue_size: usize,
    /// How many times a failed post is retried before its batch is dropped.
    #[serde(default = "UsageSink::default_max_retries")]
    pub max_retries: u32,
    /// The delay before the first retry; each later one doubles it.
    #[serde(default = "UsageSink::default_retry_backoff", with = "humantime_serde")]
    pub retry_backoff: Duration,
    /// How long one post may take before it counts as failed.
    #[serde(default = "UsageSink::default_timeout", with = "humantime_serde")]
    pub timeout: Duration,
}

impl UsageSink {
    fn default_flush_interval() -> Duration {
        Duration::from_secs(5)
    }

    fn default_max_batch() -> usize {
        100
    }

    fn default_queue_size() -> usize {
        10_000
    }

    fn default_max_retries() -> u32 {
        3
    }

    fn default_retry_backoff() -> Duration {
        Duration::from_secs(1)
    }

    fn default_timeout() -> Duration {
        Duration::from_secs(10)
    }

    /// Checks the invariants `serde` cannot.
    ///
    /// # Errors
    ///
    /// - [`Error::Empty`] if `url` or `token_env` is blank.
    /// - [`Error::InvalidSetting`] if `url` is not an absolute `http` or
    ///   `https` URL, if a duration is zero, if `max_batch` or `queue_size` is
    ///   zero, or if `max_retries` is above ten.
    pub fn validate(&self) -> Result<()> {
        if self.url.trim().is_empty() {
            return Err(Error::Empty {
                what: "usage_sink.url".to_string(),
            });
        }
        let is_http = reqwest::Url::parse(self.url.trim())
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https"));
        if !is_http {
            return Err(invalid("usage_sink.url", "must be an http or https URL"));
        }
        if self.token_env.trim().is_empty() {
            return Err(Error::Empty {
                what: "usage_sink.token_env".to_string(),
            });
        }
        // A zero interval would spin the flush loop, and a zero timeout fails
        // every post before it is sent.
        if self.flush_interval.is_zero() {
            return Err(invalid("usage_sink.flush_interval", "must be above zero"));
        }
        if self.timeout.is_zero() {
            return Err(invalid("usage_sink.timeout", "must be above zero"));
        }
        if self.max_batch == 0 {
            return Err(invalid("usage_sink.max_batch", "must be at least 1"));
        }
        if self.queue_size == 0 {
            return Err(invalid("usage_sink.queue_size", "must be at least 1"));
        }
        if self.max_retries > MAX_RETRIES {
            return Err(invalid("usage_sink.max_retries", "must be at most 10"));
        }
        Ok(())
    }
}

fn invalid(field: &str, reason: &'static str) -> Error {
    Error::InvalidSetting {
        field: field.to_string(),
        reason,
    }
}

/// What one upstream call cost, as far as its response says.
///
/// Every field is `None` when the response did not carry it. Nothing is
/// estimated: a figure here is one the upstream reported, never one the router
/// worked out from a price list.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Figures {
    /// Input tokens billed.
    pub prompt_tokens: Option<u64>,
    /// Output tokens billed.
    pub completion_tokens: Option<u64>,
    /// What the router's account was actually charged, in USD.
    pub actual_usd: Option<f64>,
    /// The market (list) cost of the same call, in USD.
    pub market_usd: Option<f64>,
}

impl Figures {
    /// These figures with any field `later` reports replacing this one's.
    ///
    /// A stream can report its usage across several events — Anthropic's
    /// sends input tokens at the start and output tokens at the end — so the
    /// last value seen for each field wins, and a field no event repeats is
    /// kept.
    #[must_use]
    pub fn merged(self, later: Self) -> Self {
        Self {
            prompt_tokens: later.prompt_tokens.or(self.prompt_tokens),
            completion_tokens: later.completion_tokens.or(self.completion_tokens),
            actual_usd: later.actual_usd.or(self.actual_usd),
            market_usd: later.market_usd.or(self.market_usd),
        }
    }
}

/// One served upstream call, as posted to the usage sink.
///
/// Carries routing facts and figures only. No part of the prompt or the
/// completion is ever put in a record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Record {
    /// A random v4 UUID, unique per upstream call, so a receiver can ignore a
    /// batch a retry delivers twice.
    pub id: String,
    /// When the call was recorded, RFC 3339 in UTC.
    pub at: String,
    /// The ladder's own name, never the alias a caller used.
    pub ladder: String,
    /// The zero-based rung that served.
    pub rung: usize,
    /// The configured provider name.
    pub provider: String,
    /// The model the rung named.
    pub model: String,
    /// The surface the ladder answers on: `chat`, `embeddings`, `images` or
    /// `video`.
    pub surface: String,
    /// Input tokens billed, or `null` when unknown.
    pub prompt_tokens: Option<u64>,
    /// Output tokens billed, or `null` when unknown.
    pub completion_tokens: Option<u64>,
    /// What the router's account was charged, in USD, or `null` when unknown.
    pub actual_usd: Option<f64>,
    /// The market cost of the same call, in USD, or `null` when unknown.
    pub market_usd: Option<f64>,
}

impl Record {
    /// A record of one served call, stamped with a fresh id and the current
    /// time.
    #[must_use]
    pub fn new(
        ladder: &str,
        rung: usize,
        provider: &str,
        model: &str,
        surface: &str,
        figures: Figures,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            at: humantime::format_rfc3339_millis(std::time::SystemTime::now()).to_string(),
            ladder: ladder.to_string(),
            rung,
            provider: provider.to_string(),
            model: model.to_string(),
            surface: surface.to_string(),
            prompt_tokens: figures.prompt_tokens,
            completion_tokens: figures.completion_tokens,
            actual_usd: figures.actual_usd,
            market_usd: figures.market_usd,
        }
    }
}
