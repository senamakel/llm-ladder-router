//! Unit tests for the Surplus dialect.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::CostBasis;

/// A slice of the live `venice-embed-1` order book, captured 2026-08-24.
const EMBEDDINGS_ORDER_BOOK: &str =
    include_str!("../../../tests/fixtures/surplus-embeddings-order-book.json");
/// `GET /api/markets/seedream-4.5`, captured 2026-09-14 and trimmed to nine
/// offers: six live, three not. Every offer is quoted per `image`.
const IMAGES_ORDER_BOOK: &str =
    include_str!("../../../tests/fixtures/surplus-images-order-book.json");
/// `GET /api/markets/kling-o3-pro-text-to-video`, captured 2026-09-14 and
/// trimmed the same way. Every offer is quoted per `job`.
const VIDEO_ORDER_BOOK: &str =
    include_str!("../../../tests/fixtures/surplus-video-order-book.json");

fn chosen(min_discount_pct: Option<u8>) -> Chosen {
    Chosen {
        rung: 0,
        provider: "surplus".to_string(),
        model: "glm-5.2".to_string(),
        cap_per_1m: Some(0.30),
        admitted: Vec::new(),
        cheapest_per_1m: None,
        min_discount_pct,
        prefer: Vec::new(),
        reasoning_effort: None,
        score_multiplier: 1.0,
        score: None,
    }
}

/// One offer shaped exactly like the live order book, in micro-USD per Mtok.
fn order_book(price_output: f64, available: bool, healthy: bool) -> String {
    serde_json::json!({
        "offers": [{
            "provider": "Z.ai",
            "price_input_per_1m": 3076.0,
            "price_output_per_1m": price_output,
            "direct_output_per_1m": 3_740_000.0,
            "available": available,
            "healthy": healthy,
        }]
    })
    .to_string()
}

#[test]
fn parses_an_order_book_and_converts_micro_usd_to_usd() {
    let prices = parse_order_book(order_book(9668.0, true, true).as_bytes()).unwrap();
    let offer = &prices.offers[0];

    assert_eq!(offer.provider, "Z.ai");
    // 9668 micro-USD per Mtok is $0.009668 per Mtok.
    assert!(
        (offer.completion_per_1m - 0.009_668).abs() < 1e-9,
        "{offer:?}"
    );
    assert!((offer.prompt_per_1m - 0.003_076).abs() < 1e-9, "{offer:?}");
    assert!((offer.direct_completion_per_1m.unwrap() - 3.74).abs() < 1e-9);
    assert!(offer.usable);
    // Surplus exposes no per-seller steering slug.
    assert_eq!(offer.tag, None);
}

#[test]
fn an_offer_must_be_both_available_and_healthy_to_be_usable() {
    for (available, healthy) in [(false, true), (true, false), (false, false)] {
        let prices = parse_order_book(order_book(100.0, available, healthy).as_bytes()).unwrap();
        assert!(
            !prices.offers[0].usable,
            "available={available} healthy={healthy} should not be usable"
        );
    }
}

#[test]
fn an_offer_missing_its_prices_is_read_as_free_rather_than_rejected() {
    let body = serde_json::json!({
        "offers": [{ "provider": "Sparse", "available": true, "healthy": true }]
    });
    let prices = parse_order_book(body.to_string().as_bytes()).unwrap();

    // A sparse row must not take down the whole refresh.
    assert_eq!(prices.offers[0].provider, "Sparse");
    assert!(prices.offers[0].completion_per_1m.abs() < f64::EPSILON);
    assert_eq!(prices.offers[0].direct_completion_per_1m, None);
}

#[test]
fn an_offer_without_a_provider_name_is_still_read() {
    let body = serde_json::json!({
        "offers": [{ "price_output_per_1m": 100.0, "available": true, "healthy": true }]
    });
    let prices = parse_order_book(body.to_string().as_bytes()).unwrap();
    assert_eq!(prices.offers[0].provider, "unknown");
}

#[test]
fn the_spendable_balance_is_the_lesser_of_balance_and_allowance() {
    // Micro-USD strings, exactly as the live endpoint returns them.
    let body = br#"{"balance_usdc":"74673082","allowance_usdc":"74673033"}"#;
    let balance = parse_balance(body).unwrap();

    // An allowance below the balance is the real limit.
    assert!((balance - 74.673_033).abs() < 1e-6, "{balance}");
}

