//! Unit tests for relaying a served stream as it arrives.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::routing::{get, post};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use super::*;
use crate::provider::Dispatched;
use crate::usage::UsageSink;

const FIRST: &str =
    "data: {\"provider\":\"Morph\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
const USAGE: &str =
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":13,\"completion_tokens\":16}}\n\n";

/// A loopback upstream whose event stream sends exactly the chunks the test
/// pushes, when the test pushes them.
async fn gated_upstream() -> (String, mpsc::UnboundedSender<&'static str>) {
    let (sender, receiver) = mpsc::unbounded_channel::<&'static str>();
    let receiver = Arc::new(tokio::sync::Mutex::new(Some(receiver)));
    let app = axum::Router::new().route(
        "/stream",
        get(move || {
            let receiver = receiver.clone();
            async move {
                let receiver = receiver.lock().await.take().unwrap();
                let chunks = futures_util::stream::unfold(receiver, |mut receiver| async move {
                    let chunk = receiver.recv().await?;
                    Some((Ok::<_, std::convert::Infallible>(chunk), receiver))
                });
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(chunks),
                )
            }
        }),
    );
    (serve(app).await, sender)
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// Opens the upstream stream the way the provider client would hand it over.
async fn opened(url: &str) -> Streaming {
    let response = reqwest::get(format!("{url}/stream")).await.unwrap();
    Streaming {
        head: Dispatched {
            status: response.status(),
            body: Vec::new(),
            served_by: None,
            content_type: Some("text/event-stream".to_string()),
            retry_after: None,
            buyer_cost_micro: None,
        },
        response,
    }
}

/// A loopback usage sink, and a feed posting every record to it at once.
async fn sink() -> (Feed, Arc<Mutex<Vec<serde_json::Value>>>) {
    let batches = Arc::new(Mutex::new(Vec::new()));
    let recorder = batches.clone();
    let app = axum::Router::new().route(
        "/usage",
        post(move |Json(body): Json<serde_json::Value>| {
            let recorder = recorder.clone();
            async move {
                recorder.lock().unwrap().push(body);
                Json(serde_json::json!({ "accepted": 1 }))
            }
        }),
    );
    let url = format!("{}/usage", serve(app).await);
    let config = crate::config::Config::parse(&format!(
        r#"
        [providers.surplus]
        kind = "surplus"
        base_url = "http://127.0.0.1:1"
        api_key_env = "LADDER_TEST_UNSET_KEY"

        [usage_sink]
        url = "{url}"
        token_env = "LADDER_TEST_UNSET_USAGE_TOKEN"
        max_batch = 1

        [[ladders]]
        name = "flash"
          [[ladders.rungs]]
          provider = "surplus"
          model = "deepseek-v4.1-flash"
        "#
    ))
    .unwrap();
    let sink: &UsageSink = config.usage_sink.as_ref().unwrap();
    let feed = Feed::new(sink, "feed-token").unwrap();
    feed.start();
    (feed, batches)
}

fn meter(feed: Option<Feed>) -> Meter {
    Meter {
        prices: Arc::new(tokio::sync::RwLock::new(crate::pricing::PriceTable::new())),
        stale_after: Duration::from_secs(60),
        feed,
        ladder: "flash".to_string(),
        surface: "chat",
        rung: 0,
        provider: "surplus".to_string(),
        model: "deepseek-v4.1-flash".to_string(),
    }
}

async fn first_record(batches: &Arc<Mutex<Vec<serde_json::Value>>>) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while batches.lock().unwrap().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no usage recorded");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    batches.lock().unwrap()[0]["records"][0].clone()
}

