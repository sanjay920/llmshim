# llmshim-catalog

An offline-first Rust model catalog. It ships a models.dev snapshot, layers
verified defaults and local policy over it, and refreshes without blocking
model requests. It has no dependency on the llmshim translation crate.

```rust
use llmshim_catalog::{Catalog, ModelFamily};

let catalog = Catalog::vendored(); // no files or network needed
let model = catalog.resolve("anthropic/claude-opus-5-5").unwrap();
assert_eq!(model.family, Some(ModelFamily::Claude));
```

`ModelInfo` carries tri-state capabilities, coarse family, USD-per-million-token
prices (including cache reads/writes), effort and budget reasoning options,
input/output modalities, knowledge/release dates, and field-level sources.
Missing data remains unknown. A date given only as a month is not turned into
an invented day. Unknown family keys remain absent and must not authorize replay.

`ModelInfo::cost_for_input_tokens(total)` selects context-dependent rates from
`context_cost_tiers`; `cost` remains the base rates. The total includes cached
input. A tier applies to the entire request strictly above its threshold, with
missing rates inherited from the preceding tier. Models.dev `cost.tiers` entries
whose type is `context` are imported. Local policy can set
`context_cost_tiers = []` to disable inherited tiers, or use:

```toml
[[models."xai/grok-4.7".context_cost_tiers]]
above_input_tokens = 200000
[models."xai/grok-4.7".context_cost_tiers.cost]
input = 4.0
cache_read = 1.0
output = 12.0
```

The strongest source replaces the tier list as a whole. Higher-priority base
price overrides still win per field over lower-priority tier rates. Provider
discovery cannot supply either base prices or tiers.

The compiled `builtin` module retains the small verified discovery table,
historical metadata, and the separate ChatGPT subscription allowlist. Catalog
coverage does not authorize or configure an API route or a subscription model.
`data/verified.json` supplies sourced launch metadata independently of the
models.dev snapshot. It does not add models to the curated discovery list.

## Layers and policy

`Catalog::load(&CatalogOptions::default())` reads, in order:

1. The vendored models.dev snapshot.
2. `~/.cache/llmshim/models.dev.json`, if usable and online mode is enabled.
3. Builtin assertions (already applied to the floor and protected during merging).
4. `~/.config/llmshim/models.toml`.
5. The current project's `.llmshim/models.toml`.

`XDG_CACHE_HOME` and `XDG_CONFIG_HOME` override the corresponding base paths;
`LLMSHIM_CATALOG_PROJECT` overrides the project file. Explicit options are useful
for applications that should not inherit a working directory's policy.

The precedence rule is per field: local > provider API > verified builtin >
models.dev. Within models.dev, the refreshed cache wins over the shipped data.
Within local policy, the project wins over the user file. `None`/`Unknown` never
erases an assertion. `field_sources` retains the winner for each field; `source`
is the strongest source that contributed to the record. Fresh provider listings
can change account-specific capabilities and limits, but never contribute pricing.

```toml
[models."vllm/custom-model"]
label = "Local model"
family = "qwen"
context_window_tokens = 32768
reasoning_options = [{ type = "budget_tokens", min = 1024, max = 8192 }]

[models."vllm/custom-model".capabilities]
tools = "unsupported"
reasoning = "supported"

[models."vllm/custom-model".cost]
input = 0.0
output = 0.0

[aliases]
local = "vllm/custom-model"
```

Malformed local overrides return an error. A corrupt cache falls back to the
vendored floor. `LLMSHIM_CATALOG_OFFLINE=1` pins to vendored + local data and
prevents all catalog and provider discovery HTTP, even an explicit refresh.

## Resident use

```rust,no_run
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
use llmshim_catalog::{CatalogHandle, CatalogOptions};

let catalog = CatalogHandle::load(CatalogOptions::default())?; // disk only
let refresh = catalog.refresh_in_background(); // detached, existing Tokio runtime
let snapshot = catalog.snapshot();             // does not wait for HTTP
let model = snapshot.resolve("openai/gpt-6-astra");
// Force a refresh from an operator action, independently of model requests:
let outcome = catalog.refresh(true).await;
# Ok(()) }
```

The TTL is 24 hours; errors serve the last snapshot indefinitely. An ETag is
sent only when its sidecar hash matches a validated cache body. Cache files are
atomically replaced under a process lock. Refresh HTTP has bounded size/time and
does not follow redirects. Holding a snapshot keeps a request's facts consistent
across background updates. `global()` provides a shared resident handle;
`resolve(id)` clones a model from its current snapshot. `Catalog::resolve(id)`
returns a borrowed model for applications already holding a snapshot.

`discover_provider(provider, models_url, bearer)` is explicit, on-demand
OpenAI-compatible `/models` discovery for self-hosted or entitlement-gated
providers. The token is only sent to that URL, never to models.dev or the cache.
Provider discoveries persist in the handle across subsequent catalog refreshes.

Remote imports are preflighted before model records and lookup indexes are
constructed. Each import has a ceiling of 32,768 rows and 128 MiB of estimated
identity and index bookkeeping, including repeated provider identifiers and
lookup aliases. Duplicate provider-listing rows consume this budget too; the
byte estimate can refuse an import before its row ceiling. Excessive imports
return a fixed error and preserve the previous snapshot, cached body, and
retained provider layers. Accepted identifiers use the existing canonicalization
and lookup alias rules.

This budget controls identity expansion after parsing. The decoded-body and
parsed-JSON limits separately bound input data; other model metadata, snapshots,
parser scratch and allocator overhead can coexist with the imported identities.
It is not a process memory limit.

Known aliases include `google/` → `gemini/`, dotted/dashed Claude versions, and
unambiguous regional profile names. Regional profile lookup retains the exact
wire ID; it never silently selects between two regions. Ambiguous bare names
return `None`, except for the verified direct-model spellings. Explicit local
aliases resolve ambiguity and cycle detection prevents unbounded traversal.

## Vendored data and release

`data/models.dev.json` is the unmodified public API snapshot from
[models.dev](https://models.dev/api.json), covered by `data/LICENSE.models.dev`.
The update workflow downloads a new snapshot, validates it, and proposes a PR;
it does not merge or publish automatically.

Verify an independently publishable package locally with:

```sh
cargo test -p llmshim-catalog
cargo package -p llmshim-catalog --allow-dirty
```

Publish this crate before a llmshim version that depends on it. Packaging is a
local operation; publication is a separate release action.
