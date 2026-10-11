//! Unit tests for reading cost figures out of upstream answers, and for the
//! `[usage_sink]` section's validation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::Config;
use crate::error::Error;

/// A real Surplus usage object, captured from a live glm-5.3-flash call on
/// 2026-10-09. Surplus's own `cost` is 0 because the call was BYOK; the market
/// price is in `cost_details`, and what Surplus charged is `buyer_cost_micro`.
const SURPLUS_USAGE: &str = r#"{"prompt_tokens":13,"completion_tokens":16,"total_tokens":29,"cost":0,"is_byok":true,"cost_details":{"upstream_inference_cost":0.00000995,"upstream_inference_prompt_cost":0.00000195,"upstream_inference_completions_cost":0.000008},"buyer_cost_micro":0}"#;

fn completion(usage: &str) -> Vec<u8> {
    format!(
        r#"{{"id":"x","object":"chat.completion","choices":[{{"message":{{"role":"assistant","content":"hi"}}}}],"usage":{usage}}}"#
    )
    .into_bytes()
}

const JSON: Option<&str> = Some("application/json");
const SSE: Option<&str> = Some("text/event-stream; charset=utf-8");

#[test]
fn a_surplus_answer_reports_its_charge_and_the_market_price_separately() {
    let figures = figures(
        ProviderKind::Surplus,
        JSON,
        &completion(SURPLUS_USAGE),
        None,
        None,
    );

    assert_eq!(figures.prompt_tokens, Some(13));
    assert_eq!(figures.completion_tokens, Some(16));
    // Charged nothing, and a zero is a figure, not an unknown.
    assert_eq!(figures.actual_usd, Some(0.0));
    assert_eq!(figures.market_usd, Some(0.000_009_95));
    assert_eq!(figures.market_source, Some(MarketSource::Reported));
}

#[test]
fn a_surplus_charge_in_micro_usd_is_converted_to_dollars() {
    let usage = r#"{"prompt_tokens":1000,"completion_tokens":500,"buyer_cost_micro":2500}"#;
    let figures = figures(ProviderKind::Surplus, JSON, &completion(usage), None, None);

    assert_eq!(figures.actual_usd, Some(0.0025));
    // No cost details means no market price, never a guess at one.
    assert_eq!(figures.market_usd, None);
}

