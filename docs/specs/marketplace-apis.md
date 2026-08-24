# Marketplace APIs

Everything here was verified against the live APIs on 2026-08-18, and the
embeddings sections on 2026-08-24. Where a
documented feature does not behave as documented, the observed behavior wins and
the discrepancy is recorded — the router is built against what the servers do.

## Price units

| Source | Field | Unit |
| --- | --- | --- |
| OpenRouter | `pricing.prompt`, `pricing.completion` | USD **per token**, as a string; multiply by 1e6 for USD/Mtok |
| Surplus | `price_output_per_1m`, `direct_output_per_1m` | **micro-USD per 1M tokens**; divide by 1e6 for USD/Mtok |
| Surplus | `balance_usdc`, `allowance_usdc`, `buyer_cost_usdc` | **micro-USD**, as a string |

Surplus units are not documented; they were derived by cross-checking
`direct_output_per_1m = 3740000` for `glm-5.2` against its published $3.74/Mtok
direct output price.

## OpenRouter

Base URL `https://openrouter.ai/api/v1`. Bearer auth. Author-qualified model
slugs (`deepseek/deepseek-v4-flash`).

- `GET /models/{model}/endpoints` → `data.endpoints[]`, one entry per
  sub-provider: `provider_name` ("DigitalOcean"), `tag` ("digitalocean" — this
  is the slug `provider.order` expects, so prefer it over case-folding
  `provider_name`), `pricing.{prompt,completion,input_cache_read}`,
  `context_length`, `quantization`, `status`, `uptime_last_5m`.
- `GET /credits` → `data.{total_credits, total_usage}`; remaining is the
  difference.
- `POST /chat/completions` accepts `provider.max_price.{prompt,completion}` in
  **USD per Mtok**. This is enforced: an unsatisfiable cap returns
  **404 `No endpoints found that satisfy the max price for this request`**.
  Sub-provider preference is `provider.order` plus `allow_fallbacks: true`.

`provider.only` is deliberately unused: an exclusive pin has been observed to
leave requests hanging while idle sub-providers sat unused.

## Surplus Intelligence

Base URL `https://api.surplusintelligence.ai`. Bearer auth. Unqualified model
slugs (`glm-5.2`). OpenAI-compatible.

- `GET /api/markets/{model}` → the full order book, **no auth required**:
  `offers[]` with `price_input_per_1m`, `price_output_per_1m`,
  `direct_output_per_1m`, `cost_multiplier`, `provider`, `rank`, `available`,
  `healthy`, `cap_remaining`. 240 offers for `glm-5.2`, 155 of them
  available and healthy.
- `GET /v1/models` → catalogue with **reference** prices only (per token, string).
  Reference prices are not what a marketplace seller charges; use the order book.
- `GET /v1/buyer/me` → `balance_usdc`, `allowance_usdc`, `credit_balance_usdc`,
  plus `stats` and `recent_usage`. Spendable is `min(balance, allowance)`.
- `POST /v1/chat/completions`, and `POST /min{N}/v1/chat/completions` for
  minimum-discount routing.
- `POST /v1/embeddings`, with **no discounted form** — see below.

Response headers worth capturing: `x-si-served-by`, `x-si-marketplace-status`
(`served` / `filtered`), `x-si-marketplace-attempts`, `x-si-provider-family`,
`x-si-route-objective`, `x-si-routing-decision-ms`, `x-request-id`, and the
`x-ratelimit-*` trio.

### The price cap does not work; the discount floor does

`max_price_per_1m` (body) and `X-Max-Price-Per-1M` (header) are both documented
as price filters. **Neither has any observable effect.** A `glm-5.2` request
capped at $0.0001/Mtok was served normally, while the cheapest available and
healthy seller in the order book was $0.0128/Mtok. Both forms were tested
independently; both returned 200 and served.

`/min{N}/v1/chat/completions` **is** enforced. `/min100/` returns
**404 `minimum_discount_not_met`** with `x-si-marketplace-status: filtered` and
`x-si-marketplace-attempts: 0`, and the message names the best otherwise-eligible
discount. `/min99/` served.

The router therefore expresses a Surplus rung's dollar cap as the equivalent
minimum discount against the model's direct price:

```text
N = floor(100 × (1 − cap_usd_per_mtok ÷ direct_usd_per_mtok))
```

clamped to `0..=99`, with `direct_usd_per_mtok` read from the order book. This
is the one Surplus mechanism that actually binds, and it preserves the intent of
a per-request dollar ceiling.

This finding invalidates the premise of riemann's Surplus budget ladder, which
sends `max_price_per_1m` and expects rungs to advance when no seller fits. They
never advance on price.

### Embeddings serve, but carry no price filter and no token prices

`venice-embed-1` is the one embedding model in the catalogue: `modality:
embedding`, `supported_parameters: ["embedding"]`, 175 offers, 173 of them
available and healthy. `POST /v1/embeddings` answers (402 `x402_payment_required`
unauthenticated, which is the same "route exists, pay first" answer the chat path
gives).

**No spelling of the discount prefix exists on this surface.** All three 404 on
the same run where `/min50/v1/chat/completions` answered 400 for a chat-shaped
body — a route that exists, rejecting the payload:

| Path | Status |
| --- | --- |
| `/v1/embeddings` | 402 (serves) |
| `/min50/v1/embeddings` | 404 |
| `/v1/min50/embeddings` | 404 |
| `/embeddings/min50` | 404 |

So a dollar ceiling cannot be restated as anything on the embeddings surface,
and the router refuses one at load time rather than carrying a number that binds
nothing.

**The prices are not in the token fields.** Every one of the 175 offers reports
`price_input_per_1m` and `price_output_per_1m` as `0`; the real figure is in
`media_unit_price` with `media_unit: "1M tokens"`, against a
`direct_media_unit_price` of `20000` (= $0.02/Mtok). Read naively, that is a
market of 173 free sellers, which would give an embeddings rung a floor of zero
and rank it ahead of every priced rung beside it. The router reads
`media_unit_price` when the unit is `1M tokens`, and only then — the same field
prices an image model per image.

Five of the 175 offers publish no `media_unit_price` of their own while still
carrying the `direct_media_unit_price`. One usable seller at zero is enough to
drag the whole rung's floor to zero, so those are read at the direct price:
"published no discount" is the honest reading, and it errs upward.

### The served provider is not necessarily an order-book seller

A `glm-5.2` request was served by `BaseTen`, which has **zero** offers in the
order book, with `x-si-provider-family: openrouter`. Surplus sellers can
themselves be resellers, so the `provider` in the response body is the terminal
upstream, not the seller that was matched. Local order-book filtering is
therefore a sound way to *skip* a rung that clearly cannot fit, but it is not a
guarantee about what the request will cost — only `/min{N}/` is.

## Rung-advance signals

Advance to the next rung on these; everything else is a caller error and is
returned unchanged rather than replayed at another provider's expense.

Marketplace-independent, in `provider::types::classify_status`: any 5xx, 408,
429, and **401 / 403 / 407**. The last three are refusals of *this router's*
credential, which the caller has never seen and cannot fix, so they name a rung
that cannot serve rather than a request that cannot be made — measured, when a
Surplus edge 403 was handed back to five callers that each had a working second
provider one rung below. The rest of 4xx describes the request, which is
identical at every rung, so walking the ladder would only report the last
refusal.

| Provider | Additional signal |
| --- | --- |
| OpenRouter | 404 no-endpoints-satisfy-max-price; 400 `Provider returned error` |
| Surplus | 404 `minimum_discount_not_met`; 404 `no_sellers_for_model`; 402 payment required |
