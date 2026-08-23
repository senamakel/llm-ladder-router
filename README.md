# LLM Ladder Router

Budget-aware routing across LLM marketplace tiers, behind an OpenAI- and
Anthropic-compatible proxy — chat completions, responses, and messages.

A **ladder** is a set of **rungs**. Each rung names a marketplace, a model, the
most it may pay per million tokens, and what that model is *worth* — its
`score_multiplier`. The router prices every rung against the live order books,
divides each one's cheapest admitted seller by its multiplier, and dispatches to
the lowest result — so a request gets the best value the budget allows instead
of failing, quietly overpaying, or taking a weak model because it happened to be
listed first.

```
POST /v1/chat/completions {"model": "reasoning", ...}

  rung  provider    model              ceiling   cheapest   ×mult   score
  0     surplus     deepseek-v4-pro    ≤ 0.30       0.63       —       —   priced out
  1     surplus     glm-5.2            ≤ 0.30       0.09     1.8    0.050  ← serves
  2     surplus     gpt-5.6-luna       ≤ 0.30       0.07     1.2    0.058
  3     surplus     deepseek-v4-flash  ≤ 0.15       0.06     1.0    0.060
  4     openrouter  deepseek-v4-flash  ≤ 0.30       0.14     1.0    0.140

  x-ladder-rung: 1   x-ladder-provider: surplus   x-ladder-score: 0.05
```

Ladder order is not precedence. It is documentation, and the tie-break when two
rungs score the same.

Five ladders ship in `config.example.toml`:

| Ladder | Rungs | Ceilings | Multipliers | Depth |
| --- | --- | --- | --- | --- |
| `flash` | surplus `gpt-5.6-luna`, surplus `deepseek-v4-flash`, openrouter `deepseek/deepseek-v4-flash` | 0.30 / 0.15 / 0.30 | 1.2 / 1.0 / 1.0 | — |
| `reasoning` | surplus `deepseek-v4-pro`, `glm-5.2`, `gpt-5.6-luna`, `deepseek-v4-flash`, openrouter `deepseek/deepseek-v4-flash` | 0.30 × 3 / 0.15 / 0.30 | 2.0 / 1.8 / 1.2 / 1.0 / 1.0 | — |
| `max-reasoning` | surplus `deepseek-v4-pro`, `glm-5.2`, `gpt-5.6-luna`, openrouter `deepseek/deepseek-v4-pro` | 1.00 / 1.00 / 0.60 / 1.00 | 8.0 / 6.0 / 1.5 / 8.0 | `high` / `high` / `xhigh` / `high` |
| `scribe` | mistral `labs-leanstral-1-5` | — | — | — |
| `uncensored` | surplus `venice-uncensored-1.2`, venice `venice-uncensored-1-2` | 0.30 / — | 1.0 / 1.0 | — |