#[test]
fn surplus_falls_back_to_its_charge_header_only_when_the_body_names_none() {
    let bare = completion(r#"{"prompt_tokens":3,"completion_tokens":4}"#);
    let from_header = figures(ProviderKind::Surplus, JSON, &bare, Some(1200.0), None);
    assert_eq!(from_header.actual_usd, Some(0.0012));

    // The body wins when it carries a charge.
    let both = figures(
        ProviderKind::Surplus,
        JSON,
        &completion(SURPLUS_USAGE),
        Some(1200.0),
        None,
    );
    assert_eq!(both.actual_usd, Some(0.0));

    // Another provider's header is not Surplus's charge.
    let other = figures(ProviderKind::OpenRouter, JSON, &bare, Some(1200.0), None);
    assert_eq!(other.actual_usd, None);
}

#[test]
fn an_openrouter_cost_is_both_the_charge_and_the_market_price() {
    let usage = r#"{"prompt_tokens":20,"completion_tokens":10,"total_tokens":30,"cost":0.000042,"is_byok":false}"#;
    let figures = figures(
        ProviderKind::OpenRouter,
        JSON,
        &completion(usage),
        None,
        None,
    );

    assert_eq!(figures.prompt_tokens, Some(20));
    assert_eq!(figures.completion_tokens, Some(10));
    assert_eq!(figures.actual_usd, Some(0.000_042));
    assert_eq!(figures.market_usd, Some(0.000_042));
}

#[test]
fn surplus_cost_field_is_not_mistaken_for_a_price() {
    // Surplus's `cost` is 0 on a BYOK call that still had a market price;
    // reading it as OpenRouter's would report a free call.
    let usage = r#"{"prompt_tokens":1,"completion_tokens":1,"cost":0.5}"#;
    let figures = figures(ProviderKind::Surplus, JSON, &completion(usage), None, None);
    assert_eq!(figures.actual_usd, None);
    assert_eq!(figures.market_usd, None);
}

#[test]
fn a_direct_provider_reports_tokens_and_no_cost() {
    let usage = r#"{"prompt_tokens":5,"completion_tokens":6,"cost":1.0}"#;
    let figures = figures(ProviderKind::Mistral, JSON, &completion(usage), None, None);
    assert_eq!(figures.prompt_tokens, Some(5));
    assert_eq!(figures.actual_usd, None);
    assert_eq!(figures.market_usd, None);
}

#[test]
fn anthropic_and_responses_token_spellings_are_read() {
    let messages = br#"{"type":"message","usage":{"input_tokens":7,"output_tokens":9}}"#;
    let figures = figures(ProviderKind::OpenRouter, JSON, messages, None, None);
    assert_eq!(figures.prompt_tokens, Some(7));
    assert_eq!(figures.completion_tokens, Some(9));
}

#[test]
fn an_embeddings_answer_has_prompt_tokens_only() {
    let body = br#"{"object":"list","data":[],"usage":{"prompt_tokens":8,"total_tokens":8}}"#;
    let figures = figures(ProviderKind::Surplus, JSON, body, None, None);
    assert_eq!(figures.prompt_tokens, Some(8));
    assert_eq!(figures.completion_tokens, None);
}

#[test]
fn a_body_without_usage_or_unreadable_is_all_unknown() {
    let none = Figures::default();
    assert_eq!(
        figures(ProviderKind::Surplus, JSON, br#"{"id":"x"}"#, None, None),
        none
    );
    assert_eq!(
        figures(
            ProviderKind::Surplus,
            JSON,
            b"<html>bad gateway</html>",
            None,
            None
        ),
        none
    );
    assert_eq!(
        figures(
            ProviderKind::Surplus,
            None,
            br#"{"usage":"n/a"}"#,
            None,
            None
        ),
        none
    );
}

#[test]
fn a_negative_or_non_numeric_cost_is_unknown_rather_than_believed() {
    let usage = r#"{"buyer_cost_micro":-5,"cost_details":{"upstream_inference_cost":"0.1"}}"#;
    let figures = figures(ProviderKind::Surplus, JSON, &completion(usage), None, None);
    assert_eq!(figures.actual_usd, None);
    assert_eq!(figures.market_usd, None);
}

/// A Surplus chat stream as it arrives: content deltas, then a final chunk
/// carrying usage with empty choices, then `[DONE]`.
fn surplus_stream() -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"Hel\"}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{\"content\":\"lo\"}}}}]}}\n\n\
         data: {{\"choices\":[],\"usage\":{SURPLUS_USAGE}}}\n\n\
         data: [DONE]\n\n"
    )
}

#[test]
fn a_stream_reports_the_usage_in_its_final_chunk() {
    let stream = surplus_stream();
    let figures = figures(ProviderKind::Surplus, SSE, stream.as_bytes(), None, None);

    assert_eq!(figures.prompt_tokens, Some(13));
    assert_eq!(figures.completion_tokens, Some(16));
    assert_eq!(figures.actual_usd, Some(0.0));
    assert_eq!(figures.market_usd, Some(0.000_009_95));
    assert_eq!(figures.market_source, Some(MarketSource::Reported));
}

#[test]
fn the_tap_reads_a_stream_split_at_every_possible_byte() {
    let stream = surplus_stream();
    let bytes = stream.as_bytes();
    let whole = figures(ProviderKind::Surplus, SSE, bytes, None, None);

    // One byte at a time is the worst case for line reassembly.
    let mut tap = SseTap::new(ProviderKind::Surplus);
    for byte in bytes {
        tap.feed(std::slice::from_ref(byte));
    }
    assert_eq!(tap.finish(), whole);

    // And every two-way split, so no boundary lands where the tap mishandles it.
    for split in 0..bytes.len() {
        let mut tap = SseTap::new(ProviderKind::Surplus);
        tap.feed(&bytes[..split]);
        tap.feed(&bytes[split..]);
        assert_eq!(tap.finish(), whole, "split at {split}");
    }
}

#[test]
fn the_tap_reads_crlf_lines_and_a_stream_without_a_final_newline() {
    let stream = "data: {\"choices\":[]}\r\n\r\ndata: {\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3,\"cost\":0.01}}";
    let mut tap = SseTap::new(ProviderKind::OpenRouter);
    tap.feed(stream.as_bytes());
    let figures = tap.finish();
    assert_eq!(figures.prompt_tokens, Some(2));
    assert_eq!(figures.actual_usd, Some(0.01));
}

#[test]
fn an_anthropic_stream_merges_usage_spread_across_events() {
    // Input tokens arrive in `message_start`, output tokens in `message_delta`.
    let stream = "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n\
         event: content_block_delta\n\
         data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n\
         event: message_delta\n\
         data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n";
    let figures = figures(ProviderKind::OpenRouter, SSE, stream.as_bytes(), None, None);
    assert_eq!(figures.prompt_tokens, Some(11));
    assert_eq!(figures.completion_tokens, Some(42));
}

