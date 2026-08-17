//! Normalized price data.
//!
//! OpenRouter quotes USD per token and Surplus quotes micro-USD per million
//! tokens. Both are converted to USD per million tokens at the edge so nothing
//! downstream has to remember which marketplace a number came from.

use std::time::Instant;

use crate::config::CostBasis;

/// One sub-provider's offer to serve a model.
#[derive(Debug, Clone, PartialEq)]
pub struct Offer {
    /// The sub-provider's name, as the marketplace reports it.
    pub provider: String,
    /// The slug used to steer traffic to this sub-provider, when the
    /// marketplace supports steering.
    pub tag: Option<String>,
    /// Input price, USD per million tokens.
    pub prompt_per_1m: f64,
    /// Output price, USD per million tokens.
    pub completion_per_1m: f64,
    /// The undiscounted output price this offer is quoted against, USD per
    /// million tokens, when the marketplace publishes one.
    ///
    /// Surplus expresses a ceiling as a discount off this number rather than as
    /// an absolute price, so the ratio is what the router needs.
    pub direct_completion_per_1m: Option<f64>,
    /// Whether the marketplace currently considers this offer usable.
    pub usable: bool,
}

impl Offer {
    /// The price this offer should be judged on, USD per million tokens.
    #[must_use]
    pub fn price(&self, basis: CostBasis) -> f64 {
        match basis {
            CostBasis::Completion => self.completion_per_1m,
            CostBasis::Prompt => self.prompt_per_1m,
            CostBasis::Blended => self.prompt_per_1m.midpoint(self.completion_per_1m),
        }
    }

    /// Whether this offer fits under a ceiling in USD per million tokens.
    ///
    /// An unusable offer never fits, and a rung with no ceiling admits every
    /// usable offer.
    #[must_use]
    pub fn fits(&self, cap: Option<f64>, basis: CostBasis) -> bool {
        self.usable && cap.is_none_or(|cap| self.price(basis) <= cap)
    }
}

/// Every offer for one model on one provider, as of one moment.
#[derive(Debug, Clone)]
pub struct ModelPrices {
    /// The offers, in the order the marketplace returned them.
    pub offers: Vec<Offer>,
    /// When this snapshot was taken, for staleness checks.
    pub fetched_at: Instant,
}

impl ModelPrices {
    /// Builds a snapshot taken now.
    #[must_use]
    pub fn new(offers: Vec<Offer>) -> Self {
        Self {
            offers,
            fetched_at: Instant::now(),
        }
    }

    /// Whether this snapshot is older than the configured tolerance.
    #[must_use]
    pub fn is_stale(&self, stale_after: std::time::Duration) -> bool {
        self.fetched_at.elapsed() > stale_after
    }

    /// The offers that fit under a ceiling, cheapest first.
    #[must_use]
    pub fn admitted(&self, cap: Option<f64>, basis: CostBasis) -> Vec<&Offer> {
        let mut admitted: Vec<&Offer> = self
            .offers
            .iter()
            .filter(|offer| offer.fits(cap, basis))
            .collect();
        admitted.sort_by(|left, right| {
            left.price(basis)
                .partial_cmp(&right.price(basis))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        admitted
    }

    /// The cheapest usable offer, whatever it costs.
    ///
    /// Used to explain a rung that was skipped: knowing the floor was $0.63
    /// when the ceiling was $0.30 is what makes the decision reviewable.
    #[must_use]
    pub fn floor(&self, basis: CostBasis) -> Option<f64> {
        self.offers
            .iter()
            .filter(|offer| offer.usable)
            .map(|offer| offer.price(basis))
            .min_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// The smallest discount off the direct price that still admits an offer
    /// under `cap`, as a whole percentage.
    ///
    /// Surplus filters by discount rather than by absolute price, so a dollar
    /// ceiling has to be restated in those terms. Returns `None` when no offer
    /// publishes a direct price to discount against.
    #[must_use]
    pub fn discount_floor_pct(&self, cap: f64, basis: CostBasis) -> Option<u8> {
        let direct = self
            .offers
            .iter()
            .filter_map(|offer| offer.direct_completion_per_1m)
            .find(|direct| *direct > 0.0)?;
        let _ = basis;
        let ratio = (cap / direct).clamp(0.0, 1.0);
        // Round down, so the filter never asks for more discount than the
        // ceiling actually implies and never skips an offer that fits.
        let pct = ((1.0 - ratio) * 100.0).floor();
        // Surplus rejects everything at 100, which would make an affordable
        // rung unreachable, so 99 is the tightest usable filter.
        Some(pct.clamp(0.0, 99.0) as u8)
    }
}
