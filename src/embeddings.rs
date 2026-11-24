//! Embeddings: one vector per text, from the providers whose API has them.
//!
//! Requests flow as an OpenAI-shaped body (`input`, `dimensions`) that each
//! adapter translates into its native wire — OpenAI's `/v1/embeddings`, Gemini's
//! `batchEmbedContents`, an OpenAI-compatible server's `/v1/embeddings` — and
//! the answer is normalized back to one vector per input, in input order.
//!
//! A provider whose API has no embeddings route refuses by name rather than
//! forwarding the batch somewhere else: a vector from a model the caller did
//! not name is the wrong answer with no signal that anything was substituted.
//!
//! Before anything is sent, three things are checked locally and in this order:
//! the batch against the provider's bounds (a server could not accept it, so it
//! must not be rendered into a native body), the provider itself (an adapter
//! with no embeddings API refuses by name), and the catalog's embedding
//! assertion for the model. All three are refusals, not adjustments — a batch
//! llmshim split or a model llmshim picked would both be work the caller did
//! not ask for.

use crate::catalog::Support;
use crate::error::{Result, ShimError};
use crate::provider::Provider;
use serde_json::{json, Value};

mod bounds;

pub use bounds::EmbeddingBounds;

/// What to embed: a batch of texts for one model, optionally at a narrower
/// output width than the model's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingRequest {
    /// A model id in the router's `provider/model` grammar.
    pub model: String,
    /// The texts, one vector each. An empty batch is refused.
    pub input: Vec<String>,
    /// Output width. `None` keeps the model's native width, which is what a
    /// vector store sized for that model wants. A width wider than the model's
    /// is refused before dispatch.
    pub dimensions: Option<u32>,
}

impl EmbeddingRequest {
    pub fn new(model: impl Into<String>, input: Vec<String>) -> Self {
        Self {
            model: model.into(),
            input,
            dimensions: None,
        }
    }

    /// Ask for `dimensions`-wide vectors. Models that truncate accept fewer
    /// than their native width — OpenAI's `text-embedding-3-*`, Gemini's
    /// `outputDimensionality` — and none accept more.
    pub fn with_dimensions(mut self, dimensions: u32) -> Self {
        self.dimensions = Some(dimensions);
        self
    }
}

/// One vector per input text, with the identity of what produced them.
#[derive(Debug, Clone, PartialEq)]
pub struct Embeddings {
    /// The provider key the router resolved the model id to.
    pub provider: String,
    /// The model name sent upstream, after the catalog resolved the id.
    pub model: String,
    /// The model the provider's own response named, when it named one: the
    /// pinned snapshot the request landed on rather than the alias it was
    /// addressed with, which is what a stored vector's identity should record.
    /// `None` means the provider named none — Gemini's `batchEmbedContents`
    /// response carries no model.
    pub revision: Option<String>,
    /// One vector per input text, in input order.
    pub vectors: Vec<Vec<f32>>,
    pub usage: EmbeddingUsage,
}

/// Tokens and money for one embeddings call. Both are optional because a
/// provider may report neither, and an unreported count is unknown, not zero.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingUsage {
    /// Provider-reported input tokens. Embeddings have no cache and no output,
    /// so the prompt total is the whole bill.
    pub input_tokens: Option<u64>,
    /// USD, from the provider's own bill where it reported one, else the
    /// catalog's price for `input_tokens`. `None` is unknown, not free.
    pub cost_usd: Option<f64>,
    /// `provider`, `provider_floor` or `catalog`; see [`crate::cost`].
    pub cost_source: Option<String>,
}

/// The llmshim-internal embedding payload a request renders to: `input`, and
/// `dimensions` when the caller asked for one. `model` travels separately, as
/// it does for a completion.
pub(crate) fn payload(request: &EmbeddingRequest) -> Value {
    let mut body = json!({ "input": request.input });
    if let Some(dimensions) = request.dimensions {
        body["dimensions"] = json!(dimensions);
    }
    body
}

/// The texts of an embedding payload, refused unless the payload holds an array
/// of strings.
pub(crate) fn texts(request: &Value) -> Result<Vec<&str>> {
    request["input"]
        .as_array()
        .filter(|texts| texts.iter().all(Value::is_string))
        .map(|texts| texts.iter().filter_map(Value::as_str).collect())
        .ok_or_else(|| refusal("embeddings request needs an input array of strings".into()))
}

/// The output width the caller asked for, when there is one.
pub(crate) fn dimensions(request: &Value) -> Option<u32> {
    u32::try_from(request["dimensions"].as_u64()?).ok()
}

/// The OpenAI embeddings body: what both the OpenAI adapter and a
/// Chat-Completions-compatible server's `/v1/embeddings` are sent, since the
/// endpoint is the same one on both.
pub(crate) fn openai_body(model: &str, texts: &[&str], dimensions: Option<u32>) -> Value {
    let mut body = json!({ "model": model, "input": texts });
    if let Some(dimensions) = dimensions {
        body["dimensions"] = json!(dimensions);
    }
    body
}

