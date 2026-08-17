//! Unit tests for how a routing decision explains itself.
//!
//! These messages are what a 502 shows an operator, so each one is pinned.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::types::SkipReason;

#[test]
fn a_missing_credential_names_the_variable() {
    let reason = SkipReason::MissingCredential {
        variable: "SURPLUS_API_KEY".to_string(),
    };
    assert_eq!(reason.to_string(), "credential SURPLUS_API_KEY is unset");
}

#[test]
fn an_exhausted_balance_names_both_figures() {
    let reason = SkipReason::ExhaustedBalance {
        remaining_usd: 0.1,
        floor_usd: 0.5,
    };
    assert_eq!(
        reason.to_string(),
        "balance $0.10 is below the $0.50 floor"
    );
}

#[test]
fn missing_and_stale_price_data_read_differently() {
    assert_eq!(SkipReason::NoPriceData.to_string(), "no price data");
    assert_eq!(
        SkipReason::StalePriceData.to_string(),
        "price data is stale"
    );
}

#[test]
fn a_priced_out_rung_names_the_ceiling_and_the_cheapest_seller() {
    let reason = SkipReason::NoSellerUnderCap {
        cap_per_1m: 0.3,
        cheapest_per_1m: Some(0.63),
    };
    // Knowing the floor was 0.63 against a 0.30 ceiling is what makes the
    // decision reviewable.
    assert_eq!(
        reason.to_string(),
        "no seller under $0.3/Mtok; cheapest is $0.63/Mtok"
    );
}

#[test]
fn a_rung_with_no_usable_seller_at_all_says_so() {
    let reason = SkipReason::NoSellerUnderCap {
        cap_per_1m: 0.3,
        cheapest_per_1m: None,
    };
    assert_eq!(reason.to_string(), "no usable seller under $0.3/Mtok");
}

#[test]
fn an_upstream_failure_carries_the_detail_through() {
    let reason = SkipReason::UpstreamFailed {
        detail: "503 all sellers unhealthy".to_string(),
    };
    assert_eq!(
        reason.to_string(),
        "upstream failed: 503 all sellers unhealthy"
    );
}
