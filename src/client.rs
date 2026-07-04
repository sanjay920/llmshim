use crate::error::{Result, ShimError};
use crate::provider::{Provider, ProviderRequest};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::Stream;
use reqwest::header::HeaderMap;
use reqwest::Client;
use std::pin::Pin;
use std::time::Duration;

/// Retry bounds, resolved once from the environment (with defaults) at
/// construction time. Internal/additive to `ShimClient` — not part of the
/// public API surface.
#[derive(Clone, Copy, Debug)]
struct RetryConfig {
    /// Number of *retries* after the initial attempt.
    max_retries: u32,
    /// Base for exponential backoff (attempt 0 → `base`, 1 → 2·base, …).
    base: Duration,
    /// Hard cap on any single wait, whether server-dictated or computed.
    cap: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(60),
        }
    }
}

impl RetryConfig {
    /// Read `LLMSHIM_MAX_RETRIES` and `LLMSHIM_MAX_BACKOFF_SECS`, falling back
    /// to defaults on absence or unparseable values (never panics).
    fn from_env() -> Self {
        let d = Self::default();
        let max_retries = env_parse("LLMSHIM_MAX_RETRIES").unwrap_or(d.max_retries);
        let cap_secs = env_parse::<u64>("LLMSHIM_MAX_BACKOFF_SECS").unwrap_or(d.cap.as_secs());
        Self {
            max_retries,
            base: d.base,
            cap: Duration::from_secs(cap_secs),
        }
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.trim().parse().ok()
}

pub struct ShimClient {
    http: Client,
    retry: RetryConfig,
}

impl Default for ShimClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ShimClient {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .pool_idle_timeout(Duration::from_secs(90))
                .pool_max_idle_per_host(4)
                .tcp_keepalive(Duration::from_secs(30))
                .tcp_nodelay(true)
                .build()
                .expect("failed to build HTTP client"),
            retry: RetryConfig::from_env(),
        }
    }

    /// Pre-establish TCP+TLS connections to provider endpoints.
    /// Call this after creating the Router to warm the connection pool.
    pub async fn warmup(&self, urls: &[&str]) {
        let futs: Vec<_> = urls
            .iter()
            .map(|url| {
                let client = self.http.clone();
                let url = url.to_string();
                tokio::spawn(async move {
                    // HEAD request — cheapest way to establish a connection
                    let _ = client
                        .head(&url)
                        .timeout(Duration::from_secs(5))
                        .send()
                        .await;
                })
            })
            .collect();
        for f in futs {
            let _ = f.await;
        }
    }

    const RETRYABLE_STATUSES: &'static [u16] = &[429, 500, 502, 503, 504, 529];

    pub async fn send(&self, req: &ProviderRequest) -> Result<reqwest::Response> {
        let max_retries = self.retry.max_retries;

        for attempt in 0..=max_retries {
            let mut builder = self.http.post(&req.url);
            for (k, v) in &req.headers {
                builder = builder.header(k, v);
            }
            builder = builder.json(&req.body);

            match builder.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(resp);
                    }
                    let status_code = status.as_u16();
                    if Self::RETRYABLE_STATUSES.contains(&status_code) && attempt < max_retries {
                        // Prefer server-provided timing (Retry-After / provider
                        // reset hints); otherwise fall back to jittered backoff.
                        let wait =
                            retry_after_wait(resp.headers(), self.retry.cap).unwrap_or_else(|| {
                                backoff_with_jitter(attempt, self.retry.base, self.retry.cap)
                            });
                        // Consume body before retrying (can't reuse response)
                        let _ = resp.text().await;
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    let body = resp.text().await.unwrap_or_default();
                    return Err(ShimError::ProviderError {
                        status: status_code,
                        body,
                    });
                }
                // Transport errors carry no headers: always jittered backoff.
                Err(e) if Self::is_retryable_transport(&e) && attempt < max_retries => {
                    tokio::time::sleep(backoff_with_jitter(
                        attempt,
                        self.retry.base,
                        self.retry.cap,
                    ))
                    .await;
                    continue;
                }
                Err(e) => return Err(ShimError::Http(e)),
            }
        }
        unreachable!()
    }

    fn is_retryable_transport(err: &reqwest::Error) -> bool {
        // Retry all transport-level failures: connect, timeout, request build,
        // body read errors, connection reset, incomplete messages, etc.
        err.is_connect() || err.is_timeout() || err.is_request() || err.is_body()
    }

    pub async fn completion(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let provider_req = provider.transform_request(model, request)?;
        let resp = self.send(&provider_req).await?;
        let body: serde_json::Value = resp.json().await?;
        provider.transform_response(model, body)
    }

    pub async fn stream(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &serde_json::Value,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        let mut req_value = request.clone();
        req_value["stream"] = serde_json::Value::Bool(true);

        let provider_req = provider.transform_request(model, &req_value)?;
        let resp = self.send(&provider_req).await?;
        let provider_name = provider.name().to_string();
        let model_str = model.to_string();

        let byte_stream = resp.bytes_stream();

        Ok(Box::pin(SseStream {
            inner: Box::pin(byte_stream),
            buffer: String::new(),
            provider_name,
            model: model_str,
        }))
    }
}

