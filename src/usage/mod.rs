//! What each served call cost, read from the upstream's own answer.
//!
//! After a rung serves, the router reads the response it is relaying for the
//! token counts and the cost the marketplace reported, logs one line, and —
//! when `[usage_sink]` is configured — queues a [`Record`] for the background
//! [`Feed`] to post. None of this sits on the response path: the caller's
//! answer is built first and handed back unchanged, and the reading happens on
//! a task of its own.
//!
//! ## Where the numbers come from
//!
//! Each marketplace reports cost in its own words, so the reading branches on
//! [`ProviderKind`]:
//!
//! - **Surplus** charges `usage.buyer_cost_micro` (integer micro-USD), with the
//!   `x-si-buyer-cost-micro` response header as a fallback when the body
//!   carries none, and states the market price of the same call as
//!   `usage.cost_details.upstream_inference_cost` (USD). Its `usage.cost` is
//!   not either of those and is ignored.
//! - **`OpenRouter`** reports one `usage.cost` (USD), which is both what was
//!   charged and the market price.
//! - **Direct providers** report tokens only.
//!
//! Token counts are read under either spelling: `prompt_tokens` /
//! `completion_tokens` (`OpenAI` chat and embeddings) or `input_tokens` /
//! `output_tokens` (Anthropic Messages and `OpenAI` Responses).
//!
//! A figure the response does not carry is `None`, and is posted as `null`.
//! The router never estimates one from a price list.
//!
//! ## Streams
//!
//! A streamed answer reports usage in its last events, not up front. The
//! [`SseTap`] reads the relayed bytes chunk by chunk, keeps no more than one
//! line at a time, and only parses a `data:` line that mentions `usage`. It is
//! fed the same bytes the caller receives, after they have been handed over.
//! The router currently reads an upstream answer in full before relaying it
//! (see `Client::infer`), so the tap is given that body in one piece; it adds
//! no buffering of its own and would sit unchanged on a chunked relay.
//!
//! ## What is never recorded
//!
//! Routing facts and figures only. No part of a prompt or a completion is
//! logged, queued or posted.

mod sink;
pub mod types;

pub use sink::Feed;
pub use types::{Figures, Record, UsageSink};

use crate::config::ProviderKind;

/// The longest SSE line the tap will hold. A usage event is a few hundred
/// bytes; a longer line is a content delta the tap has no use for, and is
/// skipped rather than buffered without bound.
const MAX_LINE: usize = 1 << 20;

/// Reads the figures out of one relayed upstream answer.
///
/// `content_type` decides how the body is read: `text/event-stream` goes
/// through an [`SseTap`], anything else is read as one JSON document.
/// `buyer_cost_micro` is the value of Surplus's `x-si-buyer-cost-micro`
/// header, used only when the body names no charge of its own.
#[must_use]
pub fn figures(
    kind: ProviderKind,
    content_type: Option<&str>,
    body: &[u8],
    buyer_cost_micro: Option<f64>,
) -> Figures {
    let streamed = content_type.is_some_and(|value| value.contains("text/event-stream"));
    let mut figures = if streamed {
        let mut tap = SseTap::new(kind);
        tap.feed(body);
        tap.finish()
    } else {
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .as_ref()
            .and_then(usage_of)
            .map(|usage| read_usage(kind, usage))
            .unwrap_or_default()
    };
    if kind == ProviderKind::Surplus && figures.actual_usd.is_none() {
        figures.actual_usd = buyer_cost_micro.and_then(micro_to_usd);
    }
    figures
}

/// Writes the one log line a served call gets.
///
/// Absent figures are left out of the line rather than written as zero.
pub fn log(record: &Record) {
    tracing::info!(
        ladder = %record.ladder,
        rung = record.rung,
        provider = %record.provider,
        model = %record.model,
        surface = %record.surface,
        prompt_tokens = record.prompt_tokens,
        completion_tokens = record.completion_tokens,
        actual_usd = record.actual_usd,
        market_usd = record.market_usd,
        "usage recorded"
    );
}