#[tokio::test]
async fn the_first_chunk_reaches_the_caller_before_the_upstream_has_finished() {
    let (url, upstream) = gated_upstream().await;
    upstream.send(FIRST).unwrap();
    let response = relay(opened(&url).await, ProviderKind::Surplus, meter(None), None).await;
    let mut body = response.into_body().into_data_stream();

    // The upstream has sent one chunk and is still open. A buffering relay
    // would wait here for the rest and the timeout would fire.
    let first = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .expect("the first chunk was held back until the stream ended")
        .unwrap()
        .unwrap();
    assert_eq!(first, FIRST.as_bytes());

    upstream.send(USAGE).unwrap();
    upstream.send("data: [DONE]\n\n").unwrap();
    drop(upstream);
    let mut rest = Vec::new();
    while let Some(chunk) = body.next().await {
        rest.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(rest, format!("{USAGE}data: [DONE]\n\n").as_bytes());
}

#[tokio::test]
async fn the_sub_provider_named_by_the_first_event_is_stamped_on_the_response() {
    let (url, upstream) = gated_upstream().await;
    upstream.send(FIRST).unwrap();
    let response = relay(opened(&url).await, ProviderKind::Surplus, meter(None), None).await;

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(response.headers()[HEADER_SUB_PROVIDER], "Morph");
    assert_eq!(
        response.headers()[axum::http::header::CONTENT_TYPE],
        "text/event-stream"
    );
}

#[tokio::test]
async fn a_finished_streams_usage_is_recorded() {
    let (url, upstream) = gated_upstream().await;
    let (feed, batches) = sink().await;
    for chunk in [FIRST, USAGE, "data: [DONE]\n\n"] {
        upstream.send(chunk).unwrap();
    }
    drop(upstream);
    let response = relay(
        opened(&url).await,
        ProviderKind::Surplus,
        meter(Some(feed)),
        None,
    )
    .await;
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    let record = first_record(&batches).await;
    assert_eq!(record["ladder"], "flash");
    assert_eq!(record["model"], "deepseek-v4.1-flash");
    assert_eq!(record["prompt_tokens"], 13);
    assert_eq!(record["completion_tokens"], 16);
}

#[tokio::test]
async fn a_caller_hanging_up_mid_stream_still_records_what_was_relayed() {
    let (url, upstream) = gated_upstream().await;
    let (feed, batches) = sink().await;
    upstream.send(USAGE).unwrap();
    let response = relay(
        opened(&url).await,
        ProviderKind::Surplus,
        meter(Some(feed)),
        None,
    )
    .await;
    let mut body = response.into_body().into_data_stream();
    body.next().await.unwrap().unwrap();

    // The upstream is still open; the caller walks away.
    drop(body);

    let record = first_record(&batches).await;
    assert_eq!(record["completion_tokens"], 16);
    drop(upstream);
}

#[tokio::test]
async fn a_stream_that_ends_before_its_first_chunk_is_relayed_empty() {
    let (url, upstream) = gated_upstream().await;
    drop(upstream);
    let response = relay(opened(&url).await, ProviderKind::Surplus, meter(None), None).await;

    assert!(response.headers().get(HEADER_SUB_PROVIDER).is_none());
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body.is_empty());
}

#[test]
fn an_unrelayable_stream_is_a_bad_gateway() {
    assert_eq!(unrelayable().status(), axum::http::StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn a_finished_surplus_stream_is_priced_at_the_fresh_list_price() {
    let (url, upstream) = gated_upstream().await;
    let (feed, batches) = sink().await;
    let meter = meter(Some(feed));
    meter.prices.write().await.insert(
        "surplus",
        "deepseek-v4.1-flash",
        crate::pricing::ModelPrices::new(Vec::new()).with_list_price(Some(
            crate::pricing::ListPrice {
                prompt_per_1m: 1.0,
                completion_per_1m: 2.0,
            },
        )),
    );
    upstream.send(USAGE).unwrap();
    drop(upstream);
    let response = relay(opened(&url).await, ProviderKind::Surplus, meter, None).await;
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let record = first_record(&batches).await;
    assert_eq!(record["market_source"], "list_price");
    assert_eq!(record["market_usd"], 45.0 / 1_000_000.0);
}

#[tokio::test]
async fn the_usage_meter_only_prices_surplus_with_a_fresh_snapshot() {
    for (kind, stale, expected) in [
        (ProviderKind::Surplus, false, Some(45.0 / 1_000_000.0)),
        (ProviderKind::Surplus, true, None),
        (ProviderKind::OpenRouter, false, None),
    ] {
        let (feed, batches) = sink().await;
        let meter = meter(Some(feed));
        let mut snapshot = crate::pricing::ModelPrices::new(Vec::new()).with_list_price(Some(
            crate::pricing::ListPrice {
                prompt_per_1m: 1.0,
                completion_per_1m: 2.0,
            },
        ));
        if stale {
            snapshot.fetched_at = std::time::Instant::now()
                .checked_sub(Duration::from_secs(120))
                .expect("the test clock supports a snapshot two minutes old");
        }
        meter
            .prices
            .write()
            .await
            .insert(&meter.provider, &meter.model, snapshot);
        meter.record(
            kind,
            Figures {
                prompt_tokens: Some(13),
                completion_tokens: Some(16),
                ..Figures::default()
            },
        );
        let record = first_record(&batches).await;
        assert_eq!(record["market_usd"], serde_json::json!(expected));
    }
}
