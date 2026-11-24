use chrono::Utc;
use futures::Stream;
use serde::Serialize;
use serde_json::Value;
use std::io::Write;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

/// A single log entry for an LLM request.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub ts: String,
    pub model: String,
    pub provider: String,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    /// USD accounting for this response. A provider floor is a lower bound;
    /// `null` means unknown and never means free.
    pub cost_usd: Option<f64>,
    /// Where `cost_usd` came from: `"provider"` for an exact terminal bill,
    /// `"provider_floor"` for the highest partial provider bill observed (a
    /// lower bound), or `"catalog"` for an estimate.
    pub cost_source: Option<String>,
    pub status: String,
    pub gateway_integrity: crate::providers::anthropic_signature::Integrity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl LogEntry {
    /// Extract token counts from a normalized OpenAI-format response.
    pub fn from_response(
        provider: &str,
        model: &str,
        response: &Value,
        latency: std::time::Duration,
    ) -> Self {
        let usage = response
            .get("usage")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let (cost, cost_source) = crate::cost::attribute(provider, model, &usage);
        Self {
            ts: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            model: model.to_string(),
            provider: provider.to_string(),
            latency_ms: latency.as_millis() as u64,
            input_tokens: usage
                .get("prompt_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            output_tokens: usage
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            reasoning_tokens: usage
                .get("reasoning_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            total_tokens: usage
                .get("total_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            status: "ok".to_string(),
            gateway_integrity: match response["x-llmshim-served-model"].as_str() {
                Some(served) => crate::providers::anthropic_signature::Integrity::Mismatch {
                    requested: model.into(),
                    served: served.split(',').map(str::to_owned).collect(),
                },
                None => crate::providers::anthropic_signature::inspect(response, model),
            },
            cache_read_tokens: usage["cache_read_tokens"].as_u64().unwrap_or(0),
            cache_write_tokens: usage["cache_write_tokens"].as_u64().unwrap_or(0),
            // Priced at the transport boundary, which is the only place that
            // still knows the dispatch target. Fall back to pricing here when a
            // caller hands us an unstamped response.
            cost_usd: cost,
            cost_source,
            error: None,
            request_id: response
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from),
        }
    }

    pub fn from_error(
        provider: &str,
        model: &str,
        error: &str,
        latency: std::time::Duration,
    ) -> Self {
        Self {
            ts: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            model: model.to_string(),
            provider: provider.to_string(),
            latency_ms: latency.as_millis() as u64,
            input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 0,
            cost_usd: None,
            cost_source: None,
            status: "error".to_string(),
            gateway_integrity: crate::providers::anthropic_signature::Integrity::Unknown,
            error: Some(error.to_string()),
            request_id: None,
        }
    }
}

/// Logger that writes JSONL to a writer (file or stdout).
#[derive(Clone)]
pub struct Logger {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Logger {
    /// Create a logger that writes to a file.
    pub fn to_file(path: &str) -> std::io::Result<Self> {
        let mut file_options = std::fs::OpenOptions::new();
        file_options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file_options.mode(0o600);
        }
        let log_file = file_options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            log_file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self {
            writer: Arc::new(Mutex::new(Box::new(log_file))),
        })
    }

    /// Create a logger that writes to stderr (won't interfere with CLI output).
    pub fn to_stderr() -> Self {
        Self {
            writer: Arc::new(Mutex::new(Box::new(std::io::stderr()))),
        }
    }

    pub fn log(&self, entry: &LogEntry) {
        if let Ok(json) = serde_json::to_string(entry) {
            if let Ok(mut writer) = self.writer.lock() {
                let _ = writeln!(writer, "{}", json);
            }
        }
    }

    /// Wrap a provider chunk stream so at most one entry is written for it.
    ///
    /// A chunk whose parsed JSON carries an object `usage` is remembered, and the
    /// last one seen is logged as a success once the stream is finished with —
    /// whether it ran to its end or its reader stopped early, as the native
    /// facade does at the `done` event. An error chunk logs one error entry
    /// instead. A stream that saw no usage object logs nothing, so an
    /// unaccounted stream never fabricates a zero-token line.
    pub fn wrap_stream(
        self,
        provider: &str,
        model: &str,
        started: RequestTimer,
        upstream: Pin<
            Box<dyn Stream<Item = std::result::Result<String, crate::error::ShimError>> + Send>,
        >,
    ) -> Pin<Box<dyn Stream<Item = std::result::Result<String, crate::error::ShimError>> + Send>>
    {
        let entry = StreamEntry {
            logger: self,
            provider: provider.to_string(),
            model: model.to_string(),
            started,
            usage: None,
            written: false,
        };
        Box::pin(Metered { upstream, entry })
    }
}

/// A chunk stream that owes one log entry: written when the upstream ends, or
/// when the stream is dropped first — a reader that stops at the final event
/// never polls it to its end.
struct Metered {
    upstream:
        Pin<Box<dyn Stream<Item = std::result::Result<String, crate::error::ShimError>> + Send>>,
    entry: StreamEntry,
}

impl Stream for Metered {
    type Item = std::result::Result<String, crate::error::ShimError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let polled = this.upstream.as_mut().poll_next(cx);
        match &polled {
            Poll::Ready(Some(Ok(chunk))) => this.entry.saw(chunk),
            Poll::Ready(Some(Err(error))) => this.entry.failed(&error.to_string()),
            Poll::Ready(None) => this.entry.finish(),
            Poll::Pending => {}
        }
        polled
    }
}