#[test]
fn either_figure_alone_is_enough() {
    assert!((parse_balance(br#"{"balance_usdc":"1000000"}"#).unwrap() - 1.0).abs() < 1e-9);
    assert!((parse_balance(br#"{"allowance_usdc":"2000000"}"#).unwrap() - 2.0).abs() < 1e-9);
}

#[test]
fn a_profile_with_neither_figure_is_unreadable() {
    assert!(matches!(
        parse_balance(b"{}").unwrap_err(),
        Error::UnreadablePayload { .. }
    ));
    assert!(matches!(
        parse_order_book(b"[]").unwrap_err(),
        Error::UnreadablePayload { .. }
    ));
}

#[test]
fn the_discount_prefix_sits_in_a_different_place_on_each_surface() {
    // Verified against the live API: the other orderings 404.
    assert_eq!(
        inference_path(&chosen(Some(95)), Wire::OpenAi),
        "/min95/v1/chat/completions"
    );
    assert_eq!(
        inference_path(&chosen(Some(95)), Wire::Anthropic),
        "/anthropic/min95/v1/messages"
    );
}

#[test]
fn a_rung_needing_no_discount_uses_the_plain_path() {
    for discount in [None, Some(0)] {
        assert_eq!(
            inference_path(&chosen(discount), Wire::OpenAi),
            "/v1/chat/completions"
        );
        assert_eq!(
            inference_path(&chosen(discount), Wire::Anthropic),
            "/anthropic/v1/messages"
        );
    }
}

#[test]
fn the_paths_name_the_model_and_the_buyer() {
    assert_eq!(order_book_path("glm-5.2"), "/api/markets/glm-5.2");
    assert_eq!(balance_path(), "/v1/buyer/me");
}

#[test]
fn only_the_model_is_rewritten_in_the_body() {
    let mut body = serde_json::json!({ "model": "flash", "temperature": 0.5 });
    apply_routing(&mut body, &chosen(Some(95)));

    assert_eq!(body["model"], "glm-5.2");
    assert_eq!(body["temperature"], 0.5);
    // The ceiling travels in the path; OpenRouter's provider object must never
    // reach a Surplus endpoint.
    assert!(body.get("provider").is_none());
    assert!(body.get("max_price_per_1m").is_none());
}

#[test]
fn the_developer_role_is_folded_to_system_on_the_chat_shape() {
    let mut body = serde_json::json!({
        "model": "flash",
        "messages": [
            { "role": "developer", "content": "be terse" },
            { "role": "user", "content": "say OK" },
        ],
    });
    apply_routing(&mut body, &chosen(None));

    // Surplus 400s on `developer`, and a 400 does not advance the ladder, so
    // an unfolded role is a Codex session that cannot reach this provider.
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "be terse");
    // Every other role is left exactly as the caller sent it.
    assert_eq!(body["messages"][1]["role"], "user");
}

#[test]
fn the_developer_role_is_folded_to_system_on_the_responses_shape() {
    let mut body = serde_json::json!({
        "model": "flash",
        "input": [
            { "role": "developer", "content": "be terse" },
            { "role": "assistant", "content": "OK" },
        ],
    });
    apply_routing(&mut body, &chosen(None));

    assert_eq!(body["input"][0]["role"], "system");
    assert_eq!(body["input"][1]["role"], "assistant");
}

#[test]
fn a_body_with_nothing_to_fold_is_untouched() {
    let original = serde_json::json!({
        "model": "glm-5.2",
        "messages": "not an array",
        "input": [42, { "no_role": true }],
    });
    let mut body = original.clone();
    apply_routing(&mut body, &chosen(None));

    assert_eq!(body["messages"], original["messages"]);
    assert_eq!(body["input"], original["input"]);
}

#[test]
fn a_streaming_responses_request_is_refused_and_everything_else_is_served() {
    // The one refusal: Surplus's responses stream stops before
    // `response.output_item.done`, so a client reading it sees an empty turn.
    assert!(!serves(Wire::Responses, true));

    // Non-streaming responses is complete, and every other surface is
    // unaffected either way - refusing more than the broken case would strand
    // traffic Surplus serves perfectly well.
    assert!(serves(Wire::Responses, false));
    for wire in [Wire::OpenAi, Wire::Anthropic, Wire::Embeddings] {
        for streaming in [true, false] {
            assert!(serves(wire, streaming), "{wire:?} streaming={streaming}");
        }
    }
}

#[test]
fn a_non_object_body_is_left_alone() {
    let mut body = serde_json::json!(42);
    apply_routing(&mut body, &chosen(Some(95)));
    assert_eq!(body, serde_json::json!(42));
}

#[test]
fn the_marketplace_refusals_advance_the_ladder() {
    // No seller met the discount floor.
    assert_eq!(
        classify(
            reqwest::StatusCode::NOT_FOUND,
            br#"{"error":{"code":"minimum_discount_not_met"}}"#
        ),
        Disposition::Advance
    );
    // Nobody carries the model.
    assert_eq!(
        classify(
            reqwest::StatusCode::NOT_FOUND,
            br#"{"error":{"code":"no_sellers_for_model"}}"#
        ),
        Disposition::Advance
    );
    // The balance ran out mid-flight.
    assert_eq!(
        classify(reqwest::StatusCode::PAYMENT_REQUIRED, b"{}"),
        Disposition::Advance
    );
    // Every seller is down.
    assert_eq!(
        classify(reqwest::StatusCode::SERVICE_UNAVAILABLE, b"{}"),
        Disposition::Advance
    );
}

#[test]
fn a_model_that_mandates_reasoning_advances() {
    // The exact shape seen on 2026-09-09, from `glm-5.3-flash`, `minimax-m2.5`,
    // `minimax-m2.7` and `glm-5.3`. Whether a model can be asked to stop
    // thinking is a fact about that model, so the rung beside it serves the
    // identical body — which is what makes this an advance rather than a
    // caller error.
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"type":"invalid_request_error","code":"request_rejected","message":"Reasoning is mandatory for this endpoint and cannot be disabled."}}"#
        ),
        Disposition::Advance
    );
}

#[test]
fn a_thinking_model_demanding_its_own_reasoning_back_advances() {
    // The exact shape seen on 2026-09-09. A thinking sub-provider requires the
    // `reasoning_content` it emitted to be echoed on the assistant turns of the
    // next request; a client that does not carry that field back can never
    // satisfy it, and the router relays the caller's messages rather than
    // inventing content, so retrying here is futile. Whether a model demands
    // this is a property of that model, so the rung beside it serves the
    // identical body — which is what makes this an advance rather than a caller
    // error, despite the `invalid_request_error` type.
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"message":"The reasoning_content in the thinking mode must be passed back to the API.","type":"invalid_request_error","param":null,"code":"invalid_request_error"}}"#
        ),
        Disposition::Advance
    );
}

