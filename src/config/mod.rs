//! Loading and validating `config.toml`.
//!
//! Parsing is `serde`'s job; this module owns the checks `serde` cannot express:
//! that every rung names a provider that exists, that every price ceiling is a
//! usable positive number, and that ladder names are unique so a request can
//! select between them.

mod types;

pub use types::{
    Config, CostBasis, Credits, Ladder, Pricing, Provider, ProviderKind, Rung, Server, Sessions,
};

use crate::error::{Error, Result};

impl Config {
    /// Reads and validates a configuration file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ConfigRead`] if the file cannot be read and
    /// [`Error::ConfigParse`] if it is not valid TOML. Validation adds
    /// [`Error::Empty`] for a configuration with no ladders or a ladder with no
    /// rungs, [`Error::DuplicateLadder`] for two ladders sharing a name,
    /// [`Error::UnknownProvider`] for a rung naming an undefined provider, and
    /// [`Error::InvalidPrice`] for a ceiling that is not positive and finite.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Parses and validates a configuration from TOML text.
    ///
    /// # Errors
    ///
    /// As [`Config::load`], minus the read failure.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Checks the invariants `serde` cannot.
    ///
    /// # Errors
    ///
    /// - [`Error::Empty`] if there are no ladders, or a ladder has no rungs.
    /// - [`Error::DuplicateLadder`] if two ladders share a name.
    /// - [`Error::UnknownProvider`] if a rung names an undefined provider.
    /// - [`Error::InvalidPrice`] if a ceiling or a score multiplier is not
    ///   positive and finite.
    /// - [`Error::Empty`] if a declared `reasoning_effort` is blank, which would
    ///   otherwise reach an upstream as an empty string and be rejected there.
    fn validate(&self) -> Result<()> {
        if self.ladders.is_empty() {
            return Err(Error::Empty {
                what: "ladders".to_string(),
            });
        }

        for (name, provider) in &self.providers {
            check_price(
                provider.max_cost_per_1m,
                &format!("providers.{name}.max_cost_per_1m"),
            )?;
        }

        let mut seen = std::collections::BTreeSet::new();
        for ladder in &self.ladders {
            if !seen.insert(ladder.name.as_str()) {
                return Err(Error::DuplicateLadder(ladder.name.clone()));
            }
            if ladder.rungs.is_empty() {
                return Err(Error::Empty {
                    what: format!("ladder {} rungs", ladder.name),
                });
            }
            check_effort(
                ladder.reasoning_effort.as_deref(),
                &format!("ladder {} reasoning_effort", ladder.name),
            )?;
            for (index, rung) in ladder.rungs.iter().enumerate() {
                if !self.providers.contains_key(&rung.provider) {
                    return Err(Error::UnknownProvider {
                        ladder: ladder.name.clone(),
                        rung: index,
                        provider: rung.provider.clone(),
                    });
                }
                check_price(
                    rung.max_cost_per_1m,
                    &format!("ladder {} rung {index} max_cost_per_1m", ladder.name),
                )?;
                check_effort(
                    rung.reasoning_effort.as_deref(),
                    &format!("ladder {} rung {index} reasoning_effort", ladder.name),
                )?;
                // A multiplier divides a price, so zero, a negative, and `NaN`
                // are all the same mistake: a rung that would rank ahead of
                // every honest one and take every request.
                check_price(
                    rung.score_multiplier,
                    &format!("ladder {} rung {index} score_multiplier", ladder.name),
                )?;
            }
        }

        Ok(())
    }

    /// Looks up a ladder by the name a request used.
    #[must_use]
    pub fn ladder(&self, name: &str) -> Option<&Ladder> {
        self.ladders.iter().find(|ladder| ladder.name == name)
    }

    /// The ceiling that applies to a rung once its provider's ceiling is folded
    /// in, in USD per million tokens.
    ///
    /// Returns `None` when neither the rung nor its provider sets one.
    #[must_use]
    pub fn cap_for(&self, rung: &Rung) -> Option<f64> {
        rung.effective_cap(
            self.providers
                .get(&rung.provider)
                .and_then(|provider| provider.max_cost_per_1m),
        )
    }
}

/// Rejects a ceiling that is not a usable amount of money.
///
/// Zero and negative ceilings would silently admit nothing, and `NaN` compares
/// false against every price, so both are configuration mistakes rather than
/// meaningful policies.
fn check_price(price: Option<f64>, field: &str) -> Result<()> {
    match price {
        Some(value) if !value.is_finite() || value <= 0.0 => Err(Error::InvalidPrice {
            field: field.to_string(),
        }),
        _ => Ok(()),
    }
}

/// Rejects a declared reasoning effort that is blank.
///
/// A blank value is not "unset": it would be injected into the request body as
/// an empty string, and an upstream rejects that with a 400 the failover loop
/// attributes to the caller — so the ladder would stop rather than step down.
/// Which values are *meaningful* is the model's business and cannot be checked
/// here; that a value was declared at all is this file's.
fn check_effort(effort: Option<&str>, field: &str) -> Result<()> {
    match effort {
        Some(value) if value.trim().is_empty() => Err(Error::Empty {
            what: field.to_string(),
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod test;