/// The one log entry a wrapped stream owes, and whether it has been written.
struct StreamEntry {
    logger: Logger,
    provider: String,
    model: String,
    started: RequestTimer,
    usage: Option<Value>,
    written: bool,
}

impl StreamEntry {
    /// Remember `chunk` when it carries a usage object — read from the parsed
    /// chunk, never its text.
    fn saw(&mut self, chunk: &str) {
        if let Ok(parsed) = serde_json::from_str::<Value>(chunk) {
            if parsed.get("usage").is_some_and(Value::is_object) {
                self.usage = Some(parsed);
            }
        }
    }

    /// One error line for the stream, however many errors it yields.
    fn failed(&mut self, error: &str) {
        if self.written {
            return;
        }
        self.written = true;
        self.logger.log(&LogEntry::from_error(
            &self.provider,
            &self.model,
            error,
            self.started.elapsed(),
        ));
    }

    /// The success line for the last usage seen, once; nothing if none was seen.
    fn finish(&mut self) {
        if std::mem::replace(&mut self.written, true) {
            return;
        }
        if let Some(usage) = &self.usage {
            self.logger.log(&LogEntry::from_response(
                &self.provider,
                &self.model,
                usage,
                self.started.elapsed(),
            ));
        }
    }
}

impl Drop for StreamEntry {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Timer helper — start before request, finish after.
pub struct RequestTimer {
    start: Instant,
}

impl RequestTimer {
    pub fn start() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.start.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{stream, StreamExt};

    fn lines(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn error() -> crate::error::ShimError {
        crate::error::ShimError::Stream("upstream failed".into())
    }

    #[tokio::test]
    async fn wrap_stream_logs_one_entry_from_the_terminal_usage_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.jsonl");
        let logger = Logger::to_file(path.to_str().unwrap()).unwrap();
        let chunks = vec![
            Ok("{\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}".to_string()),
            Ok(
                "{\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6},\"id\":\"r1\"}"
                    .to_string(),
            ),
        ];
        let mut wrapped = logger.wrap_stream(
            "openai",
            "gpt-5.4",
            RequestTimer::start(),
            Box::pin(stream::iter(chunks)),
        );
        let mut seen = 0;
        while wrapped.next().await.is_some() {
            seen += 1;
        }
        assert_eq!(seen, 2, "the wrapper must forward every chunk unchanged");
        let lines = lines(&path);
        assert_eq!(lines.len(), 1, "exactly one log line");
        assert_eq!(lines[0]["input_tokens"], 4);
        assert_eq!(lines[0]["output_tokens"], 2);
        assert_eq!(lines[0]["total_tokens"], 6);
        assert_eq!(lines[0]["request_id"], "r1");
        assert_eq!(lines[0]["status"], "ok");
    }

    #[tokio::test]
    async fn wrap_stream_logs_nothing_without_a_usage_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.jsonl");
        let logger = Logger::to_file(path.to_str().unwrap()).unwrap();
        let chunks = vec![
            Ok("{\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}".to_string()),
            Ok("{\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}".to_string()),
        ];
        let mut wrapped = logger.wrap_stream(
            "openai",
            "gpt-5.4",
            RequestTimer::start(),
            Box::pin(stream::iter(chunks)),
        );
        while wrapped.next().await.is_some() {}
        assert!(lines(&path).is_empty(), "no usage object means no log line");
    }

    #[tokio::test]
    async fn wrap_stream_logs_one_error_entry_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.jsonl");
        let logger = Logger::to_file(path.to_str().unwrap()).unwrap();
        let chunks: Vec<std::result::Result<String, crate::error::ShimError>> = vec![
            Ok("{\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}".to_string()),
            Err(error()),
        ];
        let mut wrapped = logger.wrap_stream(
            "openai",
            "gpt-5.4",
            RequestTimer::start(),
            Box::pin(stream::iter(chunks)),
        );
        let mut errors = 0;
        while let Some(item) = wrapped.next().await {
            if item.is_err() {
                errors += 1;
            }
        }
        assert_eq!(errors, 1);
        let lines = lines(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["status"], "error");
        assert!(lines[0]["error"]
            .as_str()
            .is_some_and(|e| e.contains("upstream failed")));
    }

    #[tokio::test]
    async fn wrap_stream_logs_the_last_usage_object_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.jsonl");
        let logger = Logger::to_file(path.to_str().unwrap()).unwrap();
        let chunks = vec![
            Ok("{\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}".to_string()),
            Ok("{\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":7,\"total_tokens\":12},\"id\":\"final\"}".to_string()),
        ];
        let mut wrapped = logger.wrap_stream(
            "openai",
            "gpt-5.4",
            RequestTimer::start(),
            Box::pin(stream::iter(chunks)),
        );
        while wrapped.next().await.is_some() {}
        let lines = lines(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["total_tokens"], 12);
        assert_eq!(lines[0]["request_id"], "final");
    }
}
