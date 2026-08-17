//! The selection engine: which rung of a ladder should serve a request.
//!
//! This module performs no I/O. It takes the configuration, a price snapshot,
//! and the known balances, and returns the first rung that can serve along with
//! a reason for every rung it passed over. Keeping it pure is what makes the
//! routing policy testable without a network, and what lets the proxy re-run it
//! after an upstream failure by excluding the rungs already tried.

mod types;

pub use types::{Chosen, Selection, SkipReason, Skipped};

use crate::config::{Config, Ladder};
use crate::credits::CreditState;
use crate::pricing::PriceTable;

/// Walks a ladder and picks the first rung that can serve the request.
///
/// `exclude` holds the positions of rungs already tried and failed during this
/// request, so a retry after an upstream failure resumes below them rather than
/// looping on the same rung.
#[must_use]
pub fn select(
    config: &Config,
    ladder: &Ladder,
    prices: &PriceTable,
    credits: &CreditState,
    exclude: &[usize],
) -> Selection {
    let mut skipped = Vec::new();

    for (index, rung) in ladder.rungs.iter().enumerate() {
        if exclude.contains(&index) {
            continue;
        }

        let describe = |reason| Skipped {
            rung: index,
            provider: rung.provider.clone(),
            model: rung.model.clone(),
            reason,
        };

        // A provider missing from the map is rejected at load time, so this
        // only guards against a caller assembling a `Config` by hand.
        let Some(provider) = config.providers.get(&rung.provider) else {
            skipped.push(describe(SkipReason::MissingCredential {
                variable: rung.provider.clone(),
            }));
            continue;
        };

        if let Some(reason) = credits.unusable(
            &rung.provider,
            &provider.api_key_env,
            config.credits.min_balance_usd,
        ) {
            skipped.push(describe(reason));
            continue;
        }

        let cap = config.cap_for(rung);
        let Some(model_prices) = prices.get(&rung.provider, &rung.model) else {
            // Without a ceiling there is nothing to check prices against, so a
            // missing snapshot is not a reason to refuse to try.
            if cap.is_none() {
                return Selection {
                    skipped,
                    chosen: Some(Chosen {
                        rung: index,
                        provider: rung.provider.clone(),
                        model: rung.model.clone(),
                        cap_per_1m: None,
                        admitted: Vec::new(),
                        cheapest_per_1m: None,
                        min_discount_pct: None,
                        prefer: rung.prefer.clone(),
                    }),
                };
            }
            skipped.push(describe(SkipReason::NoPriceData));
            continue;
        };

        if cap.is_some() && model_prices.is_stale(config.pricing.stale_after) {
            skipped.push(describe(SkipReason::StalePriceData));
            continue;
        }

        let admitted = model_prices.admitted(cap, ladder.cost_basis);
        if admitted.is_empty() {
            if let Some(cap_per_1m) = cap {
                skipped.push(describe(SkipReason::NoSellerUnderCap {
                    cap_per_1m,
                    cheapest_per_1m: model_prices.floor(ladder.cost_basis),
                }));
                continue;
            }
            // No ceiling and no usable offer means the marketplace reported
            // every seller as down, which is a failure to try, not a price
            // decision.
            skipped.push(describe(SkipReason::NoSellerUnderCap {
                cap_per_1m: f64::INFINITY,
                cheapest_per_1m: None,
            }));
            continue;
        }

        let cheapest_per_1m = admitted.first().map(|offer| offer.price(ladder.cost_basis));
        let min_discount_pct = cap.and_then(|cap| model_prices.discount_floor_pct(cap));

        return Selection {
            skipped,
            chosen: Some(Chosen {
                rung: index,
                provider: rung.provider.clone(),
                model: rung.model.clone(),
                cap_per_1m: cap,
                admitted: admitted
                    .iter()
                    .filter_map(|offer| offer.tag.clone().or_else(|| Some(offer.provider.clone())))
                    .collect(),
                cheapest_per_1m,
                min_discount_pct,
                prefer: rung.prefer.clone(),
            }),
        };
    }

    Selection {
        skipped,
        chosen: None,
    }
}

#[cfg(test)]
mod test;