/// Reads usage figures out of a server-sent event stream as it passes.
///
/// Fed in arbitrary chunks — a line may be split across any number of them —
/// and holds at most one partial line. Every `data:` event that carries a
/// usage object is merged into the running figures, later events winning
/// field by field, so the final usage chunk decides what it reports and an
/// earlier one fills in what it leaves out.
#[derive(Debug)]
pub struct SseTap {
    kind: ProviderKind,
    line: Vec<u8>,
    /// Set while discarding the rest of a line that outgrew [`MAX_LINE`].
    overflowed: bool,
    figures: Figures,
}

impl SseTap {
    /// A tap for a stream from a provider of the given kind.
    #[must_use]
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            line: Vec::new(),
            overflowed: false,
            figures: Figures::default(),
        }
    }

    /// Reads the next chunk of the stream.
    pub fn feed(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        while let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
            self.hold(&rest[..end]);
            self.end_line();
            rest = &rest[end + 1..];
        }
        self.hold(rest);
    }

    /// The figures the stream reported, once it has ended.
    #[must_use]
    pub fn finish(mut self) -> Figures {
        // A stream need not end on a newline.
        self.end_line();
        self.figures
    }

    fn hold(&mut self, part: &[u8]) {
        if self.overflowed {
            return;
        }
        if self.line.len() + part.len() > MAX_LINE {
            self.overflowed = true;
            self.line = Vec::new();
            return;
        }
        self.line.extend_from_slice(part);
    }

    fn end_line(&mut self) {
        if !self.overflowed
            && let Some(usage) = sse_usage(self.kind, &self.line)
        {
            self.figures = self.figures.merged(usage);
        }
        self.line.clear();
        self.overflowed = false;
    }
}

/// The figures in one SSE line, if it is a `data:` event carrying usage.
fn sse_usage(kind: ProviderKind, line: &[u8]) -> Option<Figures> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let data = line.strip_prefix(b"data:")?;
    // Only a line that names usage is worth a JSON parse; every content delta
    // in the stream is skipped on a byte scan.
    if !data.windows(7).any(|window| window == b"\"usage\"") {
        return None;
    }
    let event = serde_json::from_slice::<serde_json::Value>(data).ok()?;
    usage_of(&event).map(|usage| read_usage(kind, usage))
}

/// The usage object of a response body or stream event.
///
/// Top-level `usage` in a chat completion, an embeddings answer, a final chat
/// chunk and an Anthropic `message_delta`; `response.usage` in a Responses
/// event; `message.usage` in an Anthropic `message_start`.
fn usage_of(value: &serde_json::Value) -> Option<&serde_json::Value> {
    [
        value.get("usage"),
        value.get("response").and_then(|inner| inner.get("usage")),
        value.get("message").and_then(|inner| inner.get("usage")),
    ]
    .into_iter()
    .flatten()
    .find(|usage| usage.is_object())
}

/// Reads one usage object in the dialect of the provider that wrote it.
fn read_usage(kind: ProviderKind, usage: &serde_json::Value) -> Figures {
    let count = |names: [&str; 2]| {
        names
            .into_iter()
            .find_map(|name| usage.get(name).and_then(serde_json::Value::as_u64))
    };
    let (actual_usd, market_usd) = match kind {
        ProviderKind::Surplus => (
            usage
                .get("buyer_cost_micro")
                .and_then(serde_json::Value::as_f64)
                .and_then(micro_to_usd),
            usage
                .get("cost_details")
                .and_then(|details| details.get("upstream_inference_cost"))
                .and_then(serde_json::Value::as_f64)
                .and_then(dollars),
        ),
        ProviderKind::OpenRouter => {
            let cost = usage
                .get("cost")
                .and_then(serde_json::Value::as_f64)
                .and_then(dollars);
            (cost, cost)
        }
        ProviderKind::Mistral | ProviderKind::Venice => (None, None),
    };
    Figures {
        prompt_tokens: count(["prompt_tokens", "input_tokens"]),
        completion_tokens: count(["completion_tokens", "output_tokens"]),
        actual_usd,
        market_usd,
    }
}

/// A reported amount of money, or `None` when it is not one.
fn dollars(value: f64) -> Option<f64> {
    (value.is_finite() && value >= 0.0).then_some(value)
}

/// Micro-USD as USD.
fn micro_to_usd(micro: f64) -> Option<f64> {
    dollars(micro).map(|micro| micro / 1_000_000.0)
}

#[cfg(test)]
mod test;
