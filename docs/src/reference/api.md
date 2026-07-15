# API references

Use the reference that matches your boundary:

| Boundary | Canonical reference |
|---|---|
| HTTP proxy | [`api/openapi.yaml`](https://github.com/sanjay920/llmshim/blob/main/api/openapi.yaml) |
| Rust crate | [`llmshim` on docs.rs](https://docs.rs/llmshim) |
| Python | [Client README](https://github.com/sanjay920/llmshim/blob/main/clients/python/README.md) |
| TypeScript | [Client README](https://github.com/sanjay920/llmshim/blob/main/clients/typescript/README.md) |
| Go | [Client README](https://github.com/sanjay920/llmshim/blob/main/clients/go/README.md) |
| Ruby | [Client README](https://github.com/sanjay920/llmshim/blob/main/clients/ruby/README.md) |

The proxy specification is OpenAPI 3.1 and currently reports llmshim version
`0.1.26`. It describes the four HTTP endpoints, compact request and response
schemas, typed SSE events, and documented HTTP errors. It does not describe the
Rust `serde_json::Value` contract or CLI workflow.

Open `api/openapi.yaml` in any OpenAPI 3.1-compatible viewer for interactive
schema browsing, or pass it to a compatible generator as the starting point
for another HTTP client. Generated code still needs your deployment's gateway
authentication and must respect the SSE and `Retry-After` behaviors described
in [HTTP API](../proxy/http-api.md) and [Errors and retries](errors.md).
