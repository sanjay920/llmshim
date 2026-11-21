# 0055: A browser smoke test for the admin UI in CI

Status: built. Follows the advisor's review of 0052.

## Problem

The admin UI is hand-written, no-build JavaScript with a strict CSP. Nothing in CI loaded it, so a broken import, a CSP
violation, a renamed element or a section that renders nothing would reach `main` unseen. Until now the browser checks
were scripts run by hand.

## What is built

`crates/llmshim-control/ui-smoke/` (`run.sh`, `smoke.mjs`, `package.json` with `puppeteer-core` only) and the
`ui-smoke` job in `control-check.yml`. `run.sh` migrates a throwaway SQLite database, starts the real binary with an
admin and an ingest token, and runs `smoke.mjs` against it in headless Chrome. The script fails on:

- any page error, console error or warning (a CSP violation is one), dialog, or request to another origin;
- a wrong token that signs in, or a section (every entry in the navigation) that renders nothing;
- the Requests view not showing the two seeded requests, a sheet that does not open, close on Escape, or say where the
  prompt changed (the seeded pair differ in the system prompt, so the cache debugger has a verdict to give);
- creating an organization, team, project and client through the forms, and a secret that is not shown, stays on the
  page after "I have stored it", or remains in the DOM.

It uses the selectors a person uses (labels, placeholders), not test ids, so a rename that breaks a person breaks it.

## Not built

- A live OIDC sign-in (a Keycloak or Dex container): the SSO path is covered by the unit tests with a signed token, not
  by a browser round trip. Still to do.
- Visual regression (screenshots) and dark-mode runs.
- Payload and diff views: they need a gateway with an object store behind the control plane.
