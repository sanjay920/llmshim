# Embeddings

Embeddings return one vector per input text, in input order, with the model
identity and the tokens the provider billed.

> **Availability:** Rust: `llmshim::embeddings` · proxy/clients: not exposed yet

## Embed a batch from Rust

```rust
use llmshim::embeddings::EmbeddingRequest;

let router = llmshim::router::Router::from_env();
let request = EmbeddingRequest::new(
    "openai/text-embedding-3-small",
    vec!["the cat sat on the mat".to_string(), "a dog barked".to_string()],
);

let batch = llmshim::embeddings(&router, &request).await.unwrap();
assert_eq!(batch.vectors.len(), 2);
println!("{} {} {:?}", batch.provider, batch.model, batch.revision);
```

The model id is the same `provider/model` grammar a completion uses, so a named
route, an alias and provider prefix inference all work the same way.
`batch.provider` and `batch.model` name what produced the vectors;
`batch.revision` is the model string the provider's own response reported — the
pinned snapshot the request landed on — and is `None` when the provider's
response names no model.

`batch.usage.input_tokens` is the provider's own count, and
`batch.usage.cost_usd` / `cost_source` follow the same rules as a completion:
the provider's bill where there is one, else the catalog's price, else `null`
for unknown — never `0` for "we did not find out".

### Narrower vectors

`EmbeddingRequest::with_dimensions` asks for a smaller vector on models that
truncate:

```rust
let request = EmbeddingRequest::new("gemini/gemini-embedding-001", texts)
    .with_dimensions(512);
```

A width wider than the model's own is refused before dispatch: no provider
widens, and a silent failure upstream costs a round trip for something llmshim
can see locally.

## Which providers embed

| Provider | Wire |
|---|---|
| OpenAI | `POST /v1/embeddings`, with `dimensions` and `encoding_format: "float"` |
| Gemini | `POST /v1beta/models/{model}:batchEmbedContents` |
| An OpenAI-compatible server (`vllm/…`, `sglang/…`) | `POST <base>/embeddings`, faithful passthrough |
| Anthropic, xAI, OpenRouter, ChatGPT subscription | no embeddings route — refused by name |

A refusal is local and names the provider, e.g. `anthropic has no embeddings
API`. llmshim never forwards the batch to another vendor's model: a vector from
a model the caller did not name is the wrong answer with no signal that a
substitution happened.

Any server that speaks OpenAI's `/v1/embeddings` — a local Ollama, LM Studio,
vLLM or SGLang instance — is reached through the self-hosted provider already
configured for it by base URL. Since that server's own limits are its launch
configuration, llmshim passes the batch through and surfaces the server's error.

## The catalog decides which models embed

A model the catalog carries but does not mark as an embedding model is refused
before anything is sent, naming the model:

```
openai/gpt-6-astra is not marked as an embedding model in the catalog;
assert `embedding = true` under [models."openai/gpt-6-astra"] to embed with it
```

That is deliberate. An OpenAI chat model sent to `/v1/embeddings` fails
upstream; the catalog assertion turns a wasted round trip into a local answer,
and the state is tri-state — a model the catalog has never heard of (a
self-hosted embedding model, an OpenRouter slug) is sent, because the provider
is the authority on what it serves.

The assertion comes from the layer that knew:

```toml
# ~/.llmshim/config.toml
[models."vllm/bge-m3"]
embedding = true
dimensions = 1024
```

Builtin assertions cover `openai/text-embedding-3-small`,
`openai/text-embedding-3-large`, `openai/text-embedding-ada-002` and
`gemini/gemini-embedding-001`. A local entry outranks them, so a private
endpoint can assert its own model and width.

## Batching

A batch over the provider's bounds is refused rather than split: a second HTTP
request is a second charge and a different rate-limit footprint from the one the
caller asked for. OpenAI's documented 2048-input cap and a 1 MiB text ceiling
apply there; every other provider gets llmshim's own wider ceiling (8192 texts,
4 MiB), which is a guard against a runaway batch rather than a claim about any
server's real limit. Check `Provider::embedding_bounds` on the provider you are
addressing if you need the number.

Split the batch yourself and issue one call per chunk when you have more texts
than the bound — the vectors come back in input order, so concatenating chunks
preserves the mapping to your texts.