#[test]
fn a_responses_stream_reads_usage_from_the_completed_response() {
    let stream = "event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":5}}}\n\n";
    let figures = figures(ProviderKind::Surplus, SSE, stream.as_bytes(), None, None);
    assert_eq!(figures.prompt_tokens, Some(4));
    assert_eq!(figures.completion_tokens, Some(5));
}

#[test]
fn a_stream_without_usage_is_all_unknown_and_the_header_still_applies() {
    let stream =
        "data: {\"choices\":[{\"delta\":{\"content\":\"the word usage\"}}]}\n\ndata: [DONE]\n\n";
    let figures = figures(
        ProviderKind::Surplus,
        SSE,
        stream.as_bytes(),
        Some(7.0),
        None,
    );
    assert_eq!(figures.prompt_tokens, None);
    assert_eq!(figures.actual_usd, Some(0.000_007));
}

#[test]
fn an_oversized_line_is_skipped_without_losing_the_lines_around_it() {
    let huge = "x".repeat(MAX_LINE + 10);
    let mut tap = SseTap::new(ProviderKind::OpenRouter);
    tap.feed(b"data: {\"usage\":{\"prompt_tokens\":1}}\n");
    tap.feed(format!("data: {{\"usage\":{{\"prompt_tokens\":99}},\"pad\":\"{huge}").as_bytes());
    tap.feed(b"\"}\ndata: {\"usage\":{\"completion_tokens\":2}}\n");
    let figures = tap.finish();
    // The oversized event is dropped, not half-read; its neighbours count.
    assert_eq!(figures.prompt_tokens, Some(1));
    assert_eq!(figures.completion_tokens, Some(2));
}

#[test]
fn a_record_carries_routing_facts_and_figures_under_a_fresh_id() {
    let figures = Figures {
        prompt_tokens: Some(13),
        completion_tokens: Some(16),
        actual_usd: Some(0.0),
        market_usd: None,
        market_source: None,
    };
    let first = Record::new(
        "memory-flash",
        0,
        "surplus",
        "glm-5.3-flash",
        "chat",
        figures,
    );
    let second = Record::new(
        "memory-flash",
        0,
        "surplus",
        "glm-5.3-flash",
        "chat",
        figures,
    );
    assert_ne!(first.id, second.id);
    assert_eq!(first.id.len(), 36);
    assert!(first.at.ends_with('Z'), "{}", first.at);

    log(&first);
    let json = serde_json::to_value(&first).unwrap();
    assert_eq!(json["ladder"], "memory-flash");
    assert_eq!(json["rung"], 0);
    assert_eq!(json["provider"], "surplus");
    assert_eq!(json["model"], "glm-5.3-flash");
    assert_eq!(json["surface"], "chat");
    assert_eq!(json["prompt_tokens"], 13);
    assert_eq!(json["completion_tokens"], 16);
    assert_eq!(json["actual_usd"], 0.0);
    // Unknown is posted as null, not left out and not zero.
    assert!(json["market_usd"].is_null());
    assert!(json.as_object().unwrap().contains_key("market_usd"));
    assert!(json["market_source"].is_null());
    assert!(json.as_object().unwrap().contains_key("market_source"));
}

const LADDER: &str = r#"
[providers.surplus]
kind = "surplus"
base_url = "http://127.0.0.1:1"
api_key_env = "LADDER_TEST_UNSET_KEY"

[[ladders]]
name = "flash"
  [[ladders.rungs]]
  provider = "surplus"
  model = "m"
"#;

fn with_sink(sink: &str) -> crate::error::Result<Config> {
    Config::parse(&format!("{LADDER}\n[usage_sink]\n{sink}\n"))
}

#[test]
fn no_usage_sink_means_the_feed_is_off() {
    let config = Config::parse(LADDER).unwrap();
    assert!(config.usage_sink.is_none());
}

#[test]
fn a_usage_sink_takes_defaults_for_everything_but_where_and_how() {
    let config = with_sink(
        "url = \"https://backend.test/internal/memory/model-usage\"\ntoken_env = \"LADDER_TEST_UNSET_TOKEN\"",
    )
    .unwrap();
    let sink = config.usage_sink.unwrap();
    assert_eq!(sink.url, "https://backend.test/internal/memory/model-usage");
    assert_eq!(sink.token_env, "LADDER_TEST_UNSET_TOKEN");
    assert_eq!(sink.flush_interval, std::time::Duration::from_secs(5));
    assert_eq!(sink.max_batch, 100);
    assert_eq!(sink.queue_size, 10_000);
    assert_eq!(sink.max_retries, 3);
    assert_eq!(sink.retry_backoff, std::time::Duration::from_secs(1));
    assert_eq!(sink.timeout, std::time::Duration::from_secs(10));
}