#[test]
fn a_genuine_caller_error_still_stops_the_ladder() {
    // The counterpart to the advancing cases above: a 400 about the request
    // itself is uniform across rungs, so walking the ladder would only repeat
    // it at every rung and bill for the attempts.
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"type":"invalid_request_error","code":"unknown_field","message":"unknown field `wat`"}}"#
        ),
        Disposition::CallerError
    );
}

#[test]
fn a_sub_provider_schema_rejection_advances() {
    // The exact shape seen on 2026-08-24: a sub-provider that accepts only a
    // string `content` refusing an Anthropic block array, relayed as a 400.
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":"2 request validation errors: Input should be a valid string, field: 'messages[1].content.str'"}"#
        ),
        Disposition::Advance
    );
    // The same failure wearing Surplus's relay prefix.
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":"Provider returned 400: bad content block"}"#
        ),
        Disposition::Advance
    );
}

#[test]
fn a_genuine_caller_error_stops_the_ladder() {
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"code":"invalid_request_error"}}"#
        ),
        Disposition::CallerError
    );
}

#[test]
fn a_success_is_served() {
    assert_eq!(
        classify(reqwest::StatusCode::OK, b"{}"),
        Disposition::Served
    );
}

#[test]
fn the_order_book_admits_only_sellers_under_the_ceiling() {
    let body = serde_json::json!({
        "offers": [
            { "provider": "cheap", "price_output_per_1m": 9_668.0, "direct_output_per_1m": 3_740_000.0, "available": true, "healthy": true },
            { "provider": "dear",  "price_output_per_1m": 900_000.0, "direct_output_per_1m": 3_740_000.0, "available": true, "healthy": true },
        ]
    });
    let prices = parse_order_book(body.to_string().as_bytes()).unwrap();

    // $0.30/Mtok admits the 0.0097 seller and excludes the 0.90 one.
    let admitted = prices.admitted(Some(0.30), CostBasis::Completion);
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].provider, "cheap");
}

