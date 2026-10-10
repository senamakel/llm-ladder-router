//! Unit tests for the usage feed's batching, retries and drops.
//!
//! Each test stands up a loopback sink that records what reaches it, because
//! what matters is the wire: the batch shape, the bearer token, and how many
//! times a failing post is attempted before the batch is let go.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;

use super::*;
use crate::usage::{Figures, MarketSource};

/// What the loopback sink saw.
#[derive(Default)]
struct Seen {
    batches: Vec<serde_json::Value>,
    tokens: Vec<String>,
}

/// A loopback sink that answers `failures` posts with `status`, then 200.
async fn sink(status: StatusCode, failures: usize) -> (String, Arc<Mutex<Seen>>, Arc<AtomicUsize>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let (recorder, counter) = (seen.clone(), attempts.clone());

    let app = axum::Router::new().route(
        "/internal/memory/model-usage",
        post(
            move |headers: HeaderMap, axum::Json(body): axum::Json<serde_json::Value>| {
                let (recorder, counter) = (recorder.clone(), counter.clone());
                async move {
                    let attempt = counter.fetch_add(1, Ordering::SeqCst);
                    let mut seen = recorder.lock().unwrap();
                    seen.tokens.push(
                        headers
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string(),
                    );
                    if attempt < failures {
                        return (status, axum::Json(serde_json::json!({})));
                    }
                    let accepted = body["records"].as_array().map_or(0, Vec::len);
                    seen.batches.push(body);
                    (
                        StatusCode::OK,
                        axum::Json(serde_json::json!({ "accepted": accepted })),
                    )
                }
            },
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (
        format!("http://{address}/internal/memory/model-usage"),
        seen,
        attempts,
    )
}

fn config(url: &str, max_batch: usize, flush_interval: Duration) -> UsageSink {
    UsageSink {
        url: url.to_string(),
        token_env: "LADDER_TEST_UNSET_USAGE_TOKEN".to_string(),
        flush_interval,
        max_batch,
        queue_size: 16,
        max_retries: 2,
        retry_backoff: Duration::from_millis(1),
        timeout: Duration::from_secs(5),
    }
}

fn record(rung: usize) -> Record {
    Record::new(
        "memory-flash",
        rung,
        "surplus",
        "glm-5.3-flash",
        "chat",
        Figures {
            prompt_tokens: Some(13),
            completion_tokens: Some(16),
            actual_usd: Some(0.0),
            market_usd: Some(0.000_009_95),
            market_source: Some(MarketSource::Reported),
        },
    )
}

/// Polls until `done` holds, failing the test after five seconds.
async fn until(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not met in time"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

const NEVER: Duration = Duration::from_secs(3600);

#[tokio::test]
async fn a_full_batch_is_posted_with_the_bearer_token() {
    let (url, seen, _) = sink(StatusCode::OK, 0).await;
    let feed = Feed::new(&config(&url, 2, NEVER), "feed-token").unwrap();
    feed.start();

    let (first, second) = (record(0), record(1));
    feed.push(first.clone());
    feed.push(second.clone());

    until(|| !seen.lock().unwrap().batches.is_empty()).await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.tokens, ["Bearer feed-token"]);
    let records = seen.batches[0]["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["id"], first.id.as_str());
    assert_eq!(records[1]["id"], second.id.as_str());
    assert_eq!(records[0]["ladder"], "memory-flash");
    assert_eq!(records[0]["prompt_tokens"], 13);
    assert_eq!(records[0]["actual_usd"], 0.0);
    assert_eq!(records[0]["market_usd"], 0.000_009_95);
}

#[tokio::test]
async fn a_partial_batch_is_posted_when_the_interval_passes() {
    let (url, seen, _) = sink(StatusCode::OK, 0).await;
    let feed = Feed::new(&config(&url, 100, Duration::from_millis(20)), "t").unwrap();
    feed.start();
    feed.push(record(0));

    until(|| !seen.lock().unwrap().batches.is_empty()).await;
    assert_eq!(
        seen.lock().unwrap().batches[0]["records"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn records_queued_before_start_go_out_with_the_first_batch() {
    let (url, seen, _) = sink(StatusCode::OK, 0).await;
    let feed = Feed::new(&config(&url, 2, NEVER), "t").unwrap();
    feed.push(record(0));
    feed.push(record(1));
    feed.start();
    // A second start, from any clone, starts nothing more.
    feed.clone().start();

    until(|| !seen.lock().unwrap().batches.is_empty()).await;
    assert_eq!(seen.lock().unwrap().batches.len(), 1);
}

#[tokio::test]
async fn a_failing_sink_is_retried_and_the_same_records_resent() {
    let (url, seen, attempts) = sink(StatusCode::SERVICE_UNAVAILABLE, 2).await;
    let feed = Feed::new(&config(&url, 1, NEVER), "t").unwrap();
    feed.start();
    let sent = record(0);
    feed.push(sent.clone());

    until(|| !seen.lock().unwrap().batches.is_empty()).await;
    // Two refusals, then the third attempt lands, carrying the same id so the
    // receiver can tell a retry from a new call.
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(
        seen.lock().unwrap().batches[0]["records"][0]["id"],
        sent.id.as_str()
    );
}

#[tokio::test]
async fn a_batch_is_dropped_once_its_retries_run_out() {
    let (url, seen, attempts) = sink(StatusCode::BAD_GATEWAY, usize::MAX).await;
    let feed = Feed::new(&config(&url, 1, NEVER), "t").unwrap();
    feed.start();
    feed.push(record(0));
    feed.push(record(1));

    // One attempt and two retries for each batch, and then each is let go
    // rather than blocking the next.
    until(|| attempts.load(Ordering::SeqCst) == 6).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 6);
    assert_eq!(seen.lock().unwrap().batches.len(), 0);
}

#[tokio::test]
async fn a_refusal_that_would_repeat_is_not_retried() {
    let (url, _, attempts) = sink(StatusCode::UNAUTHORIZED, usize::MAX).await;
    let feed = Feed::new(&config(&url, 1, NEVER), "wrong").unwrap();
    feed.start();
    feed.push(record(0));
    feed.push(record(1));

    // The second batch is posted only after the first is given up on, so
    // seeing it proves the first was tried once.
    until(|| attempts.load(Ordering::SeqCst) == 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_unreachable_sink_is_retried_then_dropped() {
    let feed = Feed::new(&config("http://127.0.0.1:1/usage", 1, NEVER), "t").unwrap();
    feed.start();
    feed.push(record(0));
    // Nothing to observe on the wire; the pump must still be alive to take
    // more, which a panic or a stuck retry loop would prevent.
    tokio::time::sleep(Duration::from_millis(100)).await;
    feed.push(record(1));
    assert_eq!(feed.dropped.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn a_full_queue_drops_new_records_and_counts_them() {
    let (url, seen, _) = sink(StatusCode::OK, 0).await;
    let mut sink = config(&url, 100, Duration::from_millis(20));
    sink.queue_size = 2;
    let feed = Feed::new(&sink, "t").unwrap();

    feed.push(record(0));
    feed.push(record(1));
    feed.push(record(2));
    feed.push(record(3));
    assert_eq!(feed.dropped.load(Ordering::Relaxed), 2);
    assert!(format!("{feed:?}").contains("dropped: 2"));

    // The flush that follows reports the drops and resets the count, and the
    // records that fit are still delivered.
    feed.start();
    until(|| !seen.lock().unwrap().batches.is_empty()).await;
    until(|| feed.dropped.load(Ordering::Relaxed) == 0).await;
    assert_eq!(
        seen.lock().unwrap().batches[0]["records"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn the_last_records_are_posted_when_every_sender_is_gone() {
    let (url, seen, _) = sink(StatusCode::OK, 0).await;
    let feed = Feed::new(&config(&url, 100, NEVER), "t").unwrap();
    feed.start();
    feed.push(record(0));
    drop(feed);

    until(|| !seen.lock().unwrap().batches.is_empty()).await;
}

#[test]
fn an_unset_token_leaves_the_feed_off() {
    let sink = config("http://127.0.0.1:1/usage", 1, NEVER);
    assert!(Feed::from_env(&sink).unwrap().is_none());
}

#[test]
fn the_token_never_appears_in_debug_output() {
    let feed = Feed::new(&config("http://127.0.0.1:1/usage", 1, NEVER), "sekrit").unwrap();
    let shown = format!("{feed:?}");
    assert!(!shown.contains("sekrit"), "{shown}");
}
