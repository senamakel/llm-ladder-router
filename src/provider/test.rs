//! Unit tests for the provider dispatch layer.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::ProviderKind;

fn provider(kind: ProviderKind) -> Provider {
    Provider {
        kind,
        base_url: "https://example.test/v1".to_string(),
        api_key_env: "LADDER_TEST_UNSET_KEY".to_string(),
        max_cost_per_1m: None,
        headers: std::collections::BTreeMap::new(),
    }
}

#[test]
fn an_injected_credential_is_used() {
    let client = Client::with_credential(
        "surplus",
        provider(ProviderKind::Surplus),
        reqwest::Client::new(),
        Some("secret".to_string()),
    );

    assert_eq!(client.name(), "surplus");
    assert!(client.has_credential());
    assert_eq!(client.credential_variable(), "LADDER_TEST_UNSET_KEY");
}

#[test]
fn a_blank_credential_counts_as_absent() {
    // An empty or whitespace-only variable is a misconfiguration, not a key.
    for blank in ["", "   ", "\n"] {
        let client = Client::with_credential(
            "surplus",
            provider(ProviderKind::Surplus),
            reqwest::Client::new(),
            Some(blank.to_string()),
        );
        assert!(!client.has_credential(), "{blank:?} should not count as a key");
    }
}

#[test]
fn an_absent_environment_variable_leaves_the_client_without_a_credential() {
    // The variable name is deliberately one nothing sets.
    let client = Client::new(
        "surplus",
        provider(ProviderKind::Surplus),
        reqwest::Client::new(),
    );
    assert!(!client.has_credential());
}

#[test]
fn a_credential_is_trimmed() {
    let client = Client::with_credential(
        "surplus",
        provider(ProviderKind::Surplus),
        reqwest::Client::new(),
        Some("  secret  ".to_string()),
    );
    assert!(client.has_credential());
}

#[test]
fn the_serving_sub_provider_is_read_from_the_response_body() {
    assert_eq!(
        served_by(br#"{"provider":"DeepInfra","choices":[]}"#),
        Some("DeepInfra".to_string())
    );
}

#[test]
fn a_body_without_a_provider_field_names_nobody() {
    // Anthropic responses carry no top-level `provider`, and guessing is worse
    // than reporting nothing.
    assert_eq!(served_by(br#"{"type":"message","content":[]}"#), None);
    assert_eq!(served_by(b"not json"), None);
    assert_eq!(served_by(br#"{"provider":42}"#), None);
}

#[test]
fn statuses_are_classified_by_who_is_at_fault() {
    assert_eq!(classify_status(reqwest::StatusCode::OK), Disposition::Served);
    assert_eq!(
        classify_status(reqwest::StatusCode::NO_CONTENT),
        Disposition::Served
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::BAD_GATEWAY),
        Disposition::Advance
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS),
        Disposition::Advance
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::UNAUTHORIZED),
        Disposition::CallerError
    );
    assert_eq!(
        classify_status(reqwest::StatusCode::UNPROCESSABLE_ENTITY),
        Disposition::CallerError
    );
}
