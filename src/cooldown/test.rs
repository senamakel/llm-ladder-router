//! Unit tests for the rate-limit cooldown store.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use super::Cooldowns;

#[test]
fn a_cooled_rung_reports_the_time_it_has_left() {
    let mut cooldowns = Cooldowns::new();
    cooldowns.cool("surplus", "glm-5.2", Duration::from_secs(30));

    let remaining = cooldowns.remaining("surplus", "glm-5.2").unwrap();
    assert!(remaining <= Duration::from_secs(30));
    assert!(remaining > Duration::from_secs(28));
}

/// One model being throttled says nothing about another on the same
/// marketplace, and parking the whole provider would empty a ladder over one
/// busy model.
#[test]
fn a_cooldown_is_per_rung_rather_than_per_provider() {
    let mut cooldowns = Cooldowns::new();
    cooldowns.cool("surplus", "glm-5.2", Duration::from_secs(30));

    assert!(cooldowns.remaining("surplus", "deepseek-v4-pro").is_none());
    assert!(cooldowns.remaining("openrouter", "glm-5.2").is_none());
}

/// Two concurrent requests can both meet the same limit; the later answer is
/// not evidence that the earlier one was wrong.
#[test]
fn a_second_rate_limit_extends_rather_than_shortens() {
    let mut cooldowns = Cooldowns::new();
    cooldowns.cool("surplus", "glm-5.2", Duration::from_secs(60));
    cooldowns.cool("surplus", "glm-5.2", Duration::from_secs(5));

    assert!(cooldowns.remaining("surplus", "glm-5.2").unwrap() > Duration::from_secs(30));
}

#[test]
fn an_elapsed_cooldown_is_over() {
    let mut cooldowns = Cooldowns::new();
    cooldowns.cool("surplus", "glm-5.2", Duration::from_nanos(1));
    std::thread::sleep(Duration::from_millis(2));

    assert!(cooldowns.remaining("surplus", "glm-5.2").is_none());
}

/// The map is keyed by rung, so it is bounded by the configuration — but an
/// entry for a rung nobody asks about again should still not sit there forever.
#[test]
fn expired_entries_are_dropped_on_the_next_write() {
    let mut cooldowns = Cooldowns::new();
    cooldowns.cool("surplus", "gone", Duration::from_nanos(1));
    std::thread::sleep(Duration::from_millis(2));

    cooldowns.cool("surplus", "glm-5.2", Duration::from_secs(30));

    assert_eq!(cooldowns.len(), 1);
    assert!(!cooldowns.is_empty());
}
