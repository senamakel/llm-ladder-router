//! Tests for refusing requests by their system prompt.

// As in every other test module here: panicking helpers are the clearest way
// to assert in a test, and a failure is the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use serde_json::json;

fn refuse(prefixes: &[&str]) -> Refuse {
    Refuse {
        system_prefixes: prefixes.iter().map(|p| (*p).to_string()).collect(),
    }
}

const SYNTH: &str = "You are a synthesis engine.";

#[test]
fn refuses_a_chat_request_whose_system_prompt_starts_with_a_prefix() {
    let body = json!({"model": "m", "messages": [
        {"role": "system", "content": "You are a synthesis engine. Given a group..."},
        {"role": "user", "content": "go"}
    ]});
    assert_eq!(
        matching_rule(&body, Wire::OpenAi, &refuse(&[SYNTH])),
        Some(0)
    );
}

#[test]
fn names_the_first_matching_rule() {
    let body = json!({"messages": [{"role": "system", "content": "You are a synthesis engine."}]});
    let rules = refuse(&["You are an extraction engine", SYNTH]);
    assert_eq!(matching_rule(&body, Wire::OpenAi, &rules), Some(1));
}

#[test]
fn ignores_leading_whitespace_on_both_sides() {
    let body =
        json!({"messages": [{"role": "system", "content": "\n  You are a synthesis engine."}]});
    assert_eq!(
        matching_rule(
            &body,
            Wire::OpenAi,
            &refuse(&["  You are a synthesis engine"])
        ),
        Some(0)
    );
}

#[test]
fn leaves_other_system_prompts_alone() {
    let body = json!({"messages": [
        {"role": "system", "content": "You are a precise extraction engine."},
        {"role": "user", "content": "You are a synthesis engine."}
    ]});
    assert_eq!(matching_rule(&body, Wire::OpenAi, &refuse(&[SYNTH])), None);
}

#[test]
fn matches_only_the_start_of_the_prompt() {
    let body = json!({"messages": [
        {"role": "system", "content": "Note: You are a synthesis engine."}
    ]});
    assert_eq!(matching_rule(&body, Wire::OpenAi, &refuse(&[SYNTH])), None);
}

#[test]
fn is_case_sensitive() {
    let body = json!({"messages": [{"role": "system", "content": "you are a synthesis engine."}]});
    assert_eq!(matching_rule(&body, Wire::OpenAi, &refuse(&[SYNTH])), None);
}

#[test]
fn reads_developer_messages_and_content_parts() {
    let body = json!({"messages": [{"role": "developer", "content": [
        {"type": "text", "text": "You are a synth"},
        {"type": "text", "text": "esis engine. Go."}
    ]}]});
    assert_eq!(
        matching_rule(&body, Wire::OpenAi, &refuse(&[SYNTH])),
        Some(0)
    );
}

#[test]
fn reads_the_anthropic_system_field() {
    let as_string = json!({"system": "You are a synthesis engine. Go."});
    let as_blocks = json!({"system": [{"type": "text", "text": "You are a synthesis engine."}]});
    let rules = refuse(&[SYNTH]);
    assert_eq!(matching_rule(&as_string, Wire::Anthropic, &rules), Some(0));
    assert_eq!(matching_rule(&as_blocks, Wire::Anthropic, &rules), Some(0));
}

#[test]
fn reads_responses_instructions_and_system_input() {
    let rules = refuse(&[SYNTH]);
    let instructions = json!({"instructions": "You are a synthesis engine.", "input": "go"});
    let input = json!({"input": [{"role": "system", "content": "You are a synthesis engine."}]});
    assert_eq!(
        matching_rule(&instructions, Wire::Responses, &rules),
        Some(0)
    );
    assert_eq!(matching_rule(&input, Wire::Responses, &rules), Some(0));
}

#[test]
fn never_refuses_a_surface_without_a_system_prompt() {
    let body =
        json!({"input": ["You are a synthesis engine."], "prompt": "You are a synthesis engine."});
    let rules = refuse(&[SYNTH]);
    for wire in [Wire::Embeddings, Wire::Images, Wire::Video] {
        assert_eq!(matching_rule(&body, wire, &rules), None);
    }
}

#[test]
fn refuses_nothing_when_unconfigured() {
    let body = json!({"messages": [{"role": "system", "content": "You are a synthesis engine."}]});
    assert_eq!(matching_rule(&body, Wire::OpenAi, &Refuse::default()), None);
}

#[test]
fn ignores_a_blank_prefix_rather_than_refusing_everything() {
    let body = json!({"messages": [{"role": "system", "content": "anything at all"}]});
    assert_eq!(
        matching_rule(&body, Wire::OpenAi, &refuse(&["", "   "])),
        None
    );
}

#[test]
fn tolerates_malformed_messages() {
    let body = json!({"messages": [
        {"role": "system"},
        {"role": "system", "content": 42},
        "not an object"
    ]});
    assert_eq!(matching_rule(&body, Wire::OpenAi, &refuse(&[SYNTH])), None);
    assert_eq!(
        matching_rule(
            &json!({"messages": "nope"}),
            Wire::OpenAi,
            &refuse(&[SYNTH])
        ),
        None
    );
}

#[test]
fn parses_from_toml_and_rejects_unknown_fields() {
    let parsed: Refuse = toml::from_str(r#"system_prefixes = ["You are a synthesis engine"]"#)
        .expect("a list of prefixes parses");
    assert_eq!(parsed.system_prefixes.len(), 1);
    assert!(toml::from_str::<Refuse>(r#"prefixes = ["x"]"#).is_err());
}
