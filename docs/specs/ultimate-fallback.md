# Ultimate fallback rung

- **Status:** Accepted
- **Owner:** Maintainers
- **Plan:** [`../plans/ultimate-fallback.md`](../plans/ultimate-fallback.md)

## Problem

A ladder can exhaust every normal rung because prices exceed their ceilings, a
provider is unavailable, or an upstream attempt fails. Some workloads would
prefer a known, potentially expensive model to a 502 in that case.

## Behavior

A ladder may declare one optional `fallback` rung:

```toml
[[ladders]]
name = "scribe"

  [ladders.fallback]
  provider = "surplus"
  model = "deepseek-v4-flash"
```

The router considers normal `rungs` first, using their normal price ceilings
and ranking. It considers `fallback` only when no normal rung can serve, or
after every selected normal rung has advanced because of an upstream failure.

The fallback never inherits a ladder, rung, or provider price ceiling. It is
dispatched without a marketplace price filter, so it may cost more than the
normal policy permits. It still requires a configured credential, a usable
balance, a non-cooling provider/model, and a provider that supports the request
wire. Its own upstream failure still produces the usual exhausted-ladder 502.

Fallbacks are not session-pinned: a subsequent request always gets another
chance to use a normal rung at its normal price.

## Constraints

- One fallback per ladder; it is optional and existing configurations retain
  their current behavior.
- A fallback must name a configured provider and must not set
  `max_cost_per_1m`, because an ultimate fallback intentionally has no cap.
- The fallback is on the ladder's declared surface and follows its reasoning
  effort policy.
- The response identifies the served fallback through the existing model and
  routing headers.

## Acceptance criteria

- A priced-out normal ladder reaches its fallback without a cap.
- An upstream failure on every normal rung reaches its fallback.
- A healthy normal rung wins over the fallback, even if the fallback is cheaper.
- Invalid fallback provider and fallback ceiling configuration are rejected.
- A fallback never creates a session pin.
