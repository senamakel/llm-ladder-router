//! Counting how many distinct marked identities a single request body carries.
//!
//! One router usually sits in front of several callers, and an upstream request
//! is expected to belong to exactly one of them. Whether that actually holds is
//! decided by whatever builds the request — often an application the operator
//! does not control — and a router that only forwards bodies cannot normally
//! tell. This counts, so the operator can.
//!
//! The operator configures a prefix their identities share
//! (`[markers] prefix = "acct-"`). For each request the body's strings are
//! scanned for tokens starting with that prefix, and the number of **distinct**
//! tokens is reported. One is the ordinary case. More than one means a single
//! upstream call carried more than one identity's content, which is usually a
//! batching or grouping bug in the caller and is worth knowing about.
//!
//! ## What is deliberately not recorded
//!
//! Only the count leaves this module. The matched tokens are not logged, not
//! returned, and not stored, and neither is any part of the body. An operator
//! investigating a mixed request gets a number and a place to look, never a
//! copy of the content — which is the point, since the content is exactly what
//! they would not want a router to start writing down.
//!
//! Disabled unless a prefix is configured, and the scan is then a plain
//! substring walk over the strings already parsed into the body `Value`.

use std::collections::BTreeSet;

pub mod types;

pub use types::Markers;

/// The characters a marker token may contain after its prefix.
///
/// Identities are conventionally alphanumeric with `-`, `_`, `:` and `.`; a
/// token ends at the first character outside that set, so a marker embedded in
/// a sentence ends at the space or punctuation that follows it.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')
}

/// Every distinct marker in one string, appended to `found`.
fn scan_str(text: &str, prefix: &str, found: &mut BTreeSet<String>, cap: usize) {
    if prefix.is_empty() || found.len() >= cap {
        return;
    }
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        let from = &rest[at..];
        let end = from
            .char_indices()
            .skip(prefix.chars().count())
            .find(|(_, c)| !is_token_char(*c))
            .map_or(from.len(), |(index, _)| index);
        let token = &from[..end];
        // A bare prefix with nothing after it is not an identity.
        if token.chars().count() > prefix.chars().count() {
            found.insert(token.to_string());
            if found.len() >= cap {
                return;
            }
        }
        // Advance past this match even when the token was rejected, so a bare
        // prefix cannot spin here.
        let step = end.max(prefix.len()).max(1);
        rest = &from[step..];
    }
}

/// Walk every string in a JSON value.
fn walk(value: &serde_json::Value, prefix: &str, found: &mut BTreeSet<String>, cap: usize) {
    if found.len() >= cap {
        return;
    }
    match value {
        serde_json::Value::String(text) => scan_str(text, prefix, found, cap),
        serde_json::Value::Array(items) => {
            for item in items {
                walk(item, prefix, found, cap);
            }
        }
        serde_json::Value::Object(fields) => {
            for field in fields.values() {
                walk(field, prefix, found, cap);
            }
        }
        _ => {}
    }
}

/// How many distinct markers a request body carries.
///
/// `None` when counting is switched off. `Some(0)` means the body carried no
/// marker at all, which is not the same as not looking: a caller that stopped
/// labelling its requests shows up as a run of zeros rather than as silence.
#[must_use]
pub fn count(body: &serde_json::Value, markers: &Markers) -> Option<usize> {
    let prefix = markers.prefix.as_deref().filter(|p| !p.is_empty())?;
    let mut found = BTreeSet::new();
    walk(body, prefix, &mut found, markers.scan_cap());
    Some(found.len())
}

#[cfg(test)]
#[path = "test.rs"]
mod test;