`max-reasoning` is the odd one and deliberately so: it pays roughly three times
what `reasoning` pays, and it asks for depth — `reasoning_effort = "high"` on
every rung, `xhigh` on the one model family that takes more. Every rung is a
reasoning model, so it steps down in price without stepping down in kind. It is
for the handful of callers whose answer keeps improving while the model thinks
longer, not for anything on a per-turn budget. See
[Reasoning depth](#reasoning-depth).

## Quick start

```sh
cp config.example.toml config.toml
cp .env.example .env && $EDITOR .env      # marketplace credentials
./scripts/run-server start                # http://127.0.0.1:6969
```

```sh
curl localhost:6969/v1/chat/completions \
  -H 'authorization: Bearer ladder-local-dev-key' \
  -H 'content-type: application/json' \
  -d '{"model":"flash","messages":[{"role":"user","content":"hi"}]}' -i
```

`scripts/run-server` also takes `stop`, `restart`, `status`, `logs`, and `tail`.
It keys its pidfile to the config path, so a second config on a second port runs
alongside the first. Point it elsewhere with `LADDER_CONFIG=other.toml`.

## Docker

Images are published to GitHub Packages on every push to `main` and every
`v*` tag.

```sh
docker run --rm -p 6969:6969 \
  --env-file .env \
  -v "$PWD/config.toml:/etc/ladder/config.toml:ro" \
  ghcr.io/senamakel/llm-ladder-router:latest
```

The image ships `config.example.toml` as its default configuration, so it runs
with no volume at all — mount your own over `/etc/ladder/config.toml` to change
the ladders or the caller key. Marketplace credentials come from the
environment, as they do everywhere else.

Two things to know before exposing it:

- It binds `0.0.0.0`, because loopback inside a container is reachable by
  nothing. That means **the caller key is what stands between the router and
  anyone who can reach the port** — set `server.api_key`, or point
  `server.api_key_env` at `LADDER_API_KEY` and pass that in.
- The router loads every rung's order book before it binds, so a fresh
  container is deliberately not healthy for the first few seconds. The
  `HEALTHCHECK` allows for it with a start period; give orchestrator probes the
  same slack.

It runs as an unprivileged fixed uid (10001), and needs no CA bundle — TLS
roots are compiled in.

## Endpoints

| Path | Surface |
| --- | --- |
| `POST /v1/chat/completions` | OpenAI chat completions |
| `POST /v1/responses` | OpenAI responses |
| `POST /v1/messages` | Anthropic Messages |
| `GET /v1/models` | the configured ladders, listed as models |
| `GET /healthz` | liveness |

Both marketplaces serve all three formats natively, so requests are **relayed,
not translated** — an Anthropic request reaches an Anthropic endpoint unchanged
and its response comes back unchanged. Parameters this router does not model
pass through untouched.

Responses is its own surface rather than a flavour of chat completions: the
request names its prompt in `input` rather than `messages`, the reply is a
`response` object rather than a `chat.completion`, and reasoning depth is
spelled differently in each. It is also the only surface some agent harnesses
speak — anything built on `codex` posts to `/responses` and nothing else.

The `model` field names the **ladder** (`flash`, `reasoning`, `max-reasoning`,
`scribe`), not a model.

Authenticate with `Authorization: Bearer <key>` or `x-api-key: <key>`; both work
on both surfaces. The key is `server.api_key` in `config.toml`, or
`server.api_key_env` to keep it out of the file. With neither set the router
accepts every caller, which is only appropriate on a loopback bind.

Every response says how it was routed: `x-ladder-name`, `x-ladder-rung`,
`x-ladder-provider`, `x-ladder-model`, `x-ladder-sub-provider`,
`x-ladder-cap-per-1m`, `x-ladder-score`, `x-ladder-skipped`, and
`x-ladder-reasoning-effort` when the ladder asked for a reasoning depth. When no rung can serve, the 502 body
lists each rung and why it was passed over.

## Scoring

`score = cheapest admitted seller ÷ score_multiplier`, and the lowest score
serves. The multiplier answers one question — *how many times the baseline
model's price is this one still worth paying* — so `1.0` is the baseline, a rung
at `2.0` wins while it stays under twice the baseline's price, and a rung that
says nothing is a baseline rung.

The same model carries a different multiplier on different ladders, and that is
the point: what depth is worth is a property of the job, not of the model.
`flash` keeps its multipliers within a factor of two, because on that ladder any
rung will do and price should decide; `max-reasoning` puts eight times between
its strongest and weakest rung, because a cheap seller on a weak model is not
what that ladder is for.

A rung with no price data and no ceiling is *unpriced*, not free: it cannot be
ranked, so it serves only when nothing that can be ranked is available.

## Ceilings

Ceilings are configuration, never per-request, and they combine — a
provider-wide ceiling bounds every rung that uses it, and a rung may tighten it
further. **The tighter of the two applies.**

```toml
[providers.surplus]
max_cost_per_1m = 0.50      # nothing on this marketplace exceeds this

  [[ladders.rungs]]
  provider = "surplus"
  model = "deepseek-v4-flash"
  max_cost_per_1m = 0.15    # ...and this rung is stricter still
```

`cost_basis` picks which price the ceiling applies to: `completion` (the
default, since output dominates most bills), `prompt`, or `blended`.

A rung is skipped without spending a round trip when its sellers are all above
the ceiling, its provider's balance is spent, its credential is missing, its
price data is missing or stale, or it is cooling down after a rate limit. A rung
that *is* tried and fails upstream drops out and the next-best rung takes the
request; a request the caller got wrong is returned as-is rather than replayed
and charged again at every rung.

## Reasoning depth

A ladder is a price band, and it can also be a **depth**. `reasoning_effort` on
a ladder is injected into every request that did not already carry one; a rung
overrides it, because the accepted values belong to the model family rather than
to the ladder — `xhigh` is understood by some reasoning models and rejected by
others, and a rejected value is a 400 the failover loop hands back to the caller
rather than stepping past.

```toml
[[ladders]]
name = "max-reasoning"
reasoning_effort = "high"       # every rung, unless it says otherwise

  [[ladders.rungs]]
  provider = "surplus"
  model = "gpt-5.6-luna"
  reasoning_effort = "xhigh"    # ...the one family that takes more
```

Three rules hold. **The caller always wins** — a body already carrying
`reasoning_effort` or `reasoning` is left alone, so a request asking for a
shallow answer is not made expensive by the ladder it selected. **Only on the
two OpenAI surfaces**, since Anthropic spells depth as a `thinking` token budget
and inventing one from an effort word would be translating rather than relaying.
And a ladder that declares nothing inserts nothing, so every ladder written
before this field behaves exactly as it did.

The two OpenAI surfaces spell the same idea differently, and each gets its own
spelling: chat completions take a top-level `reasoning_effort` string, responses
take a `reasoning` object with an `effort` member. Sending the chat spelling to
`/responses` would leave an unknown top-level key in the body and buy none of
the depth the ladder asked for.

The shipped `max-reasoning` ladder is the intended use: higher ceilings than
`reasoning`, and no rung below a reasoning model — a ladder whose last rung is a
fast model would answer a max-reasoning request with the cheapest thing
available, which is the failure it exists to avoid.

## When an upstream refuses

Two things a provider does are refusals rather than failures, and both advance
the ladder *and* park the rung — so a provider that is refusing costs one
wasted round trip rather than one per request.

**A 429** is the upstream refusing for a while. It carries its own backoff.

**A 401, 403 or 407** is the upstream refusing this router outright. That reads
like a caller error and is not, because two authentications are in play and
they are not the same one: the caller authenticates to this router, and the
router authenticates to the marketplace with a credential the caller has never
seen and cannot fix. So it means "this provider will not serve this router",
which is exactly what the next rung is for.

That rule was learned rather than reasoned out. Surplus spent about fifteen
minutes answering `403 Forbidden` as an HTML page from its own edge; every
ladder passed it straight back, and five long agent runs died inside the same
minute with a working second provider sitting one rung below. **A ladder whose
rungs all name one provider has no failover, whatever its length** — the
shipped config now ends every ladder on a second provider for that reason.

```toml
[rate_limits]
cooldown = "30s"        # when the upstream names no delay
max_cooldown = "5m"     # the longest Retry-After that will be honoured
```

The upstream's own `Retry-After` wins when it sends one, clamped to
`max_cooldown` — a header asking for an hour would otherwise empty a ladder on
one busy minute. A refusal carries no `Retry-After`, so `cooldown` applies. A
cooldown is per **rung**, not per provider: one model being throttled says
nothing about another on the same marketplace, and taking a whole marketplace
out on one model's answer would empty a ladder. A 500 or a timeout parks
nothing — that says the upstream broke, which the next request has every reason
to re-test.

A parked rung is skipped exactly as a priced-out one is, and says so in the 502
body: `rate limited, retry in 12s`.

## Session pinning

Marketplaces bill cached prompt tokens at a fraction of the normal rate, but a
cache is warm only on the sub-provider that already saw the prefix. A long
thread that hops between rungs pays full price for its whole history on every
hop, which for a growing conversation soon costs more than the cheaper rung ever
saved.

So once a conversation has been served it is pinned to the rung *and*
sub-provider that served it, and stays there while that choice still fits the
budget. The pin is dropped the moment it stops being justified — a changed
ceiling, a rung the market can no longer satisfy, a spent balance, or a switch to
another ladder. **A pin never overrides a ceiling**; it only breaks the tie
between rungs the budget already allows.

Identify a conversation with the `x-ladder-session` header, or let the router use
what your client already sends: OpenAI's `user` field or Anthropic's
`metadata.user_id`. Responses carry `x-ladder-session` and `x-ladder-pinned`, and
a dropped pin is logged with the reason.

```toml
[sessions]
enabled = true
ttl = "30m"            # idle timeout; every request refreshes the pin
max_entries = 10000
header = "x-ladder-session"
```

One subtlety worth knowing: a marketplace names the sub-provider that served in
one vocabulary and accepts steering in another — `OpenRouter` reports
`DigitalOcean` but routes on `digitalocean`, and a quantized endpoint answers to
`deepinfra/fp8`. The router resolves one to the other, so a pin actually lands.

## Direct providers

Not every model is resold. `kind = "mistral"` reaches Mistral's own API, where
there is one seller: no order book, no balance to poll, and nothing for a
ceiling to bind against — so a ceiling on such a provider or its rungs is
refused at load time rather than silently skipping every rung under it for
missing price data. A rung here is a choice of model.

```toml
[providers.mistral]
kind = "mistral"
base_url = "https://api.mistral.ai"
api_key_env = "MISTRAL_API_KEY"

[[ladders]]
name = "scribe"
  [[ladders.rungs]]
  provider = "mistral"
  model = "labs-leanstral-1-5"
```

A ladder of one rung is how this router says "this model or nothing". Mistral
serves only the OpenAI chat-completions surface — it publishes neither
`/v1/messages` nor `/v1/responses` — so a request on either of those wires is
declined before it is sent rather than translated, and the error names the
surface that was declined.

`kind = "venice"` is the same shape for a different reason. Venice's uncensored
model *is* resold, but whether a given marketplace still carries it next month
is that marketplace's policy decision rather than a fact the router can lean on.
So the `uncensored` ladder buys it on Surplus when Surplus is cheap and falls
back to the house that publishes it when Surplus cannot serve:

```toml
[providers.venice]
kind = "venice"
base_url = "https://api.venice.ai"
api_key_env = "VENICE_INFERENCE_KEY"

[[ladders]]
name = "uncensored"
  [[ladders.rungs]]
  provider = "surplus"
  model = "venice-uncensored-1.2"
  max_cost_per_1m = 0.30

  [[ladders.rungs]]
  provider = "venice"
  model = "venice-uncensored-1-2"
```

The two spellings are the same model: a rung names it in its provider's own
convention, and the marketplace writes the version with a dot where the house
writes it with a hyphen.

Both rungs are the same model, so nothing is traded away by falling through —
only the discount is. The order comes out of the engine rather than the file: an
unpriced rung has nothing to divide by its multiplier, so it ranks behind every
rung that could be measured, and the direct rung serves exactly when the resold
one is over its ceiling, stale, rate-limited, or gone.

One Venice-specific rewrite travels with every request: Venice prepends a system
prompt of its own unless told not to, and a tier that picked this model and
silently got that framing on top of it is not the tier that was picked — so the
router sets `venice_parameters.include_venice_system_prompt` to `false`. A
caller who sends their own `venice_parameters` keeps every key they set.

## Marketplaces

Both marketplaces are supported — see [Direct providers](#direct-providers) for
the third kind — and their differences are real rather than cosmetic.
`docs/specs/marketplace-apis.md` records what each API actually does, verified
live — including two findings that shaped the design:

- **Surplus ignores its own documented price cap.** `max_price_per_1m` and
  `X-Max-Price-Per-1M` have no observable effect. The `/min{N}/` path prefix
  does bind, so a dollar ceiling is restated as the equivalent minimum discount
  against the model's direct price.
- **`OpenRouter` enforces `provider.max_price` properly**, answering an
  unsatisfiable ceiling with a 404 that the router reads as "this rung is out".

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test --all-features
```

Tests never touch the network: the selection engine is pure and takes prices and
balances as arguments, and everything else runs against loopback servers
impersonating the marketplaces. See [`AGENTS.md`](AGENTS.md) for the full
working agreement.
