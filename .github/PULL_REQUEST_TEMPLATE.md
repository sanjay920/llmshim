## What & why

<!-- What changes, and what consumer-visible behavior it affects. -->

## Public API / contract impact

- [ ] No breaking change to semver-protected `pub` items (`src/lib.rs`, `router.rs`, `provider.rs`, `error.rs`, `fallback.rs`, `log.rs`, `config.rs`, `models.rs`, `vision.rs`) — or this PR bumps the version (pre-1.0: minor is the breaking channel) with justification
- [ ] Proxy request/response shapes unchanged — or `api/openapi.yaml` and the clients under `clients/` are updated to match
- [ ] Per-provider behavior changes (reasoning, tools, vision) are verified against the live API and the pinning tests updated

## Gates (all must pass locally — CI re-checks)

- [ ] Commits are signed off (`git commit -s`, [DCO](https://developercertificate.org/))
- [ ] `cargo fmt --check`
- [ ] `cargo clippy --features proxy -- -D warnings`
- [ ] `cargo test --features proxy --tests`
- [ ] Docs under `docs/src/` updated if the request/response shapes, CLI, proxy API, or configuration changed

## Tests

<!-- Which new assertions pin this change? A fix's test must fail on the pre-fix code. -->
