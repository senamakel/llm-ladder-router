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

use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;

use crate::config::ProviderKind;
use crate::provider::Streaming;
use crate::provider::surplus::cost::SseStamper;
use crate::usage::{Feed, Figures, Record, SseTap};

use super::types::HEADER_SUB_PROVIDER;

/// Where one served call's usage figures go once they are known.
#[derive(Debug, Clone)]
pub(super) struct Meter {
    /// The current marketplace price snapshots.
    pub(super) prices: std::sync::Arc<tokio::sync::RwLock<crate::pricing::PriceTable>>,
    /// How long a price snapshot remains usable.
    pub(super) stale_after: std::time::Duration,
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
    /// Prices and records one served call off the response's own task.
    pub(super) fn record(&self, kind: ProviderKind, figures: Figures) {
        let meter = self.clone();
        tokio::spawn(async move {
            let figures = if kind == ProviderKind::Surplus {
                let list_price = meter
                    .prices
                    .read()
                    .await
                    .get(&meter.provider, &meter.model)
                    .filter(|snapshot| !snapshot.is_stale(meter.stale_after))
                    .and_then(|snapshot| snapshot.list_price);
                figures.priced_at(list_price)
            } else {
                figures
            };
            let record = Record::new(
                &meter.ladder,
                meter.rung,
                &meter.provider,
                &meter.model,
                meter.surface,
                figures,
            );
            crate::usage::log(&record);
            if let Some(feed) = &meter.feed {
                feed.push(record);
            }
        });
    }
}

/// Reads usage off a stream as it passes, and records it when dropped.
///
/// Recording on drop rather than on the final chunk is what covers a caller
/// that disconnects halfway: the body is dropped either way, and the figures
/// then describe what was actually relayed.
struct Metered {
    kind: ProviderKind,
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
            self.meter.record(self.kind, tap.finish());
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
///
/// With a `stamper`, each line is passed through it on the way out so the
/// charge the marketplace names can be restated where clients read a cost;
/// see [`crate::provider::surplus::cost`].
pub(super) fn relay(
    open: Streaming,
    first: Option<Bytes>,
    kind: ProviderKind,
    meter: Meter,
    stamper: Option<SseStamper>,
) -> Response {
    let Streaming { head, response } = open;
    let served_by = first
        .as_deref()
        .and_then(crate::provider::served_by_in_stream);

    let mut metered = Metered {
        kind,
        tap: Some(SseTap::new(kind)),
        meter,
    };
    let stamper = Arc::new(Mutex::new(stamper));
    let tail = stamper.clone();
    let body = futures_util::stream::iter(first.map(Ok))
        .chain(response.bytes_stream())
        .map(move |chunk| {
            let bytes = chunk?;
            metered.feed(&bytes);
            Ok(restamp(&stamper, bytes))
        })
        // The stamper may hold the stream's last line; release it at the end.
        .chain(futures_util::stream::once(async move {
            let held = lock(&tail).take().map(SseStamper::finish);
            Ok::<_, reqwest::Error>(Bytes::from(held.unwrap_or_default()))
        }))
        .filter(|chunk| std::future::ready(!matches!(chunk, Ok(bytes) if bytes.is_empty())));

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

/// Reads a served stream's first chunk, before the caller has been sent
/// anything.
///
/// Until this returns the rung can still be stepped past: an upstream that
/// answers 200 and then goes silent, or drops the connection, is a rung that
/// failed, and the caller has nothing yet that a second rung's answer would
/// contradict. Once the first chunk is relayed that choice is gone, so it is
/// read here, inside the failover loop, and not inside the relay. `None` is a
/// stream that ended cleanly before sending anything.
pub(super) async fn first_chunk(open: &mut Streaming) -> reqwest::Result<Option<Bytes>> {
    open.response.chunk().await
}

/// Passes one chunk through the stamper, when there is one.
fn restamp(stamper: &Mutex<Option<SseStamper>>, bytes: Bytes) -> Bytes {
    match lock(stamper).as_mut() {
        Some(stamper) => Bytes::from(stamper.feed(&bytes)),
        None => bytes,
    }
}

/// The stamper's lock; a panic elsewhere cannot leave a line half-stamped
/// that matters more than relaying the rest.
fn lock(stamper: &Mutex<Option<SseStamper>>) -> std::sync::MutexGuard<'_, Option<SseStamper>> {
    stamper
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
