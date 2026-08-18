//! What the selection engine decides, and why.

/// Why a rung was passed over.
///
/// Every skip is recorded rather than discarded: a request that exhausts its
/// ladder has to be able to say what happened at each step, and "the ceiling
/// was $0.30 and the cheapest seller was $0.63" is the only useful form of that
/// answer.
#[derive(Debug, Clone, PartialEq)]
pub enum SkipReason {
    /// The provider's credential was not set in the environment.
    MissingCredential {
        /// The environment variable that should have carried it.
        variable: String,
    },
    /// The provider's spendable balance was below the configured floor.
    ExhaustedBalance {
        /// What remained, in USD.
        remaining_usd: f64,
        /// The floor it fell below, in USD.
        floor_usd: f64,
    },
    /// No price refresh has succeeded for this model yet.
    NoPriceData,
    /// The price snapshot was older than the configured tolerance.
    StalePriceData,
    /// Every seller was above the ceiling.
    NoSellerUnderCap {
        /// The ceiling that applied, in USD per million tokens.
        cap_per_1m: f64,
        /// The cheapest usable seller, in USD per million tokens, when there
        /// was one at all.
        cheapest_per_1m: Option<f64>,
    },
    /// The rung was tried and the upstream failed in a way that warrants
    /// falling through.
    UpstreamFailed {
        /// What the upstream said.
        detail: String,
    },
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCredential { variable } => {
                write!(formatter, "credential {variable} is unset")
            }
            Self::ExhaustedBalance {
                remaining_usd,
                floor_usd,
            } => write!(
                formatter,
                "balance ${remaining_usd:.2} is below the ${floor_usd:.2} floor"
            ),
            Self::NoPriceData => write!(formatter, "no price data"),
            Self::StalePriceData => write!(formatter, "price data is stale"),
            Self::NoSellerUnderCap {
                cap_per_1m,
                cheapest_per_1m,
            } => match cheapest_per_1m {
                Some(cheapest) => write!(
                    formatter,
                    "no seller under ${cap_per_1m}/Mtok; cheapest is ${cheapest}/Mtok"
                ),
                None => write!(formatter, "no usable seller under ${cap_per_1m}/Mtok"),
            },
            Self::UpstreamFailed { detail } => write!(formatter, "upstream failed: {detail}"),
        }
    }
}

/// A rung that was passed over, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Skipped {
    /// The rung's position in its ladder.
    pub rung: usize,
    /// The provider the rung named.
    pub provider: String,
    /// The model the rung named.
    pub model: String,
    /// Why it was passed over.
    pub reason: SkipReason,
}

/// A rung the engine is willing to try.
#[derive(Debug, Clone, PartialEq)]
pub struct Chosen {
    /// The rung's position in its ladder.
    pub rung: usize,
    /// The provider to dispatch to.
    pub provider: String,
    /// The model slug to ask for.
    pub model: String,
    /// The ceiling that applies, in USD per million tokens, once the provider's
    /// ceiling has been folded into the rung's.
    pub cap_per_1m: Option<f64>,
    /// The sub-providers that fit under the ceiling, cheapest first.
    pub admitted: Vec<String>,
    /// The cheapest admitted price, in USD per million tokens.
    pub cheapest_per_1m: Option<f64>,
    /// The minimum discount that expresses the ceiling, for marketplaces that
    /// filter by discount rather than by absolute price.
    pub min_discount_pct: Option<u8>,
    /// Sub-providers the rung asked to prefer.
    pub prefer: Vec<String>,
    /// How hard this rung should be asked to think, when the ladder or the rung
    /// declared it and the caller did not.
    pub reasoning_effort: Option<String>,
}

/// The outcome of walking a ladder.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// The rungs that were passed over, in the order they were considered.
    pub skipped: Vec<Skipped>,
    /// The rung to try, or `None` when every rung was passed over.
    pub chosen: Option<Chosen>,
    /// Why the session's pin was not honored, when it had one and it was not.
    pub pin_rejected: Option<crate::session::PinRejected>,
    /// Whether the chosen rung came from the session's pin rather than from
    /// walking the ladder.
    pub pinned: bool,
}
