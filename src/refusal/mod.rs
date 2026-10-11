//! Refusing requests whose system prompt starts with a configured prefix.
//!
//! Some callers run background jobs whose requests the operator does not want
//! sent upstream at all — a job that should have been switched off in the
//! caller, say, and must stay off even if the caller's own setting drifts back
//! on. Those requests are recognisable by a fixed instruction at the start of
//! their system prompt, so the router can turn them away by that alone.
//!
//! The operator lists the prefixes (`[refuse] system_prefixes = [...]`). A
//! chat request whose system prompt starts with one is answered `403` before a
//! ladder is walked, so nothing reaches a provider and nothing is billed.
//!
//! ## What is read, and what is recorded
//!
//! Only the system prompt is examined, and only its start. The log line for a
//! refusal names the ladder and the index of the rule that matched — never the
//! prompt, the rest of the request, or the prefix text itself, which an
//! operator already has in their own configuration.
//!
//! Embeddings, image and video requests carry no system prompt and are never
//! refused here.

pub mod types;

pub use types::Refuse;

use crate::provider::Wire;

/// The text of one message `content`: a plain string, or the concatenated
/// `text` parts of an array of content blocks.
fn content_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(parts) => {
            let text: String = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                .collect();
            Some(text)
        }
        _ => None,
    }
}

/// The system-role entries of a `messages` or `input` array.
fn system_messages(items: Option<&serde_json::Value>) -> Vec<String> {
    items
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| {
            matches!(
                item.get("role").and_then(serde_json::Value::as_str),
                Some("system" | "developer")
            )
        })
        .filter_map(|item| item.get("content").and_then(content_text))
        .collect()
}

/// Every system prompt a request carries on its wire format.
fn system_prompts(body: &serde_json::Value, wire: Wire) -> Vec<String> {
    match wire {
        Wire::OpenAi => system_messages(body.get("messages")),
        Wire::Anthropic => body
            .get("system")
            .and_then(content_text)
            .into_iter()
            .collect(),
        Wire::Responses => {
            let mut prompts: Vec<String> = body
                .get("instructions")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .into_iter()
                .collect();
            prompts.extend(system_messages(body.get("input")));
            prompts
        }
        Wire::Embeddings | Wire::Images | Wire::Video => Vec::new(),
    }
}

/// The index of the first rule a request matches, if any.
///
/// `None` when nothing is configured, when the wire carries no system prompt,
/// or when no system prompt starts with a configured prefix.
#[must_use]
pub fn matching_rule(body: &serde_json::Value, wire: Wire, refuse: &Refuse) -> Option<usize> {
    // Nothing configured means no prompt is read at all.
    refuse.prefixes().next()?;
    let prompts = system_prompts(body, wire);
    refuse.prefixes().find_map(|(index, prefix)| {
        prompts
            .iter()
            .any(|prompt| prompt.trim_start().starts_with(prefix))
            .then_some(index)
    })
}

#[cfg(test)]
#[path = "test.rs"]
mod test;
