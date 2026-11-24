# Maintainer skills

Project [skills](https://code.claude.com/docs/en/skills) that let coding agents (and humans) act as llmshim maintainers. Each is a `SKILL.md` under its own directory; invoke with `/<name>` or let the agent load it when relevant.

| Skill | Invoke | What it does |
|---|---|---|
| [`add-model`](add-model/SKILL.md) | `/add-model provider/id "Label"` | Update the shared advertised catalog, tests, and docs for an existing provider. |
| [`add-provider`](add-provider/SKILL.md) | `/add-provider key Name` | Wire up a brand-new upstream provider (Provider trait, router, env keys, tests). |
| [`preflight`](preflight/SKILL.md) | `/preflight` | Run the exact fmt + clippy + test trio CI enforces. |
| [`release`](release/SKILL.md) | `/release 0.1.22` | Bump version, enforce semver rules, tag so CI publishes. |

`add-model`, `add-provider`, and `release` set `disable-model-invocation: true` — they have side effects, so a human triggers them explicitly. `preflight` is safe for the agent to run automatically.

These skills encode the repo-specific rules (the shared advertised catalog in `src/models.rs`, historical metadata retained for `spec()`, the `--features proxy` requirement, and the semver-protected public surface). Keep them in sync when those workflows change.
