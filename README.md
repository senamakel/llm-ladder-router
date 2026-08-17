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
  rung 2  surplus     deepseek-v4-flash  ≤ $0.15/Mtok
  rung 3  openrouter  deepseek-v4-flash  ≤ $0.30/Mtok

  x-ladder-rung: 1   x-ladder-provider: surplus   x-ladder-cap-per-1m: 0.3
```

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
