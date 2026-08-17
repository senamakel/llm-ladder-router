//! The price table the selection engine reads.
//!
//! Marketplace payloads are parsed into [`Offer`]s by each provider module;
//! this module only stores the result and answers questions about it. Keeping
//! the store free of I/O is what lets the selection engine be tested without a
//! network.

mod types;

pub use types::{ModelPrices, Offer};

use std::collections::BTreeMap;

/// A snapshot of every model's offers, keyed by provider name and model slug.
#[derive(Debug, Default, Clone)]
pub struct PriceTable {
    entries: BTreeMap<(String, String), ModelPrices>,
}

impl PriceTable {
    /// An empty table, as held before the first refresh completes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces one model's offers.
    pub fn insert(
        &mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
        prices: ModelPrices,
    ) {
        self.entries.insert((provider.into(), model.into()), prices);
    }

    /// The offers held for one model, if any refresh has succeeded for it.
    #[must_use]
    pub fn get(&self, provider: &str, model: &str) -> Option<&ModelPrices> {
        // The tuple key means a lookup would otherwise have to allocate two
        // owned strings; searching the map directly avoids that on the hot
        // path, which runs once per rung per request.
        self.entries
            .iter()
            .find(|((entry_provider, entry_model), _)| {
                entry_provider == provider && entry_model == model
            })
            .map(|(_, prices)| prices)
    }

    /// How many models the table holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no refresh has landed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod test;
