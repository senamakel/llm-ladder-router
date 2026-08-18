//! Remembering which rungs are rate-limited, and for how long.
//!
//! A 429 already advances the ladder: the request that met it is served by the
//! next rung, and nothing is lost but one round trip. The waste is in the
//! *next* request, which knows nothing and asks the same rung again — so a
//! provider that rate-limits for a minute costs one wasted round trip per
//! request for that whole minute, and the busier the run, the more of them.
//!
//! So a rung that answers 429 is put on a cooldown and skipped while it lasts,
//! exactly as if it were priced out. Two rules make that safe:
//!
//! - **The upstream's own number wins.** A `Retry-After` is what the provider
//!   said it needed; the configured default is only for when it said nothing.
//!   It is clamped, because a header asking for an hour would otherwise take a
//!   rung out of a ladder for an hour on one bad minute.
//! - **A cooldown is per rung, not per provider.** One model being throttled
//!   says nothing about another on the same marketplace, and taking the whole
//!   provider out would empty a ladder over one busy model.

mod types;

pub use types::Cooled;

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Every rung currently cooling down after a rate limit.
#[derive(Debug, Default)]
pub struct Cooldowns {
    entries: HashMap<(String, String), Instant>,
}

impl Cooldowns {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts one rung on cooldown for `duration`, extending any existing one.
    ///
    /// Extending rather than replacing: two concurrent requests can both meet
    /// the same rate limit, and the later answer is not evidence that the
    /// earlier one was wrong.
    pub fn cool(&mut self, provider: &str, model: &str, duration: Duration) {
        self.evict();
        let until = Instant::now() + duration;
        self.entries
            .entry((provider.to_string(), model.to_string()))
            .and_modify(|existing| {
                if until > *existing {
                    *existing = until;
                }
            })
            .or_insert(until);
    }

    /// How long one rung is still cooling down, if it is.
    #[must_use]
    pub fn remaining(&self, provider: &str, model: &str) -> Option<Duration> {
        let until = self
            .entries
            .get(&(provider.to_string(), model.to_string()))?;
        until.checked_duration_since(Instant::now())
    }

    /// How many rungs are held, expired ones included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is cooling down.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops entries whose cooldown has passed.
    ///
    /// Called on write rather than on a timer: the map is keyed by rung, so it
    /// is bounded by the configuration and cannot grow with traffic. This keeps
    /// it from holding a stale entry for a rung nobody asks about any more.
    fn evict(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, until| *until > now);
    }
}

#[cfg(test)]
mod test;
