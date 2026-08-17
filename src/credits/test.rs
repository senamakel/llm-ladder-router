//! Unit tests for the balance gate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn a_provider_above_the_floor_is_usable() {
    let mut credits = CreditState::new();
    credits.set_balance("surplus", 74.67);

    assert!(
        credits
            .unusable("surplus", "SURPLUS_API_KEY", 0.50)
            .is_none()
    );
    assert!((credits.balance("surplus").unwrap().remaining_usd - 74.67).abs() < f64::EPSILON);
}

#[test]
fn a_provider_below_the_floor_reports_both_numbers() {
    let mut credits = CreditState::new();
    credits.set_balance("surplus", 0.10);

    match credits.unusable("surplus", "SURPLUS_API_KEY", 0.50) {
        Some(SkipReason::ExhaustedBalance {
            remaining_usd,
            floor_usd,
        }) => {
            assert!((remaining_usd - 0.10).abs() < f64::EPSILON);
            assert!((floor_usd - 0.50).abs() < f64::EPSILON);
        }
        other => panic!("expected ExhaustedBalance, got {other:?}"),
    }
}

#[test]
fn a_balance_exactly_at_the_floor_is_usable() {
    let mut credits = CreditState::new();
    credits.set_balance("surplus", 0.50);
    // The floor is a minimum to have, not a minimum to exceed.
    assert!(
        credits
            .unusable("surplus", "SURPLUS_API_KEY", 0.50)
            .is_none()
    );
}

#[test]
fn a_missing_credential_names_the_variable_it_wanted() {
    let mut credits = CreditState::new();
    credits.set_missing_credential("surplus", "SURPLUS_API_KEY");

    match credits.unusable("surplus", "SURPLUS_API_KEY", 0.0) {
        Some(SkipReason::MissingCredential { variable }) => {
            assert_eq!(variable, "SURPLUS_API_KEY");
        }
        other => panic!("expected MissingCredential, got {other:?}"),
    }
}

#[test]
fn a_missing_credential_outranks_a_healthy_balance() {
    let mut credits = CreditState::new();
    credits.set_balance("surplus", 100.0);
    credits.set_missing_credential("surplus", "SURPLUS_API_KEY");

    // Money is no use without a key to spend it with.
    assert!(matches!(
        credits.unusable("surplus", "SURPLUS_API_KEY", 0.50),
        Some(SkipReason::MissingCredential { .. })
    ));
}

#[test]
fn an_unpolled_provider_is_left_usable() {
    let credits = CreditState::new();

    // Refusing to route until the first poll lands would make startup fragile,
    // and a marketplace that reports no balance at all still works.
    assert!(
        credits
            .unusable("surplus", "SURPLUS_API_KEY", 99.0)
            .is_none()
    );
    assert!(credits.balance("surplus").is_none());
}

#[test]
fn a_zero_floor_never_excludes_anyone() {
    let mut credits = CreditState::new();
    credits.set_balance("surplus", 0.0);
    assert!(
        credits
            .unusable("surplus", "SURPLUS_API_KEY", 0.0)
            .is_none()
    );
}
