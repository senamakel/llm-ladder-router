//! Relaying a served event stream as it arrives.
//!
//! A buffered stream costs a caller nothing in total time and everything in
//! perceived time: every chunk lands at once, when the last token was
//! generated. Measured 2026-10-12 through the `flash` ladder, all 80 chunks of
//! a seven-second answer arrived in the same instant, so an agent harness saw
//! a frozen turn for the whole call and could not stop one early.
//!
//! The ladder decides failover on the status alone, so nothing is lost by
//! relaying a 2xx stream as it comes: the first chunk is read before the
//! response is handed back, which is what lets the sub-provider it names be
//! stamped on the headers and pinned like a buffered answer's, and every later
//! chunk passes straight through. The usage figures are read off the same
//! bytes on the way past and recorded when the stream ends — or when the
//! caller hangs up, in which case they cover what was delivered.

use axum::body::Body;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;

use crate::config::ProviderKind;
use crate::provider::Streaming;
use crate::usage::{Feed, Figures, Record, SseTap};

use super::types::HEADER_SUB_PROVIDER;

/// Where one served call's usage figures go once they are known.
#[derive(Debug, Clone)]
pub(super) struct Meter {
    /// The usage feed, when one is configured.
    pub(super) feed: Option<Feed>,
    /// The ladder's own name, not the alias the caller used.
    pub(super) ladder: String,
    /// The surface name the usage record carries.
    pub(super) surface: &'static str,
    /// The rung that served.
    pub(super) rung: usize,
    /// The provider that served.
    pub(super) provider: String,
    /// The model that served.
    pub(super) model: String,
}

impl Meter {
    /// Logs one served call's figures and hands them to the feed.
    pub(super) fn record(&self, figures: Figures) {
        let record = Record::new(
            &self.ladder,
            self.rung,
            &self.provider,
            &self.model,
            self.surface,
            figures,
        );
        crate::usage::log(&record);
        if let Some(feed) = &self.feed {
            feed.push(record);
        }
    }
}

/// Reads usage off a stream as it passes, and records it when dropped.
///
/// Recording on drop rather than on the final chunk is what covers a caller
/// that disconnects halfway: the body is dropped either way, and the figures
/// then describe what was actually relayed.
struct Metered {
    tap: Option<SseTap>,
    meter: Meter,
}

impl Metered {
    fn feed(&mut self, chunk: &[u8]) {
        if let Some(tap) = &mut self.tap {
            tap.feed(chunk);
        }
    }
}

impl Drop for Metered {
    fn drop(&mut self) {
        if let Some(tap) = self.tap.take() {
            self.meter.record(tap.finish());
        }
    }
}

/// Turns a served upstream stream into a response that relays it as it
/// arrives.
///
/// The first chunk is awaited before returning, so the sub-provider it names
/// can be stamped on the response — the header the session pin reads — and
/// a stream that breaks before its first byte still reaches the caller as the
/// broken stream it is rather than as a hang.
pub(super) async fn relay(open: Streaming, kind: ProviderKind, meter: Meter) -> Response {
    let Streaming { head, mut response } = open;
    let first = response.chunk().await;
    let served_by = match &first {
        Ok(Some(chunk)) => crate::provider::served_by_in_stream(chunk),
        _ => None,
    };

    let mut metered = Metered {
        tap: Some(SseTap::new(kind)),
        meter,
    };
    let body = futures_util::stream::iter(first.transpose())
        .chain(response.bytes_stream())
        .map(move |chunk| {
            if let Ok(bytes) = &chunk {
                metered.feed(bytes);
            }
            chunk
        });

    let mut builder = Response::builder().status(head.status);
    if let Some(content_type) = &head.content_type
        && let Ok(value) = HeaderValue::from_str(content_type)
    {
        builder = builder.header(axum::http::header::CONTENT_TYPE, value);
    }
    if let Some(served_by) = &served_by
        && let Ok(value) = HeaderValue::from_str(served_by)
    {
        builder = builder.header(HEADER_SUB_PROVIDER, value);
    }
    builder
        .body(Body::from_stream(body))
        .map_or_else(|_| unrelayable(), IntoResponse::into_response)
}

/// The answer when a relayed stream's head cannot be built.
fn unrelayable() -> Response {
    (
        axum::http::StatusCode::BAD_GATEWAY,
        "upstream stream could not be relayed",
    )
        .into_response()
}

#[cfg(test)]
#[path = "stream_test.rs"]
mod test;
