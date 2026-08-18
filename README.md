# LLM Ladder Router

Budget-aware routing across LLM marketplace tiers, behind an OpenAI- and
Anthropic-compatible proxy.

A **ladder** is an ordered list of **rungs**. Each rung names a marketplace, a
model, and the most it may pay per million tokens. The router walks the ladder
and dispatches to the first rung whose sellers fit under that ceiling, stepping
down when none do — so a request gets the strongest model the budget allows
instead of failing or quietly overpaying.

```
POST /v1/chat/completions {"model": "reasoning", ...}

  rung 0  surplus     deepseek-v4-pro    ≤ $0.30/Mtok   cheapest seller $0.63  → skip
  rung 1  surplus     glm-5.2            ≤ $0.30/Mtok   cheapest seller $0.01  → serve
  rung 2  surplus     gpt-5.6-luna       ≤ $0.30/Mtok
  rung 3  surplus     deepseek-v4-flash  ≤ $0.15/Mtok
  rung 4  openrouter  deepseek-v4-flash  ≤ $0.30/Mtok

  x-ladder-rung: 1   x-ladder-provider: surplus   x-ladder-cap-per-1m: 0.3
```

Two ladders ship in `config.example.toml`:

| Ladder | Rungs, in order |
| --- | --- |
| `flash` | surplus `gpt-5.6-luna` → surplus `deepseek-v4-flash` → openrouter `deepseek/deepseek-v4-flash` |
| `reasoning` | surplus `deepseek-v4-pro` → `glm-5.2` → `gpt-5.6-luna` → `deepseek-v4-flash` → openrouter `deepseek/deepseek-v4-flash` |

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

## Endpoints

| Path | Surface |
| --- | --- |
| `POST /v1/chat/completions` | OpenAI chat completions |
| `POST /v1/messages` | Anthropic Messages |
| `GET /v1/models` | the configured ladders, listed as models |
| `GET /healthz` | liveness |

Both marketplaces serve both formats natively, so requests are **relayed, not
translated** — an Anthropic request reaches an Anthropic endpoint unchanged and
its response comes back unchanged. Parameters this router does not model pass
through untouched.

The `model` field names the **ladder** (`flash`, `reasoning`), not a model.

Authenticate with `Authorization: Bearer <key>` or `x-api-key: <key>`; both work
on both surfaces. The key is `server.api_key` in `config.toml`, or
`server.api_key_env` to keep it out of the file. With neither set the router
accepts every caller, which is only appropriate on a loopback bind.

Every response says how it was routed: `x-ladder-name`, `x-ladder-rung`,
`x-ladder-provider`, `x-ladder-model`, `x-ladder-sub-provider`,
`x-ladder-cap-per-1m`, `x-ladder-skipped`. When no rung can serve, the 502 body
lists each rung and why it was passed over.

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
the ceiling, its provider's balance is spent, its credential is missing, or its
price data is missing or stale. A rung that *is* tried and fails upstream
advances the ladder; a request the caller got wrong is returned as-is rather
than replayed and charged again at every rung.

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

## Marketplaces

Both are supported, and their differences are real rather than cosmetic.
`docs/specs/marketplace-apis.md` records what each API actually does, verified
live — including two findings that shaped the design:

- **Surplus ignores its own documented price cap.** `max_price_per_1m` and
  `X-Max-Price-Per-1M` have no observable effect. The `/min{N}/` path prefix
  does bind, so a dollar ceiling is restated as the equivalent minimum discount
  against the model's direct price.
- **`OpenRouter` enforces `provider.max_price` properly**, answering an
  unsatisfiable ceiling with a 404 that the router reads as "step down".

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
