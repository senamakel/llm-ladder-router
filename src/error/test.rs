//! Unit tests for the crate-wide error type.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn renders_a_ladder_exhaustion() {
    let error = Error::LadderExhausted {
        ladder: "reasoning".to_string(),
    };
    assert_eq!(
        error.to_string(),
        "no rung of ladder reasoning could serve the request"
    );
}

#[test]
fn names_the_offending_rung_when_a_provider_is_unknown() {
    let error = Error::UnknownProvider {
        ladder: "flash".to_string(),
        rung: 2,
        provider: "typo".to_string(),
    };
    assert_eq!(
        error.to_string(),
        "ladder flash rung 2 names unknown provider typo"
    );
}

#[test]
fn names_the_field_of_an_unusable_price() {
    let error = Error::InvalidPrice {
        field: "ladder flash rung 0 max_cost_per_1m".to_string(),
    };
    assert!(error.to_string().contains("must be a positive finite"));
}

#[test]
fn names_the_variable_of_a_missing_credential() {
    let error = Error::MissingCredential {
        provider: "surplus".to_string(),
        variable: "SURPLUS_API_KEY".to_string(),
    };
    assert_eq!(
        error.to_string(),
        "environment variable SURPLUS_API_KEY for provider surplus is unset"
    );
}

#[test]
fn reports_which_payload_could_not_be_read() {
    let error = Error::UnreadablePayload {
        provider: "surplus".to_string(),
        what: "order book".to_string(),
    };
    assert_eq!(
        error.to_string(),
        "surplus returned an unreadable order book payload"
    );
}

#[test]
fn wraps_a_toml_failure() {
    let error = Error::from(toml::from_str::<Config>("not = [valid").unwrap_err());
    assert!(error.to_string().starts_with("invalid config"));
}

#[test]
fn is_a_standard_error() {
    fn assert_error(error: &dyn std::error::Error) {
        assert!(!error.to_string().is_empty());
    }
    assert_error(&Error::UnknownLadder("nope".to_string()));
}

use crate::config::Config;