#[test]
fn every_usage_sink_knob_can_be_set() {
    let sink = with_sink(
        r#"url = "http://127.0.0.1:9/usage"
           token_env = "T"
           flush_interval = "250ms"
           max_batch = 7
           queue_size = 9
           max_retries = 10
           retry_backoff = "2s"
           timeout = "3s""#,
    )
    .unwrap()
    .usage_sink
    .unwrap();
    assert_eq!(sink.flush_interval, std::time::Duration::from_millis(250));
    assert_eq!(sink.max_batch, 7);
    assert_eq!(sink.queue_size, 9);
    assert_eq!(sink.max_retries, 10);
}

#[test]
fn a_usage_sink_with_an_unknown_key_is_refused() {
    assert!(matches!(
        with_sink("url = \"https://x.test\"\ntoken_env = \"T\"\ntoken = \"inline\""),
        Err(Error::ConfigParse(_))
    ));
}

#[test]
fn a_usage_sink_missing_where_or_how_is_refused() {
    assert!(matches!(
        with_sink("token_env = \"T\""),
        Err(Error::ConfigParse(_))
    ));
    assert!(matches!(
        with_sink("url = \"https://x.test\""),
        Err(Error::ConfigParse(_))
    ));
    assert!(matches!(
        with_sink("url = \" \"\ntoken_env = \"T\""),
        Err(Error::Empty { what }) if what == "usage_sink.url"
    ));
    assert!(matches!(
        with_sink("url = \"https://x.test\"\ntoken_env = \"\""),
        Err(Error::Empty { what }) if what == "usage_sink.token_env"
    ));
}

#[test]
fn a_usage_sink_with_an_unusable_value_names_the_field() {
    let cases = [
        ("url = \"backend.test/usage\"", "usage_sink.url"),
        ("url = \"ftp://backend.test/usage\"", "usage_sink.url"),
        ("flush_interval = \"0s\"", "usage_sink.flush_interval"),
        ("timeout = \"0s\"", "usage_sink.timeout"),
        ("max_batch = 0", "usage_sink.max_batch"),
        ("queue_size = 0", "usage_sink.queue_size"),
        ("max_retries = 11", "usage_sink.max_retries"),
    ];
    for (line, field) in cases {
        let text = if line.starts_with("url") {
            format!("{line}\ntoken_env = \"T\"")
        } else {
            format!("url = \"https://x.test\"\ntoken_env = \"T\"\n{line}")
        };
        match with_sink(&text) {
            Err(error @ Error::InvalidSetting { .. }) => {
                assert!(error.to_string().starts_with(field), "{error}");
            }
            other => panic!("{line}: expected InvalidSetting, got {other:?}"),
        }
    }
}

/// glm-5.3-flash's list price, as its order book quotes it.
const GLM_FLASH: crate::pricing::ListPrice = crate::pricing::ListPrice {
    prompt_per_1m: 0.15,
    completion_per_1m: 0.50,
};

/// A Surplus usage object from a seller that states no market cost: the
/// shape most production calls arrive in.
const SURPLUS_USAGE_UNPRICED: &str = r#"{"prompt_tokens":13,"completion_tokens":16,"total_tokens":29,"cost":0,"buyer_cost_micro":0}"#;

fn assert_close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("a figure");
    assert!((actual - expected).abs() < 1e-15, "{actual} != {expected}");
}

#[test]
fn a_surplus_answer_without_a_market_cost_is_priced_at_the_list_price() {
    let figures = figures(
        ProviderKind::Surplus,
        JSON,
        &completion(SURPLUS_USAGE_UNPRICED),
        None,
        Some(GLM_FLASH),
    );

    // 13 in and 16 out at 0.15 / 0.50 per million: the same 0.00000995 a
    // seller that states the cost reports for the same call.
    assert_close(figures.market_usd, 0.000_009_95);
    assert_eq!(figures.market_source, Some(MarketSource::ListPrice));
    // Only the market cost is derived; the charge is still what was reported.
    assert_eq!(figures.actual_usd, Some(0.0));
}

