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
use std::time::Duration;

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

/// A session for a caller that names none, derived from its stable prefix.
///
/// Most clients send no conversation id, and for them a marketplace's prompt
/// cache is a matter of luck: measured 2026-10-12 on `deepseek-v4.1-flash`, a
/// six-turn conversation with a 10k-token system prompt hit the cache on half
/// its warm turns or fewer without a key, and on every one with a key. What a
/// keyless conversation does carry is the part a cache keeps warm — its system
/// prompt — so that, with the ladder it asked for, names it. `OpenRouter` does
/// the same internally, but only sticks to a seller after a hit it happened to
/// get; a key from the first request is what makes the first hit likely.
///
/// Conversations that share a system prompt share the key, which is the point:
/// they share the cached prefix too. A body with no system prompt (chat
/// `system`/`developer` message, Anthropic `system`, or responses
/// `instructions`) gets no session.
#[must_use]
pub fn prefix_session(body: &serde_json::Value) -> Option<String> {
    let system = body
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|messages| {
            messages.iter().find(|message| {
                matches!(
                    message.get("role").and_then(serde_json::Value::as_str),
                    Some("system" | "developer")
                )
            })
        })
        .and_then(|message| message.get("content"))
        .or_else(|| body.get("system"))
        .or_else(|| body.get("instructions"))
        .filter(|system| !system.is_null())?;
    let model = body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let mut hash = Fnv1a::default();
    hash.write(model.as_bytes());
    hash.write(&[0]);
    hash.write(system.to_string().as_bytes());
    Some(format!("prefix-{:016x}", hash.0))
}

/// 64-bit FNV-1a: stable across processes and toolchains, unlike the standard
/// library's hasher, so a restarted router keeps naming a prefix the same way.
struct Fnv1a(u64);

impl Default for Fnv1a {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv1a {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod test;