/// An embedding model is billed per unit of input, so Surplus leaves the
/// per-token fields at zero and quotes it in `media_unit_price`. Read naively
/// that is a market full of free sellers.
#[test]
fn a_media_unit_price_stands_in_for_absent_token_prices() {
    let prices = parse_order_book(EMBEDDINGS_ORDER_BOOK.as_bytes()).unwrap();

    // 1000 micro-USD per Mtok is $0.001 per Mtok, and the direct price it is
    // quoted against is 20000, or $0.02.
    let cheapest = &prices.offers[0];
    assert!(
        (cheapest.completion_per_1m - 0.001).abs() < 1e-9,
        "{cheapest:?}"
    );
    assert!(
        (cheapest.prompt_per_1m - 0.001).abs() < 1e-9,
        "{cheapest:?}"
    );
    assert!((cheapest.direct_completion_per_1m.unwrap() - 0.02).abs() < 1e-9);

    // The floor is a real number rather than the zero the token fields carry.
    let floor = prices.floor(CostBasis::Completion).unwrap();
    assert!((floor - 0.001).abs() < 1e-9, "{floor}");
}

/// An image model is quoted per image and a video model per job, in the same
/// `media_unit_price` field an embedding model uses for tokens. Each is read
/// in its own unit: a rung on a media ladder is compared only against rungs
/// on the same surface, so a per-image price is a price.
#[test]
fn a_per_image_order_book_is_priced_per_image() {
    let prices = parse_order_book(IMAGES_ORDER_BOOK.as_bytes()).unwrap();

    // 400 micro-USD per image is $0.0004, against a $0.04 direct price.
    let cheapest = &prices.offers[0];
    assert!(
        (cheapest.completion_per_1m - 0.0004).abs() < 1e-12,
        "{cheapest:?}"
    );
    assert!((cheapest.direct_completion_per_1m.unwrap() - 0.04).abs() < 1e-12);

    // A ceiling of two cents an image admits the discounted sellers and
    // refuses none of the live ones in this slice, which all sit under it —
    // and one of a tenth of a cent admits only the deepest discount.
    assert_eq!(prices.admitted(Some(0.02), CostBasis::Completion).len(), 6);
    assert_eq!(prices.admitted(Some(0.001), CostBasis::Completion).len(), 1);
    // Two cents against a four-cent direct price is a 50% discount.
    assert_eq!(prices.discount_floor_pct(0.02), Some(50));
}

#[test]
fn a_per_job_order_book_is_priced_per_job() {
    let prices = parse_order_book(VIDEO_ORDER_BOOK.as_bytes()).unwrap();

    // 180000 micro-USD per job is $0.18, against a $0.45 direct price.
    let cheapest = &prices.offers[0];
    assert!(
        (cheapest.completion_per_1m - 0.18).abs() < 1e-12,
        "{cheapest:?}"
    );
    assert!((cheapest.direct_completion_per_1m.unwrap() - 0.45).abs() < 1e-12);
    // Twenty cents a job admits the one seller at eighteen and the one at
    // nineteen-point-eight, and is a 55% discount off the direct rate.
    assert_eq!(prices.admitted(Some(0.20), CostBasis::Completion).len(), 2);
    assert_eq!(prices.discount_floor_pct(0.20), Some(55));
}

/// An offer that publishes no unit at all and no token price is genuinely
/// free, and stays so: the media price is read only when the marketplace said
/// what it is per.
#[test]
fn a_media_price_with_no_unit_is_not_read() {
    let body = serde_json::json!({
        "offers": [{
            "provider": "Somewhere",
            "price_input_per_1m": 0.0,
            "price_output_per_1m": 0.0,
            "media_unit_price": 40_000.0,
            "available": true,
            "healthy": true,
        }]
    })
    .to_string();

    let offer = &parse_order_book(body.as_bytes()).unwrap().offers[0];
    assert!(offer.completion_per_1m.abs() < f64::EPSILON, "{offer:?}");
    assert_eq!(offer.direct_completion_per_1m, None);
}

/// A seller quoting no price of its own is reading as undiscounted, not as
/// free. One usable seller at zero would otherwise drag the whole rung's floor
/// to zero and rank it ahead of every priced rung it competes with.
#[test]
fn a_seller_quoting_no_price_of_its_own_is_read_as_undiscounted() {
    let prices = parse_order_book(EMBEDDINGS_ORDER_BOOK.as_bytes()).unwrap();
    let unquoted = prices
        .offers
        .iter()
        .find(|offer| offer.provider == "Morpheus")
        .expect("the fixture carries one seller with no media unit price");

    // The direct price, 20000 micro-USD per Mtok, rather than zero.
    assert!(
        (unquoted.completion_per_1m - 0.02).abs() < 1e-9,
        "{unquoted:?}"
    );
    assert!((unquoted.direct_completion_per_1m.unwrap() - 0.02).abs() < 1e-9);
}