#[test]
fn a_reported_market_cost_wins_over_the_list_price() {
    let list = crate::pricing::ListPrice {
        prompt_per_1m: 9.0,
        completion_per_1m: 9.0,
    };
    let figures = figures(
        ProviderKind::Surplus,
        JSON,
        &completion(SURPLUS_USAGE),
        None,
        Some(list),
    );
    assert_eq!(figures.market_usd, Some(0.000_009_95));
    assert_eq!(figures.market_source, Some(MarketSource::Reported));
}

#[test]
fn an_unknown_list_price_leaves_the_market_cost_unknown() {
    let figures = figures(
        ProviderKind::Surplus,
        JSON,
        &completion(SURPLUS_USAGE_UNPRICED),
        None,
        None,
    );
    assert_eq!(figures.market_usd, None);
    assert_eq!(figures.market_source, None);
}

#[test]
fn a_missing_token_count_is_not_priced_as_zero() {
    let output_unknown = completion(r#"{"prompt_tokens":13,"buyer_cost_micro":0}"#);
    let priced = figures(
        ProviderKind::Surplus,
        JSON,
        &output_unknown,
        None,
        Some(GLM_FLASH),
    );
    assert_eq!(priced.market_usd, None);
    assert_eq!(priced.market_source, None);

    let nothing = figures(ProviderKind::Surplus, JSON, b"{}", None, Some(GLM_FLASH));
    assert_eq!(nothing.market_usd, None);
}

#[test]
fn only_surplus_is_priced_at_the_list_price() {
    // OpenRouter keeps its own `usage.cost`, and a call without one stays
    // unknown rather than borrowing a price table entry.
    let usage = r#"{"prompt_tokens":13,"completion_tokens":16}"#;
    for kind in [
        ProviderKind::OpenRouter,
        ProviderKind::Mistral,
        ProviderKind::Venice,
    ] {
        let unpriced = figures(kind, JSON, &completion(usage), None, Some(GLM_FLASH));
        assert_eq!(unpriced.market_usd, None, "{kind:?}");
        assert_eq!(unpriced.market_source, None, "{kind:?}");
    }
    let reported = r#"{"prompt_tokens":13,"completion_tokens":16,"cost":0.5}"#;
    let priced = figures(
        ProviderKind::OpenRouter,
        JSON,
        &completion(reported),
        None,
        Some(GLM_FLASH),
    );
    assert_eq!(priced.market_usd, Some(0.5));
    assert_eq!(priced.market_source, Some(MarketSource::Reported));
}

#[test]
fn a_stream_without_a_market_cost_is_priced_at_the_list_price() {
    let stream = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"hi\"}}}}]}}\n\n\
         data: {{\"choices\":[],\"usage\":{SURPLUS_USAGE_UNPRICED}}}\n\n\
         data: [DONE]\n\n"
    );
    let figures = figures(
        ProviderKind::Surplus,
        SSE,
        stream.as_bytes(),
        None,
        Some(GLM_FLASH),
    );
    assert_close(figures.market_usd, 0.000_009_95);
    assert_eq!(figures.market_source, Some(MarketSource::ListPrice));
}

#[test]
fn the_market_source_travels_with_the_market_cost_when_merging() {
    let reported = Figures {
        market_usd: Some(1.0),
        market_source: Some(MarketSource::Reported),
        ..Figures::default()
    };
    let silent = Figures {
        prompt_tokens: Some(2),
        ..Figures::default()
    };
    let merged = reported.merged(silent);
    assert_eq!(merged.market_usd, Some(1.0));
    assert_eq!(merged.market_source, Some(MarketSource::Reported));
    assert_eq!(merged.prompt_tokens, Some(2));

    let derived = Figures {
        market_usd: Some(2.0),
        market_source: Some(MarketSource::ListPrice),
        ..Figures::default()
    };
    assert_eq!(
        reported.merged(derived).market_source,
        Some(MarketSource::ListPrice)
    );
}

#[test]
fn a_derived_market_cost_is_posted_and_named_list_price() {
    let figures = figures(
        ProviderKind::Surplus,
        JSON,
        &completion(SURPLUS_USAGE_UNPRICED),
        None,
        Some(GLM_FLASH),
    );
    let record = Record::new(
        "memory-flash",
        0,
        "surplus",
        "glm-5.3-flash",
        "chat",
        figures,
    );
    log(&record);
    let json = serde_json::to_value(&record).unwrap();
    assert_eq!(json["market_source"], "list_price");
    assert_eq!(MarketSource::Reported.as_str(), "reported");
    assert_eq!(
        serde_json::to_value(MarketSource::Reported).unwrap(),
        "reported"
    );
}
