//! Configuration for marker counting.

use serde::Deserialize;

/// `[markers]` — how a request's caller identities are recognised, if at all.
///
/// ```toml
/// [markers]
/// prefix = "acct-"    # tokens starting with this are caller identities
/// warn_above = 1      # log a warning when one request carries more than this
/// ```
///
/// Absent means the feature is off and no body is scanned.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Markers {
    /// The prefix a caller identity starts with. Counting is off without it.
    #[serde(default)]
    pub prefix: Option<String>,

    /// Log a warning when one request carries more than this many distinct
    /// markers. The default, 1, warns on the first request that mixes two.
    #[serde(default = "Markers::default_warn_above")]
    pub warn_above: usize,

    /// Stop scanning a body once this many distinct markers are found.
    ///
    /// The count is used to decide "one, or more than one", so an exact figure
    /// past a small number buys nothing and a pathological body should not be
    /// able to make the router walk it indefinitely. A count that hit the cap
    /// is still above `warn_above`, which is what the warning turns on.
    #[serde(default = "Markers::default_scan_cap")]
    pub scan_cap: usize,
}

impl Markers {
    fn default_warn_above() -> usize {
        1
    }

    fn default_scan_cap() -> usize {
        16
    }

    /// The cap, never zero — a zero cap would silently disable the scan while
    /// the operator believed a configured prefix was being counted.
    #[must_use]
    pub fn scan_cap(&self) -> usize {
        self.scan_cap.max(1)
    }

    /// Whether `count` distinct markers in one request is worth a warning.
    #[must_use]
    pub fn should_warn(&self, count: usize) -> bool {
        self.prefix.is_some() && count > self.warn_above
    }
}
