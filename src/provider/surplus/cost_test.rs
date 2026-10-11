//! Unit tests for stamping Surplus's charge into `usage.cost`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

/// The final usage event of a real streamed `deepseek-v4.1-flash` answer,
/// captured 2026-10-12: a charge, and no `cost` at all.
const USAGE_EVENT: &str = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":32,\"completion_tokens\":39,\"buyer_cost_micro\":4}}\n";

fn cost_of(body: &[u8]) -> Option<f64> {
    serde_json::from_slice::<serde_json::Value>(body).ok()?["usage"]["cost"].as_f64()
}

#[test]
fn a_buffered_answer_reporting_zero_cost_gets_the_header_charge() {
    // What a buffered answer really carries: the seller's BYOK zero.
    let body = br#"{"choices":[],"usage":{"prompt_tokens":32,"cost":0,"is_byok":true}}"#;
    let stamped = stamp_body(body, Some(23.0)).unwrap();
    assert_eq!(cost_of(&stamped), Some(0.000_023));
}

#[test]
fn the_bodys_own_charge_wins_over_the_header() {
    let body = br#"{"usage":{"buyer_cost_micro":4}}"#;
    assert_eq!(
        cost_of(&stamp_body(body, Some(99.0)).unwrap()),
        Some(0.000_004)
    );
}

#[test]
fn a_body_naming_no_charge_is_left_as_it_came() {
    assert_eq!(stamp_body(br#"{"usage":{"cost":0}}"#, None), None);
    assert_eq!(stamp_body(br#"{"choices":[]}"#, Some(4.0)), None);
    assert_eq!(stamp_body(b"not json", Some(4.0)), None);
    assert_eq!(
        stamp_body(br#"{"usage":{"buyer_cost_micro":-1}}"#, None),
        None
    );
    assert!(!stamp_usage(&mut serde_json::json!(7), Some(4.0)));
}

#[test]
fn a_streams_usage_event_gets_its_charge_and_every_other_byte_passes_unchanged() {
    let delta = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
    let stream = format!(": keep-alive\n{delta}{USAGE_EVENT}\ndata: [DONE]\n\n");
    let mut stamper = SseStamper::default();
    let mut out = stamper.feed(stream.as_bytes());
    out.extend(stamper.finish());
    let out = String::from_utf8(out).unwrap();

    assert!(out.starts_with(&format!(": keep-alive\n{delta}")));
    assert!(out.ends_with("\ndata: [DONE]\n\n"));
    let usage_line = out
        .lines()
        .find(|line| line.contains("buyer_cost_micro"))
        .unwrap();
    let event: serde_json::Value = serde_json::from_str(&usage_line["data: ".len()..]).unwrap();
    assert_eq!(event["usage"]["cost"], 0.000_004);
    assert_eq!(event["usage"]["prompt_tokens"], 32);
}

#[test]
fn an_event_split_across_chunks_is_stamped_once_whole() {
    let mut stamper = SseStamper::default();
    let (head, tail) = USAGE_EVENT.split_at(30);
    // Half an event is held back rather than relayed half-stamped.
    assert_eq!(stamper.feed(head.as_bytes()), [] as [u8; 0]);
    let out = stamper.feed(tail.as_bytes());
    let event: serde_json::Value =
        serde_json::from_slice(&out["data: ".len()..out.len() - 1]).unwrap();
    assert_eq!(event["usage"]["cost"], 0.000_004);
    assert_eq!(stamper.finish(), [] as [u8; 0]);
}

#[test]
fn a_stream_ending_without_a_newline_is_still_released_and_stamped() {
    let mut stamper = SseStamper::default();
    assert_eq!(
        stamper.feed(USAGE_EVENT.trim_end().as_bytes()),
        [] as [u8; 0]
    );
    let out = stamper.finish();
    let event: serde_json::Value = serde_json::from_slice(&out["data: ".len()..]).unwrap();
    assert_eq!(event["usage"]["cost"], 0.000_004);
}

#[test]
fn crlf_endings_survive_stamping() {
    let mut stamper = SseStamper::default();
    let out = stamper.feed(USAGE_EVENT.replace('\n', "\r\n").as_bytes());
    assert!(out.ends_with(b"}\r\n"));
}

#[test]
fn a_data_line_mentioning_the_field_without_a_usable_charge_passes_unchanged() {
    let line = "data: {\"note\":\"buyer_cost_micro\"}\n";
    let mut stamper = SseStamper::default();
    assert_eq!(stamper.feed(line.as_bytes()), line.as_bytes());
}
