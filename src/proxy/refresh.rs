//! Background refreshers for price data and account balances.
//!
//! Both are fail-soft. A refresh that fails logs and leaves the previous
//! snapshot in place, because routing on slightly stale prices is far better
//! than refusing to route at all. Staleness is bounded separately, by
//! `pricing.stale_after` in the selection engine.

use std::collections::BTreeSet;

use super::types::State;

/// Starts the periodic refresh loops and returns immediately.
///
/// Each loop sleeps before its first pass: [`super::serve`] performs the
/// initial refresh itself, before it binds, so that a request can never arrive
/// while the price table is still empty.
pub(super) fn spawn(state: State) {
    let prices = state.clone();
    tokio::spawn(async move {
        let period = prices.config.pricing.refresh;
        loop {
            tokio::time::sleep(period).await;
            refresh_prices_once(&prices).await;
        }
    });

    tokio::spawn(async move {
        let period = state.config.credits.refresh;
        loop {
            tokio::time::sleep(period).await;
            refresh_credits_once(&state).await;
        }
    });
}

/// Re-reads every model named by a rung and replaces the price table.
///
/// The table is rebuilt from what succeeded and then swapped in, so a partial
/// failure never leaves the router reading a half-updated table.
pub async fn refresh_prices_once(state: &State) {
    let mut wanted: BTreeSet<(String, String)> = BTreeSet::new();
    for ladder in &state.config.ladders {
        for rung in &ladder.rungs {
            wanted.insert((rung.provider.clone(), rung.model.clone()));
        }
    }

    let mut table = crate::pricing::PriceTable::new();
    let mut failures = 0_usize;

    for (provider, model) in wanted {
        let Some(client) = state.clients.get(&provider) else {
            continue;
        };
        match client.fetch_prices(&model).await {
            Ok(prices) => {
                tracing::debug!(
                    provider = %provider,
                    model = %model,
                    offers = prices.offers.len(),
                    "prices refreshed"
                );
                table.insert(provider, model, prices);
            }
            Err(error) => {
                failures += 1;
                tracing::warn!(
                    provider = %provider,
                    model = %model,
                    error = %error,
                    "price refresh failed, keeping the previous snapshot"
                );
            }
        }
    }

    if table.is_empty() && failures > 0 {
        // Everything failed; keeping what we had beats routing with nothing.
        return;
    }

    // Carry forward any model whose refresh failed, so one bad response does
    // not silently strip a rung of the price data it needs.
    {
        let previous = state.prices.read().await;
        for ladder in &state.config.ladders {
            for rung in &ladder.rungs {
                if table.get(&rung.provider, &rung.model).is_none()
                    && let Some(stale) = previous.get(&rung.provider, &rung.model)
                {
                    table.insert(rung.provider.clone(), rung.model.clone(), stale.clone());
                }
            }
        }
    }

    *state.prices.write().await = table;
}

/// Re-reads every provider's balance and replaces the credit state.
pub async fn refresh_credits_once(state: &State) {
    let mut credits = crate::credits::CreditState::new();

    for (name, client) in state.clients.iter() {
        if !client.has_credential() {
            credits.set_missing_credential(name.clone(), client.credential_variable());
            tracing::warn!(
                provider = %name,
                variable = %client.credential_variable(),
                "credential is unset, provider will be skipped"
            );
            continue;
        }

        match client.fetch_balance().await {
            Ok(remaining_usd) => {
                tracing::debug!(provider = %name, remaining_usd, "balance refreshed");
                credits.set_balance(name.clone(), remaining_usd);
            }
            Err(error) => {
                // An unreadable balance must not take the provider out of
                // service; leaving it unrecorded keeps it usable.
                tracing::warn!(
                    provider = %name,
                    error = %error,
                    "balance refresh failed, provider stays usable"
                );
            }
        }
    }

    *state.credits.write().await = credits;
}
