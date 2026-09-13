# Image and video generation surfaces

- **Status:** Accepted
- **Owner:** Maintainers

Everything here was verified against the live Surplus API on 2026-09-14.

## Problem

The router serves text: chat, responses, messages, embeddings. Surplus also
resells image generation (55 models, priced per image or per megapixel) and
video generation (111 models, priced per job), through the same order-book and
`/min{N}/` discount machinery the chat ladders already use. A caller wanting a
picture or a clip has to go to the marketplace directly, with no ladder, no
ceiling, and no failover.

The same caller usually also wants a model that can *look at* the picture or
clip it just made, which is an ordinary chat request to a model whose input
modalities include images and video.

## Goals and non-goals

Goals:

- `POST /v1/images/generations` and `POST /v1/video/generations`, each served
  by a ladder declared for that surface, with the usual ranking, ceilings,
  failover, cooldowns and balance checks.
- Video is asynchronous upstream, so `GET` and `DELETE` on
  `/v1/video/generations/{id}` are relayed for polling and cancelling.
- A square output by default, on both surfaces, without the caller having to
  know each model's spelling of it.
- A `vision` chat ladder of image- and video-input models, in the example
  configuration.

Non-goals:

- Image editing, image-to-video, reference-to-video and upscaling. Those take
  binary input and a different request shape; nothing here precludes them.
- Translating between provider dialects. Surplus is the only provider that
  carries these models, and the router relays its OpenAI-compatible shapes.
- Buying media anywhere but Surplus. `OpenRouter` generates images through a
  chat-completions extension rather than `/v1/images`, and the direct providers
  publish neither surface. A rung on any of them declines before the round trip
  and the ladder advances, exactly as a Mistral rung declines a Messages
  request today.

## Behavior

### Surfaces and routes

Two new values of `ladders[].surface`:

| Surface | Route | Wire |
| --- | --- | --- |
| `images` | `POST /v1/images/generations` | OpenAI Images |
| `video` | `POST /v1/video/generations` | Video Generations |

The surface is checked at the door as it is for embeddings: an images request
naming a chat ladder is a 400, not a ladder walk.

Video is a job. The `POST` answers immediately with a `media.job` object
carrying `id`, `status`, `poll_url` and `cancel_url`; the router relays it
unchanged, then relays `GET /v1/video/generations/{id}` and
`DELETE /v1/video/generations/{id}` to every configured provider that serves
the video surface, in configuration order, returning the first answer that is
not a 404. The router keeps no job table: the marketplace owns the job, and a
router restart between submit and poll loses nothing.

### Ceilings are per unit

An image model is quoted per image (or per megapixel) and a video model per
job. A `max_cost_per_1m` on such a rung would be a number in the wrong unit, so
rungs on these surfaces take **`max_cost_per_unit`**, in USD per image or per
job, and `max_cost_per_1m` is refused there at load time. The reverse holds on
the token surfaces. The provider-level `max_cost_per_1m` is in tokens and is
**not inherited** by a media ladder.

The ceiling binds through the same mechanism as chat: Surplus's `/min{N}/`
prefix exists on both media routes (`/min50/v1/images/generations` answers
402, `/min50/v1/video/generations` answers 401 — both "route exists, pay
first", where a missing route is a 404), and the discount is computed against
the order book's `direct_media_unit_price` exactly as it is against
`direct_output_per_1m`.

The order book reader therefore takes `media_unit_price` for **every** media
unit, not only `1M tokens`. A ladder's rungs share a surface and so a unit, and
a per-image price ranked against a per-image price is sound. The one caveat is
`megapixel` against `image`: at the square default below one image is 1.05
megapixels, so the two are within 5% of each other and are compared as if
equal.

### Square by default

A ladder may declare `request_defaults`, a table of fields injected into every
request that did not already set them:

```toml
[[ladders]]
name = "image"
surface = "images"

  [ladders.request_defaults]
  size = "1024x1024"
```

Images spell square as `size = "1024x1024"` and video as
`aspect_ratio = "1:1"` (Surplus lists `16:9`, `9:16` and `1:1` for every
text-to-video model checked). The mechanism is general — it is a default, not
an override, and a caller who sets the field keeps their value — but square
media is what it exists for.

### Vision

Nothing new in the router. A `vision` ladder on the chat surface names models
whose input modalities include image and video; the example configuration
carries one and points the `vision-v1` alias at it rather than at `flash`.

## Constraints

- A media ladder's rungs may set `max_cost_per_unit` and may not set
  `max_cost_per_1m`; a chat or embeddings ladder's rungs the reverse.
- `request_defaults` never overwrites a field the caller sent.
- Reasoning effort is not injected on either media surface.
- Sessions are not pinned by media requests; there is no prompt cache to keep
  warm.
- Response headers name the ceiling as `x-ladder-cap-per-unit` on a media
  surface and `x-ladder-cap-per-1m` elsewhere.

## Acceptance criteria

- An images request to an images ladder reaches Surplus at
  `/min{N}/v1/images/generations` with the rung's model and, absent a caller
  `size`, the ladder's default.
- A video request does the same at `/min{N}/v1/video/generations`; its poll and
  cancel are relayed and answer 404 from the router when no provider knows the
  job.
- A rung on a non-Surplus provider on either media surface is skipped as
  unsupported and the ladder advances.
- A media rung with `max_cost_per_1m`, or a chat rung with
  `max_cost_per_unit`, is refused at load time.
- An order book quoted per image yields offers priced per image, and a ceiling
  in USD per image admits and excludes them correctly.
