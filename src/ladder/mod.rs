//! The selection engine: which rung of a ladder should serve a request.
//!
//! This module performs no I/O. It takes the configuration, a price snapshot,
//! the known balances, the rungs currently cooling down after a rate limit, and
//! any session pin, and returns the *best* rung that
//! can serve along with a reason for every rung that could not. Keeping it pure
//! is what makes the routing policy testable without a network, and what lets
//! the proxy re-run it after an upstream failure by excluding the rungs already
//! tried.
//!
//! Best means cheapest per unit of quality: each rung's cheapest admitted
//! seller is divided by its `score_multiplier` and the lowest result wins. A
//! ladder is therefore a *set* of priced alternatives rather than a queue —
//! rung order survives only as the tie-break and as documentation. The rungs
//! still bound what may be paid, one ceiling each, so scoring chooses among
//! affordable rungs and never widens what "affordable" means.

mod types;

pub use types::{Chosen, Selection, SkipReason, Skipped};

use crate::config::{Config, Ladder, Rung};
use crate::cooldown::Cooldowns;
use crate::credits::CreditState;
use crate::pricing::PriceTable;
use crate::session::{Pin, PinRejected};

/// Ranks a ladder's rungs and picks the best one that can serve the request.
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
    select_pinned(
        config,
        ladder,
        prices,
        credits,
        &Cooldowns::new(),
        exclude,
        None,
    )
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
    cooldowns: &Cooldowns,
    exclude: &[usize],
    pin: Option<&Pin>,
) -> Selection {
    let mut skipped = Vec::new();
    let mut pin_rejected = None;

    if let Some(pin) = pin {
        match honor(config, ladder, prices, credits, cooldowns, exclude, pin) {
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

    // Every rung is considered, not just those before the first that fits: the
    // engine ranks what can serve rather than stopping at it. `skipped` is
    // therefore every rung that *could not* serve, and a rung absent from both
    // it and `chosen` is one that could have served and was outbid.
    let mut candidates: Vec<Chosen> = Vec::new();
    for (index, rung) in ladder.rungs.iter().enumerate() {
        if exclude.contains(&index) {
            continue;
        }

        match admit(config, ladder, prices, credits, cooldowns, rung, index) {
            Ok(chosen) => candidates.push(chosen),
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
        chosen: candidates.into_iter().min_by(rank),
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
    cooldowns: &Cooldowns,
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

    let mut chosen = admit(config, ladder, prices, credits, cooldowns, rung, pin.rung)
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
    cooldowns: &Cooldowns,
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

    // Checked before the prices, because a rung the upstream has just refused
    // is not available at any price. This is what turns a 429 into one wasted
    // round trip rather than one per request until the limit lifts.
    if let Some(remaining) = cooldowns.remaining(&rung.provider, &rung.model) {
        return Err(SkipReason::RateLimited {
            // Rounded up, so a sub-second remainder never reads as "retry now".
            retry_in_secs: remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0),
        });
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
                score_multiplier: rung.effective_score_multiplier(),
                // Nothing to divide: an unpriced rung cannot be ranked against
                // a priced one, so it waits behind every rung that can be.
                score: None,
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

    let cheapest_per_1m = admitted.first().map(|offer| offer.price(ladder.cost_basis));
    let multiplier = rung.effective_score_multiplier();
    Ok(Chosen {
        rung: index,
        provider: rung.provider.clone(),
        model: rung.model.clone(),
        cap_per_1m: cap,
        admitted: admitted
            .iter()
            .map(|offer| offer.tag.clone().unwrap_or_else(|| offer.provider.clone()))
            .collect(),
        cheapest_per_1m,
        min_discount_pct: cap.and_then(|cap| model_prices.discount_floor_pct(cap)),
        prefer: rung.prefer.clone(),
        reasoning_effort: ladder.effort_for(rung),
        score_multiplier: multiplier,
        score: cheapest_per_1m.map(|price| price / multiplier),
    })
}

/// Ranks two admissible rungs against each other, best first.
///
/// Lowest score wins. A rung with no score is not cheap — it is unpriced, so it
/// ranks behind every rung that could be measured. Ties fall back to ladder
/// order, which is the only thing left that a reader can predict.
fn rank(left: &Chosen, right: &Chosen) -> std::cmp::Ordering {
    match (left.score, right.score) {
        (Some(left_score), Some(right_score)) => left_score
            .partial_cmp(&right_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.rung.cmp(&right.rung)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left.rung.cmp(&right.rung),
    }
}

#[cfg(test)]
mod test;
