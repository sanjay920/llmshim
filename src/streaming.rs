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

pub use crate::stream_retention::StreamRetentionLimits;

pub(crate) fn append_string_fragment(destination: &mut Value, fragment: &str) {
    match destination {
        Value::String(assembled) => assembled.push_str(fragment),
        destination => *destination = Value::String(fragment.to_owned()),
    }
}

pub struct StreamNormalizer {
    target: ReplayTarget,
    tools: ToolStream,
    usage: StreamUsage,
    seen_terminal: bool,
    seen_refusal: bool,
    reasoning: crate::reasoning::ReasoningAccumulator,
    observed_integrity: bool,
    active_choices: std::collections::BTreeSet<u64>,
    seen_choices: std::collections::BTreeSet<u64>,
    budget: crate::stream_retention::RetainedBudget,
    finished: bool,
}

impl StreamNormalizer {
    pub fn new(target: ReplayTarget) -> Self {
        Self::with_retention_limits(target, StreamRetentionLimits::default())
            .expect("valid default stream retention limits")
    }

    pub fn with_retention_limits(
        target: ReplayTarget,
        limits: StreamRetentionLimits,
    ) -> Result<Self> {
        let limits = StreamRetentionLimits::new(
            limits.normalizer_bytes,
            limits.normalizer_entries,
            limits.native_usage_bytes,
            limits.native_usage_entries,
        )?;
        let budget = crate::stream_retention::RetainedBudget::new(
            limits.normalizer_bytes,
            limits.normalizer_entries,
        );
        Ok(Self {
            tools: ToolStream::with_budget(target.clone(), budget.clone()),
            target,
            usage: StreamUsage::with_budget(budget.clone()),
            seen_terminal: false,
            seen_refusal: false,
            reasoning: crate::reasoning::ReasoningAccumulator::with_budget(budget.clone()),
            observed_integrity: false,
            active_choices: std::collections::BTreeSet::new(),
            seen_choices: std::collections::BTreeSet::new(),
            budget,
            finished: false,
        })
    }
    pub fn is_finished(&self) -> bool {
        self.finished
    }
    pub(crate) fn abort(&mut self) {
        self.finished = true;
        self.clear_retained();
    }

    pub fn push(&mut self, data: &str) -> Result<Option<String>> {
        let result = self.push_inner(data);
        if result.is_err() {
            self.finished = true;
            self.clear_retained();
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
        let mut native: Value =
            match crate::json_bounds::parse_str(data, crate::json_bounds::Limits::SSE) {
                Ok(value) => value,
                Err(crate::json_bounds::ParseError::Malformed(error)) => return Err(error.into()),
                Err(crate::json_bounds::ParseError::Complexity) => {
                    return Err(ShimError::Stream(
                        "upstream JSON exceeds complexity limit".into(),
                    ))
                }
            };
        if matches!(native["type"].as_str(), Some("error" | "response.failed"))
            || native.get("error").is_some_and(|v| !v.is_null())
        {
            self.finished = true;
            return Err(ShimError::Stream("upstream stream failed".into()));
        }
        if self.target.wire == WireFormat::AnthropicMessages {
            self.usage.ingest("anthropic", &mut native)?;
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
            if self.seen_choices.insert(index) {
                self.budget
                    .reserve(crate::stream_retention::RetainedFootprint::record(0))?;
            }
            if choice["finish_reason"].is_string() {
                self.seen_terminal = true;
                if self.active_choices.remove(&index) {
                    self.budget
                        .release(crate::stream_retention::RetainedFootprint::record(0));
                }
            } else if choice["delta"].as_object().is_some_and(|d| !d.is_empty())
                && self.active_choices.insert(index)
            {
                self.budget
                    .reserve(crate::stream_retention::RetainedFootprint::record(0))?;
            }
        }
        crate::reasoning::bind_response_context(&mut value, &self.target);
        if self.target.wire == WireFormat::AnthropicMessages {
            self.reasoning.push(&value["choices"][0]["delta"])?;
            if self.seen_terminal && !self.observed_integrity {
                self.observed_integrity = true;
                let mut observation = serde_json::json!({"choices":[{"message":{"reasoning":self.reasoning.take_blocks()}}]});
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
        if let Err(error) = self.tools.finish() {
            self.clear_retained();
            return Err(error);
        }
        if !self.seen_terminal || !self.active_choices.is_empty() {
            self.clear_retained();
            return Err(ShimError::Stream(
                "stream ended before a terminal response".into(),
            ));
        }
        let terminal = self.usage.take_terminal();
        self.clear_retained();
        Ok(terminal)
    }

    fn clear_retained(&mut self) {
        self.tools.clear();
        self.usage.clear();
        self.reasoning.clear();
        self.active_choices.clear();
        self.seen_choices.clear();
        self.budget.reset();
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;

    #[test]
    fn retained_state_is_released_immediately_after_budget_failure() {
        let limits = StreamRetentionLimits::new(768, 64, 4 * 1024, 64).unwrap();
        let mut stream = StreamNormalizer::with_retention_limits(
            ReplayTarget::new("openai", "gpt-5.4", WireFormat::OpenAiResponses),
            limits,
        )
        .unwrap();
        stream
            .push(&serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"synthetic","name":"read","arguments":""}}).to_string())
            .unwrap();
        let error = (0..16)
            .find_map(|_| {
                stream
                    .push(&serde_json::json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}).to_string())
                    .err()
            })
            .expect("the tiny budget must fail");
        assert_eq!(
            error.to_string(),
            format!("stream error: {}", crate::stream_retention::RETENTION_ERROR)
        );
        assert_eq!(stream.budget.retained(), Default::default());
        assert!(stream.active_choices.is_empty());
        assert!(stream.seen_choices.is_empty());
    }

    #[test]
    fn atomic_tool_arguments_are_bounded_before_completeness_parse() {
        let arguments = format!(
            "[{}]",
            std::iter::repeat_n("0", 20_000)
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut stream = StreamNormalizer::new(ReplayTarget::new(
            "openrouter",
            "vendor/model",
            WireFormat::OpenAiChat,
        ));
        let error = stream
            .push(
                &serde_json::json!({
                    "choices":[{"index":0,"delta":{"tool_calls":[{
                        "id":"call-1",
                        "type":"function",
                        "function":{"name":"read","arguments":arguments}
                    }]}}]
                })
                .to_string(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("complexity limit"));
        assert_eq!(stream.budget.retained(), Default::default());
    }
}
