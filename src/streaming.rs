//! Per-response stream normalization, shared by HTTP dispatch and manual SSE
//! consumers. Tool arguments, ids and signatures are assembled before exposure.
use crate::{
    error::{Result, ShimError},
    provider::Provider,
    reasoning::{ReplayTarget, WireFormat},
    toolcall::ToolStream,
    usage::StreamUsage,
};
use serde_json::Value;

pub struct StreamNormalizer {
    target: ReplayTarget,
    tools: ToolStream,
    usage: StreamUsage,
    seen_terminal: bool,
    seen_refusal: bool,
    reasoning: crate::reasoning::ReasoningAccumulator,
    observed_integrity: bool,
    active_choices: std::collections::BTreeSet<u64>,
    finished: bool,
}
impl StreamNormalizer {
    pub fn new(target: ReplayTarget) -> Self {
        Self {
            tools: ToolStream::new(target.clone()),
            target,
            usage: StreamUsage::default(),
            seen_terminal: false,
            seen_refusal: false,
            reasoning: crate::reasoning::ReasoningAccumulator::default(),
            observed_integrity: false,
            active_choices: std::collections::BTreeSet::new(),
            finished: false,
        }
    }
    pub fn is_finished(&self) -> bool {
        self.finished
    }
    pub(crate) fn abort(&mut self) {
        self.finished = true;
    }

    pub fn push(&mut self, data: &str) -> Result<Option<String>> {
        let result = self.push_inner(data);
        if result.is_err() {
            self.finished = true;
        }
        result
    }

    fn push_inner(&mut self, data: &str) -> Result<Option<String>> {
        if self.finished {
            return Err(ShimError::Stream("stream already ended".into()));
        }
        if data.trim() == "[DONE]" {
            return self.finish();
        }
        if data.trim().is_empty() {
            return Ok(None);
        }
        let mut native: Value = serde_json::from_str(data)?;
        if matches!(native["type"].as_str(), Some("error" | "response.failed"))
            || native.get("error").is_some_and(|v| !v.is_null())
        {
            self.finished = true;
            return Err(ShimError::Stream("upstream stream failed".into()));
        }
        if self.target.wire == WireFormat::AnthropicMessages {
            self.usage.ingest("anthropic", &mut native);
        }
        let data = native.to_string();
        let parsed = if self.target.provider == "chatgpt"
            && self.target.wire == WireFormat::OpenAiResponses
        {
            crate::providers::chatgpt::parse_stream_chunk(&self.target.model, &data)
                .and_then(|chunk| crate::reasoning::capture_stream(&self.target, &native, chunk))?
        } else {
            match self.target.wire {
                WireFormat::AnthropicMessages => {
                    crate::providers::anthropic::Anthropic::new(String::new())
                        .transform_stream_chunk(&self.target.model, &data)?
                }
                WireFormat::GoogleGenerateContent => {
                    crate::providers::gemini::Gemini::new(String::new())
                        .transform_stream_chunk(&self.target.model, &data)?
                }
                WireFormat::OpenAiResponses if self.target.provider == "xai" => {
                    crate::providers::xai::Xai::new(String::new())
                        .transform_stream_chunk(&self.target.model, &data)?
                }
                WireFormat::OpenAiResponses => crate::providers::openai::OpenAi::new(String::new())
                    .transform_stream_chunk(&self.target.model, &data)?,
                WireFormat::OpenAiChat if self.target.provider == "openrouter" => {
                    crate::providers::openrouter::OpenRouter::new(String::new())
                        .transform_stream_chunk(&self.target.model, &data)?
                }
                WireFormat::OpenAiChat => crate::providers::openai_compat::OpenAiCompatible::new(
                    &self.target.provider,
                    "",
                    None,
                )
                .transform_stream_chunk(&self.target.model, &data)?,
            }
        };
        let Some(chunk) = self.tools.push(&native, parsed)? else {
            return Ok(None);
        };
        let mut value: Value = serde_json::from_str(&chunk)?;
        if self.target.wire == WireFormat::OpenAiResponses {
            if value
                .pointer("/choices/0/delta/refusal")
                .and_then(Value::as_str)
                .is_some()
            {
                self.seen_refusal = true;
            }
            if matches!(
                native["type"].as_str(),
                Some("response.completed" | "response.incomplete")
            ) {
                let refusal: String = native["response"]["output"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|item| item["type"] == "message")
                    .flat_map(|item| item["content"].as_array().into_iter().flatten())
                    .filter_map(|part| part["refusal"].as_str())
                    .collect();
                if !refusal.is_empty() && !self.seen_refusal {
                    value["choices"][0]["delta"]["refusal"] = serde_json::json!(refusal);
                    self.seen_refusal = true;
                }
                if self.seen_refusal {
                    value["choices"][0]["finish_reason"] = serde_json::json!("content_filter");
                }
            }
        }
        for (i, choice) in value["choices"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let index = choice["index"].as_u64().unwrap_or(i as u64);
            if choice["finish_reason"].is_string() {
                self.seen_terminal = true;
                self.active_choices.remove(&index);
            } else if choice["delta"].as_object().is_some_and(|d| !d.is_empty()) {
                self.active_choices.insert(index);
            }
        }
        crate::reasoning::bind_response_context(&mut value, &self.target);
        if self.target.wire == WireFormat::AnthropicMessages {
            self.reasoning.push(&value["choices"][0]["delta"]);
            if self.seen_terminal && !self.observed_integrity {
                self.observed_integrity = true;
                let mut observation = serde_json::json!({"choices":[{"message":{"reasoning":self.reasoning.blocks()}}]});
                crate::providers::anthropic_signature::observe(
                    &mut observation,
                    &self.target.model,
                );
                if let Some(served) = observation.get("x-llmshim-served-model") {
                    value["x-llmshim-served-model"] = served.clone();
                }
            }
        }
        crate::toolcall::bind_response_context(&mut value, &self.target);
        let data = value.to_string();
        if self.target.wire == WireFormat::OpenAiChat {
            self.usage.defer_chat_terminal(data).map(Some)
        } else {
            Ok(Some(data))
        }
    }

    pub fn finish(&mut self) -> Result<Option<String>> {
        if self.finished {
            return Ok(None);
        }
        self.finished = true;
        self.tools.finish()?;
        if !self.seen_terminal || !self.active_choices.is_empty() {
            return Err(ShimError::Stream(
                "stream ended before a terminal response".into(),
            ));
        }
        Ok(self.usage.take_terminal())
    }
}
