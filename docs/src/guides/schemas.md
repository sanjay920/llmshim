# Tool schema normalization

Every native adapter uses the same option-driven schema walker, after native
tool overrides. The target selects transport rules; schema logic is not copied
into each provider.

```rust
use llmshim::schema::{normalize_for, Target};

let mut schema = serde_json::json!({
    "type": "object",
    "properties": {"name": {"type": ["string", "null"]}}
});
let report = normalize_for(Target::Google, &mut schema);
assert!(!report.used_fallback);
assert_eq!(schema["properties"]["name"]["nullable"], true);
```

The walker upgrades draft-07 tuple/dependency forms, resolves document-local
references, embedded resources and anchors, removes unused definitions, and
normalizes snake-case schema keys. Snake-case wins when both spellings exist.
Property names and literal values in defaults/examples/enums are not treated as
schema keywords. External resources are never fetched.

Responses rewrites `oneOf` to `anyOf` and removes regex lookarounds. Google uses
nullable markers; Cloud Code Assist removes nullable markers and resolves
supported homogeneous combinators. Incompatible residual schemas use a per-tool
fallback: `{"type":"object","properties":{}}`. Other tools in the request remain
available. Circular/unresolved references, invalid shapes, and bounded expansion
failures take the same fallback path.

Strict mode sanitizes unsupported keywords, closes every object with
`additionalProperties:false`, makes every property required, and represents
optional properties as nullable alternatives. Human constraints such as defaults,
formats, patterns, bounds and dependencies are lifted into descriptions before
they are removed. A provider receives `strict:true` only after successful
enforcement. Keep an original schema if your application validates tool inputs;
wire sanitization can relax machine-enforced constraints.

Use `normalize_mcp_tool(&mut tool)` when ingesting an MCP `inputSchema` into a
registry. Raw MCP-style definitions (`name`, `description`, `inputSchema`) are
also accepted in the request's tools array and normalized before native dispatch.
MCP tool parameters must describe an object.

`normalize_output_for(target, schema, strict)` resolves references before
wrapping non-object output types under `response`. Its `OutputSchema::unwrap`
reverses that wrapper. The result reports whether normalization fell back; it
does not itself validate a model's generated instance or perform a repair call.
The HTTP [capability shim](capabilities.md) does both against the original schema.
`schema::validate::compile` exposes local-only instance validation separately.

Two escape hatches are available:

- `LLMSHIM_NO_SCHEMA_NORMALIZATION=1` bypasses schema rewriting and disables
  requested strict flags.
- `LLMSHIM_NO_STRICT=1` disables strict enforcement while retaining transport
  normalization.

The public `Options` API also permits an explicit bypass and depth/node/literal
budgets. Defaults bound expansion to depth 96, 32,768 schema nodes and 8 MiB of
literal/URI work, including repeated reference expansion. No network or
filesystem reads are part of schema normalization.


## Memoization

Normalization reuses a process-local result for the same schema content and
resolved `Options`, including the target, strict/nullable policies, rewrite flags,
and all budgets. Function names and descriptions outside the parameter schema
continue to come from the current tool definition. This covers provider tools,
MCP input schemas, and output-schema normalization through the same boundary.

The cache holds up to 512 entries and 16 MiB of estimated owned input/output bytes,
with least-recently-used eviction. Entries contain schemas and normalization
reports only; there is no disk persistence or response/prompt cache. Hash hits
also compare the input and options, including object order and signed zero, so a
collision cannot substitute another result. Returned values are independent of
cached values. Exact no-op hits leave the caller's value in place.

Schema walking and validation compilation run outside the cache lock. Fallback
results are cacheable too, because reference resolution never fetches external
resources. Environment overrides are resolved before lookup; changing
`LLMSHIM_NO_STRICT` or `LLMSHIM_NO_SCHEMA_NORMALIZATION` still takes effect.
Set `LLMSHIM_NO_SCHEMA_CACHE=1` to compare uncached behavior without disabling
normalization. Inputs beyond the cache's bounded key-work limits take the normal
uncached path and retain the normalizer's existing validation/fallback behavior.

The cache removes repeated normalization work. Whole-request copying, hashing,
and provider-specific translation still contribute to per-request cost. The
[release benchmark](../../../benchmarks/bench.rs) measures the full transform
at several tool counts; README results specify the workload and measurement scope.
