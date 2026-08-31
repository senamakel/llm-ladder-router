# Ultimate fallback implementation plan

- **Specification:** [`../specs/ultimate-fallback.md`](../specs/ultimate-fallback.md)

## Goal

Support an optional, uncapped fallback rung that is attempted only after a
ladder's normal routing policy has been exhausted.

## Tasks

1. Add `Ladder::fallback` and configuration validation in `src/config/`.
   Write parsing and invalid-configuration tests first.
2. Extend the pure selection engine to consider a fallback only after normal
   candidates are unavailable, bypassing price data and ceilings but retaining
   availability gates. Add ladder tests before the implementation.
3. Update the proxy walk to allow one additional fallback attempt and to avoid
   pinning it. Add end-to-end tests for price exhaustion, upstream failure, and
   normal-rung precedence.
4. Document the configuration in `README.md` and `config.example.toml`.
5. Run formatting, Clippy, build, all-feature tests, and rustdoc checks.
