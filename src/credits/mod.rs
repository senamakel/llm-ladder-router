//! Remaining account balance per provider.
//!
//! Both marketplaces bill against a prepaid balance, so a provider can be
//! perfectly healthy and still unable to serve. Polling the balance lets the
//! router skip such a provider before spending a round trip on it, and gives a
//! ladder exhaustion an honest explanation.

use std::collections::BTreeMap;

use crate::ladder::SkipReason;

/// What is known about one provider's ability to spend.
#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    /// Spendable balance in USD.
    ///
    /// Where a marketplace reports both a balance and a spending allowance,
    /// this is the smaller: an allowance below the balance is the real limit.
    pub remaining_usd: f64,
}

/// Every provider's last known balance, plus which credentials are present.
///
/// A provider absent from the balance map has simply not been polled yet, and
/// is treated as usable — refusing to route until the first poll lands would
/// make startup needlessly fragile.
#[derive(Debug, Default, Clone)]
pub struct CreditState {
    balances: BTreeMap<String, Balance>,
    missing_credentials: BTreeMap<String, String>,
}

impl CreditState {
    /// An empty state, as held before the first poll completes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a provider's balance.
    pub fn set_balance(&mut self, provider: impl Into<String>, remaining_usd: f64) {
        self.balances
            .insert(provider.into(), Balance { remaining_usd });
    }

    /// Records that a provider's credential is absent from the environment.
    pub fn set_missing_credential(
        &mut self,
        provider: impl Into<String>,
        variable: impl Into<String>,
    ) {
        self.missing_credentials
            .insert(provider.into(), variable.into());
    }

    /// The last known balance for a provider.
    #[must_use]
    pub fn balance(&self, provider: &str) -> Option<&Balance> {
        self.balances.get(provider)
    }

    /// Why this provider cannot be used, if it cannot.
    ///
    /// `variable` is the credential the provider is configured to read, and is
    /// reported when the credential is the thing that is missing.
    #[must_use]
    pub fn unusable(
        &self,
        provider: &str,
        variable: &str,
        min_balance_usd: f64,
    ) -> Option<SkipReason> {
        if self.missing_credentials.contains_key(provider) {
            return Some(SkipReason::MissingCredential {
                variable: variable.to_string(),
            });
        }

        let balance = self.balances.get(provider)?;
        if balance.remaining_usd < min_balance_usd {
            return Some(SkipReason::ExhaustedBalance {
                remaining_usd: balance.remaining_usd,
                floor_usd: min_balance_usd,
            });
        }

        None
    }
}

#[cfg(test)]
mod test;
