//! Posting usage records to `[usage_sink]` in batches.
//!
//! Best-effort by design. The feed must never slow or fail a request, so a
//! record is queued without waiting, a full queue drops the new record and
//! counts it, and a batch that cannot be delivered after its retries is
//! dropped and logged. Losing a record loses an accounting line; blocking on
//! one would lose the request.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc;

use super::types::{Record, UsageSink};
use crate::error::{Error, Result};

/// The sending half of the usage feed, shared by every request.
///
/// Cloning is cheap and every clone feeds the same queue. Records queued
/// before [`Feed::start`] wait in the queue and go out with the first batch.
#[derive(Clone)]
pub struct Feed {
    sender: mpsc::Sender<Record>,
    dropped: Arc<AtomicU64>,
    pump: Arc<Mutex<Option<Pump>>>,
}

impl std::fmt::Debug for Feed {
    // By hand, so the bearer token the pump holds can never reach a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feed")
            .field(
                "queued",
                &(self.sender.max_capacity() - self.sender.capacity()),
            )
            .field("dropped", &self.dropped.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// The receiving half: drains the queue and posts batches.
struct Pump {
    receiver: mpsc::Receiver<Record>,
    sink: UsageSink,
    token: String,
    http: reqwest::Client,
    dropped: Arc<AtomicU64>,
}

/// The body posted to the sink.
#[derive(Serialize)]
struct Batch<'a> {
    records: &'a [Record],
}

impl Feed {
    /// A feed for a validated sink, its token read from the environment
    /// variable the sink names.
    ///
    /// Returns `None`, with a warning, when that variable is unset or blank:
    /// every post would be refused, so the feed is left off and served calls
    /// are still logged.
    ///
    /// # Errors
    ///
    /// As [`Feed::new`].
    pub fn from_env(sink: &UsageSink) -> Result<Option<Self>> {
        let token = std::env::var(&sink.token_env)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let Some(token) = token else {
            tracing::warn!(
                variable = %sink.token_env,
                "usage sink token is unset; usage records will be logged but not posted"
            );
            return Ok(None);
        };
        Self::new(sink, token).map(Some)
    }

    /// A feed for a validated sink, using a token the caller already holds.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Upstream`] if the HTTP client for the sink cannot be
    /// built.
    pub fn new(sink: &UsageSink, token: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(sink.timeout)
            .build()
            .map_err(|source| Error::Upstream {
                provider: "usage_sink".to_string(),
                source,
            })?;
        let (sender, receiver) = mpsc::channel(sink.queue_size.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let pump = Pump {
            receiver,
            sink: sink.clone(),
            token: token.into(),
            http,
            dropped: dropped.clone(),
        };
        Ok(Self {
            sender,
            dropped,
            pump: Arc::new(Mutex::new(Some(pump))),
        })
    }

    /// Starts posting in the background. Only the first call on any clone
    /// does anything.
    ///
    /// Must be called from inside a Tokio runtime.
    pub fn start(&self) {
        let pump = self.pump.lock().ok().and_then(|mut slot| slot.take());
        if let Some(pump) = pump {
            tokio::spawn(pump.run());
        }
    }

    /// Queues a record without waiting.
    ///
    /// A full queue drops the record and counts it; the count is logged with
    /// the next flush.
    pub fn push(&self, record: Record) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.sender.try_send(record) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Pump {
    async fn run(mut self) {
        let mut batch: Vec<Record> = Vec::with_capacity(self.sink.max_batch);
        let mut tick = tokio::time::interval(self.sink.flush_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick completes at once; the first flush is one interval
        // from now.
        tick.tick().await;
        loop {
            tokio::select! {
                received = self.receiver.recv() => {
                    let Some(record) = received else {
                        self.flush(&mut batch).await;
                        return;
                    };
                    batch.push(record);
                    if batch.len() >= self.sink.max_batch {
                        self.flush(&mut batch).await;
                    }
                }
                _ = tick.tick() => {
                    let dropped = self.dropped.swap(0, Ordering::Relaxed);
                    if dropped > 0 {
                        tracing::warn!(dropped, "usage queue full, records dropped");
                    }
                    self.flush(&mut batch).await;
                }
            }
        }
    }

    /// Posts what is waiting and empties the batch, delivered or not.
    async fn flush(&self, batch: &mut Vec<Record>) {
        if batch.is_empty() {
            return;
        }
        self.deliver(batch).await;
        batch.clear();
    }

    /// Posts one batch, retrying with doubling backoff on a failure the sink
    /// might recover from.
    ///
    /// A transport error, a 5xx, a 408 or a 429 is retried. Any other refusal
    /// is not: the same records with the same token would be refused again.
    async fn deliver(&self, records: &[Record]) {
        let mut delay = self.sink.retry_backoff;
        let mut attempt = 0_u32;
        loop {
            attempt += 1;
            let outcome = self
                .http
                .post(&self.sink.url)
                .bearer_auth(&self.token)
                .json(&Batch { records })
                .send()
                .await;
            let (detail, retryable) = match outcome {
                Ok(response) if response.status().is_success() => {
                    tracing::debug!(records = records.len(), "usage batch delivered");
                    return;
                }
                Ok(response) => {
                    let status = response.status();
                    let retryable = status.is_server_error()
                        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || status == reqwest::StatusCode::REQUEST_TIMEOUT;
                    (status.to_string(), retryable)
                }
                Err(error) => (error.to_string(), true),
            };
            if !retryable || attempt > self.sink.max_retries {
                tracing::warn!(
                    records = records.len(),
                    attempts = attempt,
                    detail = %detail,
                    "usage batch dropped"
                );
                return;
            }
            tokio::time::sleep(delay).await;
            delay = delay.saturating_mul(2);
        }
    }
}

#[cfg(test)]
#[path = "sink_test.rs"]
mod test;
