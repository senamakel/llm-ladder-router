//! Loading and validating `config.toml`.
//!
//! Parsing is `serde`'s job; this module owns the checks `serde` cannot express:
//! that every rung names a provider that exists, that every price ceiling is a
//! usable positive number, and that ladder names are unique so a request can
//! select between them.

mod types;

pub use types::{
    Config, CostBasis, Credits, Ladder, Pricing, Provider, ProviderKind, RateLimits, Rung, Server,
    Sessions, Surface,
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
    /// - [`Error::UnpriceableCeiling`] if a ceiling is set on a direct provider
    ///   or one of its rungs, where no order book exists to check it against.
    /// - [`Error::UncappableSurface`] if a ceiling is set on a rung of an
    ///   embeddings ladder, where no marketplace publishes a price filter.
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
            // A ceiling on a direct endpoint is checked against an order book
            // that does not exist, so every rung under it would be skipped for
            // missing price data — a ladder that silently serves nothing. Said
            // at load time, where it is a typo, rather than at 3am, where it is
            // an outage.
            if !provider.kind.is_marketplace() && provider.max_cost_per_1m.is_some() {
                return Err(Error::UnpriceableCeiling {
                    field: format!("providers.{name}.max_cost_per_1m"),
                    provider: name.clone(),
                });
            }
        }

        self.check_names_are_unique()?;

        for ladder in &self.ladders {
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
                if rung.max_cost_per_1m.is_some()
                    && self
                        .providers
                        .get(&rung.provider)
                        .is_some_and(|provider| !provider.kind.is_marketplace())
                {
                    return Err(Error::UnpriceableCeiling {
                        field: format!("ladder {} rung {index} max_cost_per_1m", ladder.name),
                        provider: rung.provider.clone(),
                    });
                }
                // A ceiling on the embeddings surface has nothing to bind
                // against: no marketplace publishes a price filter for it. The
                // provider's own ceiling is dropped silently by
                // [`Ladder::cap_for`] because it was written for the chat
                // ladders and inherited by accident, but one written here was
                // meant, and a limit that never limits anything is worth
                // refusing where it is still a typo.
                if !ladder.surface.is_cappable() && rung.max_cost_per_1m.is_some() {
                    return Err(Error::UncappableSurface {
                        field: format!("ladder {} rung {index} max_cost_per_1m", ladder.name),
                    });
                }
            }
        }

        Ok(())
    }

    /// Refuses two ladders that would answer to the same name.
    ///
    /// Names and aliases share one namespace, because a request cannot say
    /// which of the two it meant. Two ladders answering to one name would
    /// resolve by declaration order, which is not a policy anybody wrote down.
    ///
    /// # Errors
    ///
    /// - [`Error::DuplicateLadder`] if a name or alias is claimed twice.
    /// - [`Error::Empty`] if an alias is blank, which would otherwise be a name
    ///   no request can send and no reader can see.
    fn check_names_are_unique(&self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for ladder in &self.ladders {
            if !seen.insert(ladder.name.trim()) {
                return Err(Error::DuplicateLadder(ladder.name.clone()));
            }
            for alias in &ladder.aliases {
                if alias.trim().is_empty() {
                    return Err(Error::Empty {
                        what: format!("ladder {} alias", ladder.name),
                    });
                }
                if !seen.insert(alias.trim()) {
                    return Err(Error::DuplicateLadder(alias.trim().to_string()));
                }
            }
        }
        Ok(())
    }

    /// Looks up a ladder by the name a request used.
    ///
    /// A caller names a *model*, and the name that arrives is rarely the one
    /// written here: clients fold in their own vocabulary (`chat-v1` for the
    /// cheap ladder), append a context-variant marker (`reasoning[1m]`), and
    /// disagree about case. Matching only on the exact string turned each of
    /// those into `unknown ladder`, and the workaround — a copy of the ladder
    /// under every spelling — is how one configuration grew eleven ladders
    /// serving four intents.
    ///
    /// So four passes, in descending order of how deliberate the match is:
    ///
    /// 1. the ladder's own name, exactly;
    /// 2. a declared [`Ladder::aliases`] entry, exactly;
    /// 3. either, with a trailing `[...]` variant marker stripped;
    /// 4. either, folded to lowercase.
    ///
    /// Earlier passes win outright, so a ladder genuinely named `reasoning[1m]`
    /// still answers to it and is never shadowed by the `reasoning` that
    /// stripping it produces. Within a pass the first ladder declared wins,
    /// which validation makes moot by refusing two ladders that answer to the
    /// same name.
    ///
    /// Returns `None` when no pass matches; the caller reports that as an
    /// unknown ladder rather than guessing at what was meant.
    #[must_use]
    pub fn ladder(&self, name: &str) -> Option<&Ladder> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let stripped = strip_variant(name);
        let folded = name.to_lowercase();
        let folded_stripped = strip_variant(&folded).to_string();

        self.find(|candidate| candidate == name)
            .or_else(|| self.find(|candidate| candidate == stripped))
            .or_else(|| self.find(|candidate| candidate.to_lowercase() == folded))
            .or_else(|| self.find(|candidate| candidate.to_lowercase() == folded_stripped))
    }

    /// The first ladder with a name or alias the predicate accepts.
    ///
    /// A ladder's own name is offered before its aliases so that a pass which
    /// would match both reports the ladder for the reason a reader expects.
    fn find(&self, matches: impl Fn(&str) -> bool) -> Option<&Ladder> {
        self.ladders.iter().find(|ladder| {
            matches(&ladder.name) || ladder.aliases.iter().any(|alias| matches(alias.trim()))
        })
    }

    /// The ceiling that applies to a rung of a ladder once its provider's
    /// ceiling is folded in, in USD per million tokens.
    ///
    /// Returns `None` when neither the rung nor its provider sets one, and
    /// always on a surface that cannot enforce a ceiling — see
    /// [`Ladder::cap_for`].
    #[must_use]
    pub fn cap_for(&self, ladder: &Ladder, rung: &Rung) -> Option<f64> {
        ladder.cap_for(
            rung,
            self.providers
                .get(&rung.provider)
                .and_then(|provider| provider.max_cost_per_1m),
        )
    }
}

/// A name with a trailing `[...]` context-variant marker removed.
///
/// The Claude ACP layer appends one before a request leaves it, so the ladder
/// `reasoning` arrives as `reasoning[1m]`. The marker says which context window
/// of the same model is wanted, which is the upstream's business rather than a
/// different routing policy — the rungs that serve it are identical either way.
///
/// Only a marker that closes at the very end is removed, and never the whole
/// name: `[1m]` alone is not a request for every ladder.
fn strip_variant(name: &str) -> &str {
    let Some(open) = name.strip_suffix(']').and_then(|rest| rest.rfind('[')) else {
        return name;
    };
    if open == 0 {
        return name;
    }
    name[..open].trim_end()
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