/// Both media routes take the discount prefix in the leading position, the
/// same place chat completions does; a job is polled and cancelled at its
/// un-prefixed path.
#[test]
fn the_media_paths_carry_the_discount_prefix() {
    assert_eq!(
        inference_path(&chosen(Some(50)), Wire::Images),
        "/min50/v1/images/generations"
    );
    assert_eq!(
        inference_path(&chosen(None), Wire::Images),
        "/v1/images/generations"
    );
    assert_eq!(
        inference_path(&chosen(Some(55)), Wire::Video),
        "/min55/v1/video/generations"
    );
    assert_eq!(
        inference_path(&chosen(None), Wire::Video),
        "/v1/video/generations"
    );
    assert_eq!(
        video_job_path("01M2E752E5S0GQ5AYRJWB8WGD1"),
        "/v1/video/generations/01M2E752E5S0GQ5AYRJWB8WGD1"
    );
}

/// Every prefixed spelling of the embeddings path 404s on the live API, so a
/// discount never travels on this surface — which is why an embeddings ladder
/// is refused a ceiling at load time.
#[test]
fn the_embeddings_path_never_carries_a_discount_prefix() {
    assert_eq!(
        inference_path(&chosen(Some(50)), Wire::Embeddings),
        "/v1/embeddings"
    );
    assert_eq!(
        inference_path(&chosen(None), Wire::Embeddings),
        "/v1/embeddings"
    );
}

#[test]
fn a_rung_the_marketplace_no_longer_lists_or_that_refuses_the_frame_advances() {
    // Both shapes as seen on 2026-09-14: a delisted image model, and a video
    // model that renders only widescreen answering a square ladder. Neither
    // is about the request, so neither should end the walk.
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"type":"invalid_request_error","message":"venice-recraft-v4-pro is not a valid model ID. Unusual bug? Email support@surplusintelligence.ai"}}"#
        ),
        Disposition::Advance
    );
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"type":"invalid_request_error","code":"invalid_request_error","message":"Unsupported aspect_ratio '1:1' for model 'veo3-1-fast-text-to-video'. Supported: 16:9, 9:16."}}"#
        ),
        Disposition::Advance
    );
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"type":"invalid_request_error","code":"request_rejected","message":"The model 'gpt-5-image-mini' does not exist."}}"#
        ),
        Disposition::Advance
    );
    assert!(is_delisted("venice-recraft-v4-pro is not a valid model ID"));
    assert!(!is_delisted("prompt must be shorter"));
    // A 400 about the request itself still stops the walk.
    assert_eq!(
        classify(
            reqwest::StatusCode::BAD_REQUEST,
            br#"{"error":{"type":"invalid_request_error","message":"prompt must be shorter than or equal to 2000 characters."}}"#
        ),
        Disposition::CallerError
    );
}

#[test]
fn a_polled_job_is_read_for_whether_the_marketplace_placed_it() {
    // The four live shapes, 2026-09-14.
    let fresh = serde_json::json!({
        "status": "queued", "served_by": "unknown", "provider_family": "unknown",
        "marketplace_status": "unknown", "marketplace_attempts": 0
    });
    assert_eq!(job_progress(&fresh), JobProgress::Waiting);

    let running = serde_json::json!({
        "status": "running", "served_by": "api.venice.ai", "provider_family": "venice",
        "marketplace_status": "submitted", "marketplace_attempts": 2
    });
    assert_eq!(job_progress(&running), JobProgress::Taken);
    assert_eq!(
        job_progress(&serde_json::json!({ "status": "succeeded" })),
        JobProgress::Taken
    );
    // Queued but already assigned to a seller counts as taken.
    assert_eq!(
        job_progress(&serde_json::json!({ "status": "queued", "served_by": "api.venice.ai" })),
        JobProgress::Taken
    );

    let unplaced = serde_json::json!({
        "status": "failed", "served_by": "unknown", "marketplace_attempts": 1,
        "error": { "type": "provider_unavailable", "message": "No provider could accept this job right now. Please retry." }
    });
    assert_eq!(
        job_progress(&unplaced),
        JobProgress::Failed(
            "provider_unavailable: No provider could accept this job right now. Please retry."
                .to_string()
        )
    );
    let broke = serde_json::json!({ "status": "failed", "served_by": "api.venice.ai", "error": { "type": "provider_error" } });
    assert_eq!(
        job_progress(&broke),
        JobProgress::Failed("provider_error: no detail".to_string())
    );
    assert_eq!(
        job_progress(&serde_json::json!({ "status": "canceled" })),
        JobProgress::Failed("canceled: no detail".to_string())
    );
    assert_eq!(
        video_artifact_path("01JOB", "0"),
        "/v1/media/artifacts/01JOB/0"
    );
}
