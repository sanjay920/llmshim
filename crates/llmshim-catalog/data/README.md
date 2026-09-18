`models.dev.json` is the unmodified response downloaded from
https://models.dev/api.json on 2026-09-16. It is third-party data under the MIT
license in `LICENSE.models.dev`. No credentials, local overrides, provider
account discovery, or private fixtures belong in this directory.

The refresh workflow replaces only this public snapshot. Tests validate its
structure and minimum coverage before proposing an update. The builtin table
continues to win conflicts for the fields it asserts.
