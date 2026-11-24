//! The batch ceiling a provider states for its embeddings wire.

use super::refusal;
use crate::error::Result;

/// The largest batch one request may carry: how many texts, and how many bytes
/// of text altogether.
///
/// A provider whose own API caps a batch states its cap here. The default is
/// llmshim's own ceiling and claims nothing about a server: it stops a runaway
/// batch, and a server that refuses the batch anyway answers with its own 400,
/// classified like every other upstream error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingBounds {
    pub max_texts: usize,
    pub max_bytes: usize,
}

impl EmbeddingBounds {
    pub const fn new(max_texts: usize, max_bytes: usize) -> Self {
        Self {
            max_texts,
            max_bytes,
        }
    }

    pub(super) fn allows(&self, texts: &[&str]) -> Result<()> {
        let bytes = texts
            .iter()
            .fold(0usize, |total, text| total.saturating_add(text.len()));
        if texts.len() <= self.max_texts && bytes <= self.max_bytes {
            return Ok(());
        }
        Err(refusal(format!(
            "embeddings batch of {} texts / {bytes} bytes exceeds this provider's limit of {} texts / {} bytes",
            texts.len(),
            self.max_texts,
            self.max_bytes
        )))
    }
}

impl Default for EmbeddingBounds {
    /// 8192 texts and 4 MiB, for a provider that publishes no cap llmshim can
    /// hold it to. Wide enough for any batch a caller would send on purpose,
    /// narrow enough that a chunking loop gone wrong is refused locally.
    fn default() -> Self {
        Self::new(8192, 4 << 20)
    }
}
