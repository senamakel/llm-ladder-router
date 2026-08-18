//! What a session pin remembers, and why one gets dropped.

use std::time::Instant;

/// Where a session was last served.
#[derive(Debug, Clone, PartialEq)]
pub struct Pin {
    /// The ladder the session used. A session that switches ladder is asking
    /// for a different capability, so its pin does not carry across.
    pub ladder: String,
    /// The rung's position in that ladder.
    pub rung: usize,
    /// The provider that served.
    pub provider: String,
    /// The model that served.
    pub model: String,
    /// The sub-provider the marketplace routed to, when it named one. This is
    /// the value that actually holds the warm cache.
    pub sub_provider: Option<String>,
    /// The ceiling in force when the pin was taken, in USD per million tokens.
    ///
    /// Kept so a later configuration change can be noticed: a rung whose
    /// ceiling has moved is a different routing decision, and the pin must not
    /// silently outlive the policy that produced it.
    pub cap_per_1m: Option<f64>,
    /// When the pin was last refreshed.
    pub pinned_at: Instant,
}

/// Why a pin was not honored.
///
/// Recorded rather than discarded so a response can say why a session moved,
/// which is otherwise very hard to explain after the fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinRejected {
    /// The request named a different ladder than the pin was taken on.
    DifferentLadder,
    /// The rung's ceiling changed, so the pin predates the current policy.
    CeilingChanged,
    /// The pinned rung can no longer be served — priced out, out of balance,
    /// missing a credential, or without usable price data.
    RungUnavailable,
    /// The ladder no longer has a rung at that position.
    RungGone,
}

impl std::fmt::Display for PinRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DifferentLadder => write!(formatter, "session moved to a different ladder"),
            Self::CeilingChanged => write!(formatter, "the rung's ceiling changed"),
            Self::RungUnavailable => write!(formatter, "the pinned rung can no longer serve"),
            Self::RungGone => write!(formatter, "the pinned rung no longer exists"),
        }
    }
}
