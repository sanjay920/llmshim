use crate::error::{Result, ShimError};
use crate::provider::{Provider, ProviderRequest};
use bytes::Bytes;
use futures::Stream;
use reqwest::Client;
use std::pin::Pin;
use std::time::Duration;

pub struct ShimClient {
    http: Client,
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

    pub async fn send(&self, req: &ProviderRequest) -> Result<reqwest::Response> {
        let mut builder = self.http.post(&req.url);
        for (k, v) in &req.headers {
            builder = builder.header(k, v);
        }
        builder = builder.json(&req.body);

        let resp = builder.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ShimError::ProviderError {
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp)
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
