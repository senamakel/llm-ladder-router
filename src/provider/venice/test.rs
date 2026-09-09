//! Unit tests for the direct Venice dialect.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::ladder::Chosen;

fn chosen() -> Chosen {
    Chosen {
        rung: 1,
        provider: "venice".to_string(),
        model: "venice-uncensored-1-2".to_string(),
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
fn the_model_is_rewritten_and_the_house_system_prompt_is_declined() {
    let mut body = serde_json::json!({ "model": "uncensored", "messages": [], "temperature": 0 });

    apply_routing(&mut body, &chosen(), Wire::OpenAi);

    assert_eq!(body["model"], "venice-uncensored-1-2");
    assert_eq!(
        body["venice_parameters"]["include_venice_system_prompt"],
        false
    );
    // No ceiling travels: there is one seller, so there is nothing to filter.
    assert!(body.get("provider").is_none());
    assert_eq!(body["temperature"], 0);
}

/// The default is for callers who never heard of the field. One who did keeps
/// what they sent, and keeps the rest of the object with it.
#[test]
fn a_caller_who_set_venice_parameters_keeps_them() {
    let mut body = serde_json::json!({
        "model": "uncensored",
        "venice_parameters": {
            "include_venice_system_prompt": true,
            "character_slug": "some-character"
        }
    });

    apply_routing(&mut body, &chosen(), Wire::OpenAi);

    assert_eq!(
        body["venice_parameters"]["include_venice_system_prompt"],
        true
    );
    assert_eq!(
        body["venice_parameters"]["character_slug"],
        "some-character"
    );
}

/// A caller who put something other than an object there has sent Venice a
/// request Venice will reject. Rewriting it would answer a request they never
/// made; the 400 is theirs to see.
#[test]
fn a_non_object_venice_parameters_is_left_alone() {
    let mut body = serde_json::json!({ "model": "uncensored", "venice_parameters": "yes" });

    apply_routing(&mut body, &chosen(), Wire::OpenAi);

    assert_eq!(body["model"], "venice-uncensored-1-2");
    assert_eq!(body["venice_parameters"], "yes");
}

#[test]
fn a_body_that_is_not_an_object_is_left_alone() {
    let mut body = serde_json::json!([1, 2, 3]);

    apply_routing(&mut body, &chosen(), Wire::OpenAi);

    assert_eq!(body, serde_json::json!([1, 2, 3]));
}

#[test]
fn the_openai_surface_is_the_only_one() {
    assert!(serves(Wire::OpenAi));
    assert!(!serves(Wire::Anthropic));
    assert_eq!(inference_path(Wire::OpenAi), "/api/v1/chat/completions");
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
        classify(reqwest::StatusCode::SERVICE_UNAVAILABLE, b""),
        Disposition::Advance
    );
    assert_eq!(
        classify(reqwest::StatusCode::BAD_REQUEST, b""),
        Disposition::CallerError
    );
    assert_eq!(classify(reqwest::StatusCode::OK, b""), Disposition::Served);
}

/// Venice serves embeddings, and at its own `/api/v1` root rather than `/v1`.
///
/// Regression test for the `vectors` ladder having no usable rung: this
/// returned `false`, so an embeddings request was declined before the round
/// trip and the only working embeddings provider on the fleet was unreachable.
#[test]
fn serves_embeddings_at_the_venice_root() {
    assert!(serves(Wire::Embeddings));
    assert_eq!(inference_path(Wire::Embeddings), "/api/v1/embeddings");
}

/// An embeddings body gets the model rewrite and nothing else.
///
/// `venice_parameters` is a chat concept — there is no conversation to prepend
/// a system prompt to — and Venice's embeddings endpoint validates strictly, so
/// an unknown key would be a 422. That is precisely how `mistral-embed`
/// disqualified itself from this ladder, and it must not happen here.
#[test]
fn embeddings_body_carries_no_venice_parameters() {
    let mut body = serde_json::json!({"model": "placeholder", "input": "hello"});
    apply_routing(&mut body, &chosen(), Wire::Embeddings);
    assert_eq!(body["model"], serde_json::json!(chosen().model));
    assert!(
        body.get("venice_parameters").is_none(),
        "an embeddings request must not carry venice_parameters: {body}"
    );
}