// ---------------------------------------------------------------------------
// Retry timing (reactive layer)
//
// Pure, network-free helpers so timing logic is unit-testable with no HTTP.
// ---------------------------------------------------------------------------

/// Compute how long to wait before retrying a retryable *response*, using
/// server-provided timing when available. Returns `None` when the server gives
/// no usable hint (caller then falls back to jittered backoff).
///
/// Priority: `Retry-After` header, then provider reset hints. The result is
/// clamped to `cap` and nudged with a little jitter so a fleet of clients
/// handed the same reset time don't retry in lockstep (thundering herd).
fn retry_after_wait(headers: &HeaderMap, cap: Duration) -> Option<Duration> {
    let base = parse_retry_after(headers).or_else(|| parse_provider_reset(headers))?;
    let capped = base.min(cap);
    Some(capped + small_jitter())
}

/// Parse the `Retry-After` header (RFC 7231): either an integer number of
/// seconds, or an HTTP-date. Returns `None` when absent or unparseable.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    parse_retry_after_at(headers, Utc::now())
}

fn parse_retry_after_at(headers: &HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    let raw = header_str(headers, "retry-after")?.trim();
    // Form 1: delay in whole seconds.
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // Form 2: HTTP-date (RFC 7231, e.g. "Wed, 21 Oct 2015 07:28:00 GMT").
    let when = DateTime::parse_from_rfc2822(raw).ok()?.with_timezone(&Utc);
    duration_until(when, now)
}

/// Best-effort provider-specific reset hints, used only when `Retry-After` is
/// absent. Takes the most conservative (largest) positive hint present.
///
/// - OpenAI: `x-ratelimit-reset-tokens` / `x-ratelimit-reset-requests`, which
///   are Go-duration-ish strings like `"1s"`, `"6m0s"`, `"100ms"`.
/// - Anthropic: any `anthropic-ratelimit-*-reset` header (RFC3339 timestamp).
fn parse_provider_reset(headers: &HeaderMap) -> Option<Duration> {
    parse_provider_reset_at(headers, Utc::now())
}

fn parse_provider_reset_at(headers: &HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    let mut best: Option<Duration> = None;
    let mut consider = |d: Option<Duration>| {
        if let Some(d) = d {
            best = Some(best.map_or(d, |b| b.max(d)));
        }
    };

    // OpenAI Go-duration reset hints.
    for name in ["x-ratelimit-reset-tokens", "x-ratelimit-reset-requests"] {
        if let Some(v) = header_str(headers, name) {
            consider(parse_go_duration(v));
        }
    }

    // Anthropic RFC3339 reset timestamps (header names vary by resource, e.g.
    // anthropic-ratelimit-requests-reset, -tokens-reset, -input-tokens-reset).
    for (name, value) in headers.iter() {
        let name = name.as_str();
        if name.starts_with("anthropic-ratelimit-") && name.ends_with("-reset") {
            if let Ok(v) = value.to_str() {
                if let Ok(when) = DateTime::parse_from_rfc3339(v.trim()) {
                    consider(duration_until(when.with_timezone(&Utc), now));
                }
            }
        }
    }

    best
}

/// Parse a Go-style duration string (`"1s"`, `"100ms"`, `"6m0s"`, `"1h2m3s"`).
/// Supports `h`, `m`, `s`, `ms`, `us`/`µs`, `ns` units. Returns `None` on any
/// unrecognized input.
fn parse_go_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut total = Duration::ZERO;
    let mut saw_unit = false;

    while i < bytes.len() {
        // Numeric part (integer or decimal).
        let num_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        if i == num_start {
            return None; // expected a number
        }
        let value: f64 = s[num_start..i].parse().ok()?;

        // Unit part.
        let unit_start = i;
        while i < bytes.len() && !(bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        let unit = &s[unit_start..i];
        let secs = match unit {
            "h" => value * 3600.0,
            "m" => value * 60.0,
            "s" => value,
            "ms" => value / 1_000.0,
            "us" | "µs" | "μs" => value / 1_000_000.0,
            "ns" => value / 1_000_000_000.0,
            _ => return None,
        };
        total += Duration::from_secs_f64(secs);
        saw_unit = true;
    }

    saw_unit.then_some(total)
}

