use super::*;
use serde_json::json;

fn markers(prefix: &str) -> Markers {
    Markers {
        prefix: Some(prefix.to_string()),
        warn_above: 1,
        scan_cap: 16,
    }
}

#[test]
fn counting_is_off_without_a_prefix() {
    let body = json!({"messages": [{"content": "acct-alice and acct-bob"}]});
    assert_eq!(count(&body, &Markers::default()), None);
    let empty = Markers {
        prefix: Some(String::new()),
        ..Markers::default()
    };
    assert_eq!(count(&body, &empty), None);
}

#[test]
fn one_caller_counts_one_however_often_it_appears() {
    let body = json!({
        "model": "ladder",
        "messages": [
            {"role": "system", "content": "acct-alice is the subject"},
            {"role": "user", "content": "more about acct-alice, and acct-alice again"}
        ]
    });
    assert_eq!(count(&body, &markers("acct-")), Some(1));
}

#[test]
fn two_callers_in_one_body_count_two() {
    let body = json!({
        "messages": [{"content": "acct-alice said one thing"}, {"content": "acct-bob said another"}]
    });
    assert_eq!(count(&body, &markers("acct-")), Some(2));
}

/// The shape that motivated this: a batched embeddings request whose `input`
/// array carries several callers' text.
#[test]
fn a_batched_array_body_is_counted_across_its_elements() {
    let body = json!({
        "model": "embed",
        "input": [
            "acct-alice first document",
            "acct-alice second document",
            "acct-bob first document"
        ]
    });
    assert_eq!(count(&body, &markers("acct-")), Some(2));
}

#[test]
fn markers_are_found_at_any_depth() {
    let body = json!({"a": {"b": [{"c": ["deep acct-alice"]}]}, "d": "acct-bob"});
    assert_eq!(count(&body, &markers("acct-")), Some(2));
}

#[test]
fn a_marker_ends_at_punctuation_or_space() {
    let body = json!({"content": "from acct-alice, to acct-bob. also acct-alice;"});
    assert_eq!(count(&body, &markers("acct-")), Some(2));
}

#[test]
fn a_bare_prefix_is_not_an_identity() {
    let body = json!({"content": "acct- is just a prefix, acct- again"});
    assert_eq!(count(&body, &markers("acct-")), Some(0));
}

#[test]
fn a_body_with_no_marker_counts_zero_rather_than_nothing() {
    // Zero is a real observation: a caller that stopped labelling its requests
    // should read as a run of zeros, not as the feature being off.
    let body = json!({"messages": [{"content": "nothing labelled here"}]});
    assert_eq!(count(&body, &markers("acct-")), Some(0));
}

#[test]
fn non_string_values_are_ignored() {
    let body = json!({"n": 42, "b": true, "nil": null, "s": "acct-alice"});
    assert_eq!(count(&body, &markers("acct-")), Some(1));
}

#[test]
fn the_scan_stops_at_the_cap() {
    let many: Vec<String> = (0..50).map(|i| format!("acct-{i}")).collect();
    let body = json!({ "input": many });
    let capped = Markers {
        prefix: Some("acct-".into()),
        warn_above: 1,
        scan_cap: 4,
    };
    assert_eq!(count(&body, &capped), Some(4));
}

#[test]
fn a_zero_cap_still_scans() {
    let zero = Markers {
        prefix: Some("acct-".into()),
        warn_above: 1,
        scan_cap: 0,
    };
    assert_eq!(zero.scan_cap(), 1);
    let body = json!({"content": "acct-alice"});
    assert_eq!(count(&body, &zero), Some(1));
}

#[test]
fn warning_turns_on_above_the_threshold_only() {
    let m = markers("acct-");
    assert!(!m.should_warn(0));
    assert!(!m.should_warn(1));
    assert!(m.should_warn(2));
    assert!(!Markers::default().should_warn(9), "off without a prefix");
}

#[test]
fn a_repeated_prefix_cannot_spin() {
    // Regression guard: the scan must advance past a rejected bare prefix.
    let body = json!({"content": "acct-acct-acct-"});
    assert!(count(&body, &markers("acct-")).is_some());
}
