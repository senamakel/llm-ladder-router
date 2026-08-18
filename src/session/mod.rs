//! Sticky routing for a conversation.
//!
//! Marketplaces bill cached prompt tokens at a fraction of the normal rate, but
//! a cache is warm only on the sub-provider that already saw the prefix. A
//! long thread that hops between rungs — or between sub-providers within a rung
//! — pays full price for its whole history on every hop, which for a growing
//! conversation quickly costs more than the cheaper rung ever saved.
//!
//! So once a session has been served, it is pinned to the rung and sub-provider
//! that served it, and stays there while that choice is still valid. The pin is
//! dropped as soon as it stops being justified: a changed ceiling, or a rung the
//! current market can no longer satisfy. A pin never overrides the budget — it
//! only breaks the tie between rungs the budget already allows.

mod types;

pub use types::{Pin, PinRejected};

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Every live session pin, oldest evicted first.
#[derive(Debug)]
pub struct SessionPins {
    entries: HashMap<String, Pin>,
    ttl: Duration,
    max_entries: usize,
}

impl SessionPins {
    /// A store holding pins for `ttl`, up to `max_entries` of them.
    #[must_use]
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
            max_entries,
        }
    }

    /// The pin for a session, if it has one that has not expired.
    #[must_use]
    pub fn get(&self, session: &str) -> Option<&Pin> {
        self.entries
            .get(session)
            .filter(|pin| pin.pinned_at.elapsed() <= self.ttl)
    }

    /// Records where a session was served, replacing any previous pin.
    ///
    /// Refreshing `pinned_at` on every request is deliberate: the TTL is meant
    /// to expire *idle* sessions, and an active thread is exactly the one whose
    /// cache is worth keeping warm.
    pub fn pin(&mut self, session: impl Into<String>, pin: Pin) {
        self.evict();
        self.entries.insert(session.into(), pin);
    }

    /// Forgets a session's pin.
    pub fn unpin(&mut self, session: &str) {
        self.entries.remove(session);
    }

    /// How many pins are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no session is pinned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops expired pins, then the oldest ones if still over the limit.
    ///
    /// Without a bound this map would grow for the life of the process, since
    /// sessions are named by callers and never explicitly closed.
    fn evict(&mut self) {
        let ttl = self.ttl;
        self.entries.retain(|_, pin| pin.pinned_at.elapsed() <= ttl);

        while self.entries.len() >= self.max_entries {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, pin)| pin.pinned_at)
                .map(|(session, _)| session.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod test;