/// Positive duration from `now` until `when`; `None`/zero if `when` is in the past.
fn duration_until(when: DateTime<Utc>, now: DateTime<Utc>) -> Option<Duration> {
    (when - now).to_std().ok()
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// Exponential backoff ceiling for `attempt`: `min(cap, base * 2^attempt)`.
fn backoff_bound(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let mult = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let ms = (base.as_millis() as u64).saturating_mul(mult);
    Duration::from_millis(ms).min(cap)
}

/// Full jitter: a uniform random point in `[0, bound]` given a random source.
/// Pure and deterministic for a fixed `rand` — the randomness is injected.
fn full_jitter(bound: Duration, rand: u64) -> Duration {
    let ms = bound.as_millis() as u64;
    if ms == 0 {
        return Duration::ZERO;
    }
    Duration::from_millis(rand % (ms + 1))
}

/// Full-jitter exponential backoff: uniform in `[0, min(cap, base·2^attempt)]`.
fn backoff_with_jitter(attempt: u32, base: Duration, cap: Duration) -> Duration {
    full_jitter(backoff_bound(attempt, base, cap), rand_u64())
}

/// A little jitter (0–250ms) added to server-dictated waits so a fleet handed
/// the same reset time spreads its retries instead of firing simultaneously.
fn small_jitter() -> Duration {
    Duration::from_millis(rand_u64() % 251)
}

/// Cheap non-cryptographic randomness derived from the clock — good enough for
/// retry jitter and keeps the dependency footprint at zero. SplitMix64 finalizer
/// over the current nanoseconds.
fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

struct SseStream {
    inner: Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>,
    buffer: String,
    provider_name: String,
    model: String,
}

impl Stream for SseStream {
    type Item = Result<String>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;

        loop {
            // Try to extract a complete SSE event from the buffer
            if let Some(chunk) = extract_sse_data(&mut self.buffer) {
                let transformed = match self.provider_name.as_str() {
                    "anthropic" => {
                        let p = crate::providers::anthropic::Anthropic {
                            api_key: String::new(),
                            base_url: String::new(),
                        };
                        p.transform_stream_chunk(&self.model, &chunk)
                    }
                    "gemini" => {
                        let p = crate::providers::gemini::Gemini {
                            api_key: String::new(),
                            base_url: String::new(),
                        };
                        p.transform_stream_chunk(&self.model, &chunk)
                    }
                    "xai" => {
                        let p = crate::providers::xai::Xai {
                            api_key: String::new(),
                            base_url: String::new(),
                        };
                        p.transform_stream_chunk(&self.model, &chunk)
                    }
                    _ => {
                        let p = crate::providers::openai::OpenAi {
                            api_key: String::new(),
                            base_url: String::new(),
                        };
                        p.transform_stream_chunk(&self.model, &chunk)
                    }
                };

                match transformed {
                    Ok(Some(data)) => return Poll::Ready(Some(Ok(data))),
                    Ok(None) => continue, // skip this chunk, try next
                    Err(e) => return Poll::Ready(Some(Err(e))),
                }
            }

            // Need more data from the HTTP stream
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    let text = String::from_utf8_lossy(&bytes);
                    self.buffer.push_str(&text);
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(ShimError::Http(e))));
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Extract the next complete SSE "data:" payload from the buffer.
fn extract_sse_data(buffer: &mut String) -> Option<String> {
    loop {
        let newline_pos = buffer.find('\n')?;
        let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
        buffer.drain(..=newline_pos);

        if let Some(data) = line.strip_prefix("data: ") {
            if data == "[DONE]" {
                return None;
            }
            return Some(data.to_string());
        }
        // Skip non-data lines (event:, id:, retry:, empty lines)
        if buffer.is_empty() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    // --- Retry-After parsing ------------------------------------------------

    #[test]
    fn retry_after_integer_seconds() {
        let h = headers(&[("retry-after", "5")]);
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(parse_retry_after_at(&h, now), Some(Duration::from_secs(5)));
    }

    #[test]
    fn retry_after_zero_seconds() {
        let h = headers(&[("retry-after", "0")]);
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(parse_retry_after_at(&h, now), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_http_date_future() {
        // now = 07:28:00, header = 07:28:30 GMT → 30s.
        let now = Utc.with_ymd_and_hms(2015, 10, 21, 7, 28, 0).unwrap();
        let h = headers(&[("retry-after", "Wed, 21 Oct 2015 07:28:30 GMT")]);
        assert_eq!(parse_retry_after_at(&h, now), Some(Duration::from_secs(30)));
    }

    #[test]
    fn retry_after_http_date_in_past_is_none() {
        // Header date is before `now` → no positive wait.
        let now = Utc.with_ymd_and_hms(2015, 10, 21, 7, 29, 0).unwrap();
        let h = headers(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")]);
        assert_eq!(parse_retry_after_at(&h, now), None);
    }

    #[test]
    fn retry_after_absent_or_garbage_is_none() {
        let now = Utc::now();
        assert_eq!(parse_retry_after_at(&HeaderMap::new(), now), None);
        let h = headers(&[("retry-after", "soon-ish")]);
        assert_eq!(parse_retry_after_at(&h, now), None);
    }

    // --- Provider reset hints -----------------------------------------------

    #[test]
    fn openai_reset_go_duration_takes_max() {
        let now = Utc::now();
        let h = headers(&[
            ("x-ratelimit-reset-requests", "1s"),
            ("x-ratelimit-reset-tokens", "6m0s"),
        ]);
        // max(1s, 6m) = 6m = 360s
        assert_eq!(
            parse_provider_reset_at(&h, now),
            Some(Duration::from_secs(360))
        );
    }

    #[test]
    fn openai_reset_millis() {
        let now = Utc::now();
        let h = headers(&[("x-ratelimit-reset-tokens", "100ms")]);
        assert_eq!(
            parse_provider_reset_at(&h, now),
            Some(Duration::from_millis(100))
        );
    }

    #[test]
    fn anthropic_reset_rfc3339() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let h = headers(&[("anthropic-ratelimit-requests-reset", "2026-01-01T00:00:10Z")]);
        assert_eq!(
            parse_provider_reset_at(&h, now),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn anthropic_reset_takes_max_across_resources() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let h = headers(&[
            ("anthropic-ratelimit-requests-reset", "2026-01-01T00:00:05Z"),
            ("anthropic-ratelimit-tokens-reset", "2026-01-01T00:00:20Z"),
        ]);
        assert_eq!(
            parse_provider_reset_at(&h, now),
            Some(Duration::from_secs(20))
        );
    }

    #[test]
    fn provider_reset_unknown_format_is_none() {
        let now = Utc::now();
        let h = headers(&[("x-ratelimit-reset-tokens", "not-a-duration")]);
        assert_eq!(parse_provider_reset_at(&h, now), None);
        assert_eq!(parse_provider_reset_at(&HeaderMap::new(), now), None);
    }

    // --- Go duration parser -------------------------------------------------

    #[test]
    fn go_duration_variants() {
        assert_eq!(parse_go_duration("1s"), Some(Duration::from_secs(1)));
        assert_eq!(parse_go_duration("6m0s"), Some(Duration::from_secs(360)));
        assert_eq!(parse_go_duration("100ms"), Some(Duration::from_millis(100)));
        assert_eq!(parse_go_duration("1h2m3s"), Some(Duration::from_secs(3723)));
        assert_eq!(parse_go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("abc"), None);
        assert_eq!(parse_go_duration("10"), None); // no unit
        assert_eq!(parse_go_duration("5x"), None); // unknown unit
    }

    // --- Backoff / jitter ---------------------------------------------------

    #[test]
    fn backoff_bound_doubles_and_caps() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(60);
        assert_eq!(backoff_bound(0, base, cap), Duration::from_secs(1));
        assert_eq!(backoff_bound(1, base, cap), Duration::from_secs(2));
        assert_eq!(backoff_bound(2, base, cap), Duration::from_secs(4));
        // 2^10 = 1024s clamped to cap.
        assert_eq!(backoff_bound(10, base, cap), cap);
        // Absurd attempt must not overflow/panic.
        assert_eq!(backoff_bound(200, base, cap), cap);
    }

    #[test]
    fn full_jitter_stays_within_bound() {
        let bound = Duration::from_millis(1000);
        for rand in [0u64, 1, 500, 1000, 1001, u64::MAX] {
            let j = full_jitter(bound, rand);
            assert!(j <= bound, "jitter {j:?} exceeded bound {bound:?}");
        }
        assert_eq!(full_jitter(bound, 0), Duration::ZERO);
        assert_eq!(full_jitter(Duration::ZERO, u64::MAX), Duration::ZERO);
    }

    #[test]
    fn backoff_with_jitter_within_bound_over_many_draws() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(60);
        for attempt in 0..4 {
            let bound = backoff_bound(attempt, base, cap);
            for _ in 0..200 {
                let d = backoff_with_jitter(attempt, base, cap);
                assert!(d <= bound, "{d:?} exceeded bound {bound:?}");
            }
        }
    }

    // --- retry_after_wait (combines + caps + jitters) -----------------------

    #[test]
    fn retry_after_wait_caps_bogus_header() {
        // 999999s Retry-After must be clamped to cap (+ small jitter < 251ms).
        let h = headers(&[("retry-after", "999999")]);
        let cap = Duration::from_secs(60);
        let w = retry_after_wait(&h, cap).unwrap();
        assert!(w >= cap && w < cap + Duration::from_millis(251));
    }

    #[test]
    fn retry_after_wait_none_without_hints() {
        assert_eq!(
            retry_after_wait(&HeaderMap::new(), Duration::from_secs(60)),
            None
        );
    }

    // --- Env config ---------------------------------------------------------

    #[test]
    fn retry_config_defaults() {
        let d = RetryConfig::default();
        assert_eq!(d.max_retries, 3);
        assert_eq!(d.base, Duration::from_secs(1));
        assert_eq!(d.cap, Duration::from_secs(60));
    }

    #[test]
    fn env_parse_valid_and_invalid() {
        // Unique keys so we never race the real LLMSHIM_* vars other tests use.
        std::env::set_var("LLMSHIM_TEST_ENV_PARSE_OK", "7");
        std::env::set_var("LLMSHIM_TEST_ENV_PARSE_BAD", "not-a-number");
        assert_eq!(env_parse::<u32>("LLMSHIM_TEST_ENV_PARSE_OK"), Some(7));
        assert_eq!(env_parse::<u32>("LLMSHIM_TEST_ENV_PARSE_BAD"), None);
        assert_eq!(env_parse::<u32>("LLMSHIM_TEST_ENV_PARSE_MISSING"), None);
        std::env::remove_var("LLMSHIM_TEST_ENV_PARSE_OK");
        std::env::remove_var("LLMSHIM_TEST_ENV_PARSE_BAD");
    }

    // --- Integration (mockito, local only — no provider API calls) ----------

    #[tokio::test]
    async fn honors_retry_after_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        // First response: 429 with Retry-After: 1 (served once).
        let m429 = server
            .mock("POST", "/v1/chat")
            .with_status(429)
            .with_header("retry-after", "1")
            .with_body("rate limited")
            .expect(1)
            .create_async()
            .await;
        // Then: success.
        let m200 = server
            .mock("POST", "/v1/chat")
            .with_status(200)
            .with_body("ok")
            .expect(1)
            .create_async()
            .await;

        let client = ShimClient::new();
        let req = ProviderRequest {
            url: format!("{}/v1/chat", server.url()),
            headers: vec![],
            body: serde_json::json!({"hello": "world"}),
        };

        let start = std::time::Instant::now();
        let resp = client.send(&req).await.expect("should succeed after retry");
        let elapsed = start.elapsed();

        assert!(resp.status().is_success());
        // Proves we waited on Retry-After: 1 (allow small scheduling slack).
        assert!(
            elapsed >= Duration::from_millis(900),
            "expected ~1s Retry-After wait, got {elapsed:?}"
        );
        assert_eq!(resp.text().await.unwrap(), "ok");
        m429.assert_async().await;
        m200.assert_async().await;
    }

    #[tokio::test]
    async fn falls_back_to_jittered_backoff_without_header() {
        let mut server = mockito::Server::new_async().await;
        // 500 with NO Retry-After → client uses jittered backoff (bound 1s at attempt 0).
        let m500 = server
            .mock("POST", "/v1/chat")
            .with_status(500)
            .with_body("boom")
            .expect(1)
            .create_async()
            .await;
        let m200 = server
            .mock("POST", "/v1/chat")
            .with_status(200)
            .with_body("ok")
            .expect(1)
            .create_async()
            .await;

        let client = ShimClient::new();
        let req = ProviderRequest {
            url: format!("{}/v1/chat", server.url()),
            headers: vec![],
            body: serde_json::json!({}),
        };

        let start = std::time::Instant::now();
        let resp = client.send(&req).await.expect("should succeed after retry");
        let elapsed = start.elapsed();

        assert!(resp.status().is_success());
        // Full-jitter backoff at attempt 0 is bounded by base (1s); give slack.
        assert!(
            elapsed < Duration::from_secs(3),
            "backoff should be sub-cap jitter, got {elapsed:?}"
        );
        m500.assert_async().await;
        m200.assert_async().await;
    }
}
