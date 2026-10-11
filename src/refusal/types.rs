//! Configuration for refusing requests by their system prompt.

use serde::Deserialize;

/// `[refuse]` — requests the router turns away before any upstream call.
///
/// ```toml
/// [refuse]
/// system_prefixes = [
///   "You are a summarisation engine",   # a background job we do not want run
/// ]
/// ```
///
/// Absent, or with an empty list, nothing is refused and no prompt is read.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Refuse {
    /// A request whose system prompt starts with any of these is refused.
    ///
    /// Matching is on the start of the prompt only, after leading whitespace,
    /// and is case-sensitive: a prefix names one fixed instruction a caller
    /// sends verbatim, and a looser match would start catching prompts that
    /// merely mention it.
    #[serde(default)]
    pub system_prefixes: Vec<String>,
}

impl Refuse {
    /// The configured prefixes that can actually match anything.
    ///
    /// An empty or whitespace-only entry would match every prompt, which is a
    /// configuration slip rather than an intent, so it is ignored.
    pub(crate) fn prefixes(&self) -> impl Iterator<Item = (usize, &str)> {
        self.system_prefixes
            .iter()
            .enumerate()
            .map(|(index, prefix)| (index, prefix.trim_start()))
            .filter(|(_, prefix)| !prefix.trim().is_empty())
    }
}
