use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use std::io::Write;
use std::sync::{Arc, Mutex};
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
    pub total_tokens: u64,
    pub status: String,
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
            total_tokens: 0,
            status: "error".to_string(),
            error: Some(error.to_string()),
            request_id: None,
        }
    }
}

/// Logger that writes JSONL to a writer (file or stdout).
pub struct Logger {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Logger {
    /// Create a logger that writes to a file.
    pub fn to_file(path: &str) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            writer: Arc::new(Mutex::new(Box::new(file))),
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
