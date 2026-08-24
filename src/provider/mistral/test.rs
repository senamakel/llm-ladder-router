//! Unit tests for the direct Mistral dialect.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::ladder::Chosen;

fn chosen() -> Chosen {
    Chosen {
        rung: 0,
        provider: "mistral".to_string(),
        model: "labs-leanstral-1-5".to_string(),
        cap_per_1m: None,
        admitted: Vec::new(),
        cheapest_per_1m: None,
        min_discount_pct: None,
        prefer: Vec::new(),
        reasoning_effort: None,
        score_multiplier: 1.0,
        score: None,
    }
}

#[test]
fn only_the_model_is_rewritten() {
    let mut body = serde_json::json!({ "model": "scribe", "messages": [], "temperature": 0 });

    apply_routing(&mut body, &chosen());

    assert_eq!(body["model"], "labs-leanstral-1-5");
    // No ceiling travels: there is one seller, so there is nothing to filter.
    assert!(body.get("provider").is_none());
    assert_eq!(body["temperature"], 0);
}

#[test]
fn the_chat_and_embeddings_surfaces_serve_and_the_others_do_not() {
    assert!(serves(Wire::OpenAi));
    assert!(serves(Wire::Embeddings));
    assert!(!serves(Wire::Anthropic));
    // Mistral answers `/v1/responses` with a 404, so that surface is declined
    // here rather than relayed to the chat-completions endpoint.
    assert!(!serves(Wire::Responses));
    assert_eq!(inference_path(Wire::OpenAi), "/v1/chat/completions");
    assert_eq!(inference_path(Wire::Embeddings), "/v1/embeddings");
}

/// A rate limit or an outage advances the ladder; a request the caller got
/// wrong is returned as-is rather than replayed at every rung.
#[test]
fn upstream_failures_advance_and_caller_errors_do_not() {
    assert_eq!(
        classify(reqwest::StatusCode::TOO_MANY_REQUESTS, b""),
        Disposition::Advance
    );
    assert_eq!(
        classify(reqwest::StatusCode::INTERNAL_SERVER_ERROR, b""),
        Disposition::Advance
    );
    assert_eq!(
        classify(reqwest::StatusCode::BAD_REQUEST, b""),
        Disposition::CallerError
    );
    assert_eq!(classify(reqwest::StatusCode::OK, b""), Disposition::Served);
}
