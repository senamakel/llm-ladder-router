//! Unit tests for the session pin store.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use super::*;

fn pin(rung: usize) -> Pin {
    Pin {
        ladder: "flash".to_string(),
        rung,
        provider: "surplus".to_string(),
        model: "gpt-5.6-luna".to_string(),
        sub_provider: Some("Z.ai".to_string()),
        cap_per_1m: Some(0.30),
        pinned_at: Instant::now(),
    }
}

fn store() -> SessionPins {
    SessionPins::new(Duration::from_secs(60), 100)
}

#[test]
fn a_pinned_session_is_returned() {
    let mut pins = store();
    assert!(pins.is_empty());

    pins.pin("thread-1", pin(0));

    assert_eq!(pins.len(), 1);
    assert_eq!(pins.get("thread-1").unwrap().rung, 0);
    assert!(pins.get("thread-2").is_none());
}

#[test]
fn pinning_the_same_session_again_replaces_it() {
    let mut pins = store();
    pins.pin("thread-1", pin(0));
    pins.pin("thread-1", pin(2));

    assert_eq!(pins.len(), 1);
    assert_eq!(pins.get("thread-1").unwrap().rung, 2);
}

#[test]
fn a_session_can_be_unpinned() {
    let mut pins = store();
    pins.pin("thread-1", pin(0));
    pins.unpin("thread-1");

    assert!(pins.get("thread-1").is_none());
    assert!(pins.is_empty());
}

#[test]
fn unpinning_a_session_that_was_never_pinned_is_harmless() {
    let mut pins = store();
    pins.unpin("never-seen");
    assert!(pins.is_empty());
}

#[test]
fn an_expired_pin_is_not_returned() {
    let mut pins = SessionPins::new(Duration::ZERO, 100);
    let mut stale = pin(0);
    stale.pinned_at = Instant::now() - Duration::from_secs(1);
    pins.pin("thread-1", stale);

    // Past the TTL the conversation is assumed abandoned and its cache cold.
    assert!(pins.get("thread-1").is_none());
}

#[test]
fn expired_pins_are_evicted_when_new_ones_arrive() {
    let mut pins = SessionPins::new(Duration::from_millis(1), 100);
    let mut stale = pin(0);
    stale.pinned_at = Instant::now() - Duration::from_secs(60);
    pins.pin("old", stale);

    pins.pin("new", pin(1));

    // The stale entry must not linger just because nobody asked for it.
    assert_eq!(pins.len(), 1);
    assert!(pins.get("new").is_some());
}

#[test]
fn the_store_stays_within_its_bound_by_dropping_the_oldest() {
    let mut pins = SessionPins::new(Duration::from_secs(3600), 3);

    for index in 0..6 {
        let mut entry = pin(0);
        // Space them out so "oldest" is unambiguous.
        entry.pinned_at = Instant::now() - Duration::from_secs(60 - index as u64);
        pins.pin(format!("thread-{index}"), entry);
    }

    // Sessions are named by callers and never closed, so the bound is what
    // stops this growing for the life of the process.
    assert!(pins.len() <= 3, "held {} pins", pins.len());
    // The most recent survives; the first does not.
    assert!(pins.get("thread-5").is_some());
    assert!(pins.get("thread-0").is_none());
}

#[test]
fn a_pin_remembers_the_sub_provider_holding_the_warm_cache() {
    let mut pins = store();
    pins.pin("thread-1", pin(0));

    let held = pins.get("thread-1").unwrap();
    assert_eq!(held.sub_provider.as_deref(), Some("Z.ai"));
    assert_eq!(held.ladder, "flash");
    assert_eq!(held.model, "gpt-5.6-luna");
    assert_eq!(held.cap_per_1m, Some(0.30));
}

#[test]
fn every_rejection_reason_reads_as_a_sentence() {
    for (reason, expected) in [
        (PinRejected::DifferentLadder, "session moved to a different ladder"),
        (PinRejected::CeilingChanged, "the rung's ceiling changed"),
        (PinRejected::RungUnavailable, "the pinned rung can no longer serve"),
        (PinRejected::RungGone, "the pinned rung no longer exists"),
    ] {
        assert_eq!(reason.to_string(), expected);
    }
}