/// The batch itself: an array of non-empty strings inside the provider's
/// bounds. Cheap and provider-independent, so it runs before anything else —
/// a batch a server could never accept must not be rendered into a native body
/// first.
pub(crate) fn admit_batch(provider: &dyn Provider, request: &Value) -> Result<()> {
    let texts = texts(request)?;
    if texts.is_empty() {
        return Err(refusal("embeddings request needs at least one text".into()));
    }
    provider.embedding_bounds().allows(&texts)
}

/// The model and the requested width against what the catalog asserts.
///
/// This runs after the provider has accepted the request shape, because the
/// provider's own "I have no embeddings API" is the fact that decides whether
/// any model id could work here — and no catalog override can give a provider
/// an endpoint.
pub(crate) fn admit_model(provider: &dyn Provider, model: &str, request: &Value) -> Result<()> {
    let id = format!("{}/{}", provider.name(), model);
    // No catalog row is silence, not a denial: a self-hosted server's embedding
    // model is in no catalog, and the provider is the authority on what it
    // serves. A row the catalog does carry is an assertion, and this one has to
    // be about embedding.
    let Some(info) = crate::catalog::resolve(&id) else {
        return Ok(());
    };
    if info.embeds != Support::Supported {
        return Err(refusal(format!(
            "{id} is not marked as an embedding model in the catalog; assert `embedding = true` under [models.\"{id}\"] to embed with it"
        )));
    }
    if let Some(asked) = dimensions(request) {
        if let Some(native) = info.embedding_dimensions {
            if asked > native {
                return Err(refusal(format!(
                    "dimensions {asked} exceeds the {native} dimensions {id} was trained at"
                )));
            }
        }
    }
    Ok(())
}

/// Turn a provider-normalized embeddings body into vectors.
///
/// `expected_texts` is the size of the batch that was sent. A body with any
/// other number of vectors is refused rather than returned: vectors that do not
/// pair one-to-one with the texts are indistinguishable from correct ones at
/// the call site, which is the one failure a caller cannot detect.
pub(crate) fn parse(
    provider: &str,
    model: &str,
    expected_texts: usize,
    response: Value,
) -> Result<Embeddings> {
    let Some(data) = response["data"].as_array() else {
        return Err(invalid("embeddings response has no data array".into()));
    };
    if data.len() != expected_texts {
        return Err(invalid(format!(
            "provider returned {} vectors for {expected_texts} texts",
            data.len()
        )));
    }
    // A provider that numbers its vectors is taken at its word; one that does
    // not keeps the order it answered in. The sort is stable, so the two cases
    // are one code path.
    let mut numbered: Vec<(u64, &Value)> = data
        .iter()
        .enumerate()
        .map(|(position, item)| (item["index"].as_u64().unwrap_or(position as u64), item))
        .collect();
    numbered.sort_by_key(|(index, _)| *index);

    let mut vectors = Vec::with_capacity(numbered.len());
    for (index, item) in numbered {
        let Some(values) = item["embedding"].as_array() else {
            return Err(invalid(format!("vector {index} is not an array")));
        };
        let mut vector = Vec::with_capacity(values.len());
        for value in values {
            let Some(number) = value.as_f64() else {
                return Err(invalid(format!("vector {index} holds a non-numeric value")));
            };
            let narrow = number as f32;
            if !narrow.is_finite() {
                return Err(invalid(format!(
                    "vector {index} holds {number}, which is not a finite f32"
                )));
            }
            vector.push(narrow);
        }
        vectors.push(vector);
    }

    let usage = &response["usage"];
    Ok(Embeddings {
        provider: provider.to_string(),
        model: model.to_string(),
        revision: response["model"]
            .as_str()
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        vectors,
        usage: EmbeddingUsage {
            input_tokens: input_tokens(usage),
            cost_usd: crate::cost::stamped(usage),
            cost_source: crate::cost::stamped_source(usage).map(str::to_owned),
        },
    })
}

fn input_tokens(usage: &Value) -> Option<u64> {
    ["prompt_tokens", "input_tokens"]
        .into_iter()
        .find_map(|key| usage.get(key).and_then(Value::as_u64))
}

/// A provider whose API has no embeddings route refuses by name. Forwarding the
/// batch to some other vendor's model would answer a question nobody asked.
pub(crate) fn absent(provider: &str) -> ShimError {
    refusal(format!("{provider} has no embeddings API"))
}

/// A caller-side refusal: the request cannot be sent as asked.
fn refusal(body: String) -> ShimError {
    ShimError::ProviderError {
        status: 400,
        body,
        retry_after: None,
    }
}

/// The provider answered with something that is not an embeddings response.
fn invalid(body: String) -> ShimError {
    ShimError::ProviderError {
        status: 502,
        body,
        retry_after: None,
    }
}
