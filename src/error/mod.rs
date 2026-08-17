//! The crate-wide error type.
//!
//! One variant per failure mode. Messages are lowercase and carry no trailing
//! punctuation; their text is not a stable API and callers should match on the
//! variant instead.

/// Errors returned by this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The configuration file could not be read.
    #[error("cannot read config {path}: {source}")]
    ConfigRead {
        /// The path that was attempted.
        path: String,
        /// The underlying I/O failure.
        source: std::io::Error,
    },

    /// The configuration file was not valid TOML, or did not match the schema.
    #[error("invalid config: {0}")]
    ConfigParse(#[from] toml::de::Error),

    /// A ladder rung named a provider that the configuration does not define.
    #[error("ladder {ladder} rung {rung} names unknown provider {provider}")]
    UnknownProvider {
        /// The ladder holding the offending rung.
        ladder: String,
        /// The zero-based index of the rung.
        rung: usize,
        /// The provider name that could not be resolved.
        provider: String,
    },

    /// A price ceiling was not a usable positive, finite number of dollars.
    #[error("{field} must be a positive finite number of dollars")]
    InvalidPrice {
        /// The configuration field that carried the bad value.
        field: String,
    },

    /// The configuration defined no ladders, or a ladder had no rungs.
    #[error("{what} must not be empty")]
    Empty {
        /// What was found to be empty.
        what: String,
    },

    /// Two ladders shared a name, so a request could not select between them.
    #[error("duplicate ladder name {0}")]
    DuplicateLadder(String),

    /// A provider's credential environment variable was unset or blank.
    #[error("environment variable {variable} for provider {provider} is unset")]
    MissingCredential {
        /// The provider that needs the credential.
        provider: String,
        /// The environment variable that should have carried it.
        variable: String,
    },

    /// An upstream marketplace call failed at the transport level.
    #[error("{provider} request failed: {source}")]
    Upstream {
        /// The provider that was called.
        provider: String,
        /// The underlying transport failure.
        source: reqwest::Error,
    },

    /// An upstream marketplace returned a body that did not match its schema.
    #[error("{provider} returned an unreadable {what} payload")]
    UnreadablePayload {
        /// The provider that was called.
        provider: String,
        /// Which payload was being read.
        what: String,
    },

    /// A request named a ladder that the configuration does not define.
    #[error("unknown ladder {0}")]
    UnknownLadder(String),

    /// Every rung was skipped or failed, so the request could not be served.
    #[error("no rung of ladder {ladder} could serve the request")]
    LadderExhausted {
        /// The ladder that ran out of rungs.
        ladder: String,
    },

    /// The proxy could not bind its listening socket.
    #[error("cannot bind {address}: {source}")]
    Bind {
        /// The address that was attempted.
        address: String,
        /// The underlying I/O failure.
        source: std::io::Error,
    },

    /// The HTTP server stopped with an error.
    #[error("server failed: {0}")]
    Serve(std::io::Error),
}

/// The result type returned by every fallible function in this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod test;
