//! What a rate limit produced.

use std::time::Duration;

/// A decision to take one rung out of service for a while.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cooled {
    /// How long the rung is out for.
    pub duration: Duration,
    /// Whether the upstream asked for this length itself, as against the
    /// configured default. Worth distinguishing in a log: a provider that
    /// states its own backoff is one worth believing next time.
    pub requested: bool,
}
