//! The selection engine: which rung of a ladder should serve a request.
//!
//! This module performs no I/O. It takes the configuration, a price snapshot,
//! the known balances, and any session pin, and returns the first rung that can
//! serve along with a reason for every rung it passed over. Keeping it pure is
//! what makes the routing policy testable without a network, and what lets the
//! proxy re-run it after an upstream failure by excluding the rungs already
//! tried.

mod types;

pub use types::{Chosen, Selection, SkipReason, Skipped};

use crate::config::{Config, Ladder, Rung};
use crate::credits::CreditState;
use crate::pricing::PriceTable;
use crate::session::{Pin, PinRejected};

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
    select_pinned(config, ladder, prices, credits, exclude, None)
}

/// As [`select`], but honoring a session's pin when it is still justified.
///
/// A pin only breaks the tie between rungs the budget already allows: the
/// pinned rung is put through exactly the same admissibility checks as any
/// other, and is used only if it passes. It is never a way to exceed a ceiling.
#[must_use]
pub fn select_pinned(
    config: &Config,
    ladder: &Ladder,
    prices: &PriceTable,
    credits: &CreditState,
    exclude: &[usize],
    pin: Option<&Pin>,
) -> Selection {
    let mut skipped = Vec::new();
    let mut pin_rejected = None;

    if let Some(pin) = pin {
        match honor(config, ladder, prices, credits, exclude, pin) {
            Ok(Some(chosen)) => {
                return Selection {
                    skipped,
                    chosen: Some(chosen),
                    pin_rejected: None,
                    pinned: true,
                };
            }
            // The pinned rung is excluded because it already failed this
            // request, so fall through without calling the pin unjustified.
            Ok(None) => {}
            Err(reason) => pin_rejected = Some(reason),
        }
    }

    for (index, rung) in ladder.rungs.iter().enumerate() {
        if exclude.contains(&index) {
            continue;
        }

        match admit(config, ladder, prices, credits, rung, index) {
            Ok(chosen) => {
                return Selection {
                    skipped,
                    chosen: Some(chosen),
                    pin_rejected,
                    pinned: false,
                };
            }
            Err(reason) => skipped.push(Skipped {
                rung: index,
                provider: rung.provider.clone(),
                model: rung.model.clone(),
                reason,
            }),
        }
    }

    Selection {
        skipped,
        chosen: None,
        pin_rejected,
        pinned: false,
    }
}

/// Re-checks a pinned rung, returning the choice if the pin still holds.
///
/// `Ok(None)` means the rung is excluded for this request and the caller should
/// simply walk the ladder; `Err` means the pin itself is no longer justified.
fn honor(
    config: &Config,
    ladder: &Ladder,
    prices: &PriceTable,
    credits: &CreditState,
    exclude: &[usize],
    pin: &Pin,
) -> Result<Option<Chosen>, PinRejected> {
    if pin.ladder != ladder.name {
        return Err(PinRejected::DifferentLadder);
    }
    let Some(rung) = ladder.rungs.get(pin.rung) else {
        return Err(PinRejected::RungGone);
    };
    // The model and provider are part of the identity of the decision; a
    // reordered ladder must not silently repoint a pin at something else.
    if rung.provider != pin.provider || rung.model != pin.model {
        return Err(PinRejected::RungGone);
    }
    if !same_cap(config.cap_for(rung), pin.cap_per_1m) {
        return Err(PinRejected::CeilingChanged);
    }
    if exclude.contains(&pin.rung) {
        return Ok(None);
    }

    let mut chosen = admit(config, ladder, prices, credits, rung, pin.rung)
        .map_err(|_| PinRejected::RungUnavailable)?;

    // Steer back to the sub-provider holding the warm cache, but only if the
    // ceiling still admits it — otherwise the pin would smuggle in a seller the
    // budget has since ruled out. The name the marketplace reported and the
    // slug it steers on differ, so resolve one to the other.
    if let Some(sub_provider) = &pin.sub_provider
        && let Some(model_prices) = prices.get(&rung.provider, &rung.model)
        && let Some(slug) =
            model_prices.steering_slug(sub_provider, chosen.cap_per_1m, ladder.cost_basis)
    {
        chosen.prefer.retain(|prefer| prefer != &slug);
        chosen.prefer.insert(0, slug);
    }

    Ok(Some(chosen))
}

/// Whether two ceilings are the same policy.
fn same_cap(current: Option<f64>, pinned: Option<f64>) -> bool {
    match (current, pinned) {
        (None, None) => true,
        (Some(current), Some(pinned)) => (current - pinned).abs() < f64::EPSILON,
        _ => false,
    }
}

/// Decides whether one rung can serve, and on what terms.
fn admit(
    config: &Config,
    ladder: &Ladder,
    prices: &PriceTable,
    credits: &CreditState,
    rung: &Rung,
    index: usize,
) -> Result<Chosen, SkipReason> {
    // A provider missing from the map is rejected at load time, so this only
    // guards against a caller assembling a `Config` by hand.
    let Some(provider) = config.providers.get(&rung.provider) else {
        return Err(SkipReason::MissingCredential {
            variable: rung.provider.clone(),
        });
    };

    if let Some(reason) = credits.unusable(
        &rung.provider,
        &provider.api_key_env,
        config.credits.min_balance_usd,
    ) {
        return Err(reason);
    }

    let cap = config.cap_for(rung);
    let Some(model_prices) = prices.get(&rung.provider, &rung.model) else {
        // Without a ceiling there is nothing to check prices against, so a
        // missing snapshot is not a reason to refuse to try.
        if cap.is_none() {
            return Ok(Chosen {
                rung: index,
                provider: rung.provider.clone(),
                model: rung.model.clone(),
                cap_per_1m: None,
                admitted: Vec::new(),
                cheapest_per_1m: None,
                min_discount_pct: None,
                prefer: rung.prefer.clone(),
                reasoning_effort: ladder.effort_for(rung),
            });
        }
        return Err(SkipReason::NoPriceData);
    };

    if cap.is_some() && model_prices.is_stale(config.pricing.stale_after) {
        return Err(SkipReason::StalePriceData);
    }

    let admitted = model_prices.admitted(cap, ladder.cost_basis);
    if admitted.is_empty() {
        return Err(SkipReason::NoSellerUnderCap {
            // No ceiling and no usable offer means the marketplace reported
            // every seller as down, which is a failure to try rather than a
            // price decision.
            cap_per_1m: cap.unwrap_or(f64::INFINITY),
            cheapest_per_1m: model_prices.floor(ladder.cost_basis),
        });
    }

    Ok(Chosen {
        rung: index,
        provider: rung.provider.clone(),
        model: rung.model.clone(),
        cap_per_1m: cap,
        admitted: admitted
            .iter()
            .map(|offer| offer.tag.clone().unwrap_or_else(|| offer.provider.clone()))
            .collect(),
        cheapest_per_1m: admitted.first().map(|offer| offer.price(ladder.cost_basis)),
        min_discount_pct: cap.and_then(|cap| model_prices.discount_floor_pct(cap)),
        prefer: rung.prefer.clone(),
        reasoning_effort: ladder.effort_for(rung),
    })
}

#[cfg(test)]
mod test;
