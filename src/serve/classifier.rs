//! External guardrail classifiers (note 0048): a rule whose verdict comes from an HTTP endpoint the
//! operator runs. The endpoint is sent the text and nothing else about the caller, and answers
//! `allow` or `block`. Anything it says beyond the verdict is ignored, so a hostile or buggy
//! classifier cannot put text into a response or grow the metrics.

use super::config::{Classifier, Guardrail, GuardrailScope, OnFailure};
use crate::gateway::http::{ClassifierGuard, Judgement};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

const DEFAULT_TIMEOUT_MS: u64 = 2000;
const DEFAULT_MAX_BYTES: usize = 256 * 1024;
/// The most of a classifier's answer that is read.
const ANSWER_MAX: usize = 4096;
/// Calls in flight to one endpoint; over it a call fails at once, by its rule's policy.
const IN_FLIGHT: usize = 64;

struct Endpoint {
    url: String,
    token: Option<String>,
    client: reqwest::Client,
    max_bytes: usize,
    slots: Semaphore,
}

struct Rule {
    name: String,
    scope: GuardrailScope,
    tenants: Option<BTreeSet<String>>,
    on_failure: OnFailure,
    endpoint: Arc<Endpoint>,
}

/// The classifier rules of a file, in the file's order.
pub struct Classifiers {
    rules: Vec<Rule>,
}

/// Why a call produced no verdict. Only the kind is kept, never the classifier's own words.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    Oversize,
    Busy,
    Transport,
    Status,
    Answer,
}

impl Classifiers {
    /// The rules that name a classifier, or `None` when the file has none. A token is read here, once.
    pub fn build(
        rules: &[Guardrail],
        classifiers: &BTreeMap<String, Classifier>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        let mut endpoints: BTreeMap<&str, Arc<Endpoint>> = BTreeMap::new();
        let mut built = Vec::new();
        for rule in rules {
            let Some(name) = rule.classifier.as_deref() else {
                continue;
            };
            let endpoint = match endpoints.get(name) {
                Some(endpoint) => endpoint.clone(),
                None => {
                    let config = classifiers
                        .get(name)
                        .ok_or_else(|| format!("rule {} names an unknown classifier", rule.name))?;
                    let token = config
                        .token
                        .as_ref()
                        .map(|secret| {
                            super::config::read_secret(secret, env)
                                .map_err(|message| format!("classifiers.{name}.token: {message}"))
                        })
                        .transpose()?;
                    let client = reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .no_proxy()
                        .timeout(Duration::from_millis(
                            config.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
                        ))
                        .build()
                        .map_err(|_| format!("classifiers.{name}: cannot build a client"))?;
                    let endpoint = Arc::new(Endpoint {
                        url: config.url.clone(),
                        token,
                        client,
                        max_bytes: config
                            .max_bytes
                            .map_or(DEFAULT_MAX_BYTES, |bytes| bytes as usize),
                        slots: Semaphore::new(IN_FLIGHT),
                    });
                    endpoints.insert(name, endpoint.clone());
                    endpoint
                }
            };
            built.push(Rule {
                name: rule.name.clone(),
                scope: rule.scope,
                tenants: rule
                    .tenants
                    .as_ref()
                    .map(|tenants| tenants.iter().cloned().collect()),
                on_failure: rule.on_failure.unwrap_or_default(),
                endpoint,
            });
        }
        Ok((!built.is_empty()).then_some(Self { rules: built }))
    }

    fn applies(rule: &Rule, tenant: &str) -> bool {
        rule.tenants
            .as_ref()
            .is_none_or(|tenants| tenants.contains(tenant))
    }
}

/// The guardrails in force, for the audit log's `server.started` record (note 0048): each rule's name,
/// kind and scope, and for a classifier rule its host (never the path or the token) and failure
/// policy. A change of classifier, endpoint or policy is visible at the restart that makes it.
pub fn audit_summary(rules: &[Guardrail], classifiers: &BTreeMap<String, Classifier>) -> Value {
    Value::Array(
        rules
            .iter()
            .map(|rule| {
                let scope = match rule.scope {
                    GuardrailScope::Request => "request",
                    GuardrailScope::Response => "response",
                    GuardrailScope::Both => "both",
                };
                let mut entry = json!({"name": rule.name, "scope": scope});
                if let Some(name) = &rule.classifier {
                    entry["kind"] = json!("classifier");
                    entry["classifier"] = json!(name);
                    entry["host"] = classifiers
                        .get(name)
                        .and_then(|c| reqwest::Url::parse(&c.url).ok())
                        .and_then(|url| {
                            url.host_str().map(|host| match url.port() {
                                Some(port) => format!("{host}:{port}"),
                                None => host.to_owned(),
                            })
                        })
                        .map_or(Value::Null, Value::String);
                    entry["on_failure"] = json!(match rule.on_failure.unwrap_or_default() {
                        OnFailure::Block => "block",
                        OnFailure::Allow => "allow",
                    });
                } else if rule.redact_matches.is_empty() {
                    entry["kind"] = json!("block");
                } else {
                    entry["kind"] = json!("redact");
                }
                entry
            })
            .collect(),
    )
}

/// Every string value of `value` (not its keys), outside inline media, joined with newlines. `None`
/// when the text is longer than `max` bytes: a classifier that saw part of it has not seen it.
fn text_of(value: &Value, max: usize) -> Option<String> {
    fn walk(value: &Value, out: &mut String, max: usize) -> bool {
        match value {
            Value::String(text) => {
                if super::guardrails::is_media(text) {
                    return true;
                }
                if out.len() + text.len() + 1 > max {
                    return false;
                }
                out.push_str(text);
                out.push('\n');
                true
            }
            Value::Array(items) => items.iter().all(|item| walk(item, out, max)),
            Value::Object(map) => map
                .iter()
                .filter(|(key, _)| key.as_str() != super::guardrails::BULK_KEY)
                .all(|(_, item)| walk(item, out, max)),
            _ => true,
        }
    }
    let mut out = String::new();
    walk(value, &mut out, max).then_some(out)
}

impl Endpoint {
    async fn ask(&self, rule: &str, direction: &str, text: String) -> Result<bool, Failure> {
        let _slot = self.slots.try_acquire().map_err(|_| Failure::Busy)?;
        let mut request = self
            .client
            .post(&self.url)
            .json(&json!({"direction": direction, "rule": rule, "text": text}));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let mut response = request.send().await.map_err(|_| Failure::Transport)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(Failure::Status);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Transport)? {
            if body.len() + chunk.len() > ANSWER_MAX {
                return Err(Failure::Answer);
            }
            body.extend_from_slice(&chunk);
        }
        parse_verdict(&body)
    }
}

/// `{"verdict": "allow" | "block"}`; anything else is a failure, not a guess.
fn parse_verdict(body: &[u8]) -> Result<bool, Failure> {
    let value: Value = serde_json::from_slice(body).map_err(|_| Failure::Answer)?;
    match value.get("verdict").and_then(Value::as_str) {
        Some("allow") => Ok(true),
        Some("block") => Ok(false),
        _ => Err(Failure::Answer),
    }
}

#[async_trait::async_trait]
impl ClassifierGuard for Classifiers {
    fn reads_responses(&self, tenant: &str) -> bool {
        self.rules
            .iter()
            .any(|rule| rule.scope.reads_response() && Self::applies(rule, tenant))
    }

    async fn judge(&self, tenant: &str, response: bool, body: &Value) -> Judgement {
        let direction = if response { "response" } else { "request" };
        let view;
        let body = if response {
            view = super::guardrails::response_view(body);
            &view
        } else {
            body
        };
        for rule in self.rules.iter().filter(|rule| {
            Self::applies(rule, tenant)
                && if response {
                    rule.scope.reads_response()
                } else {
                    rule.scope.reads_request()
                }
        }) {
            let verdict = match text_of(body, rule.endpoint.max_bytes) {
                None => Err(Failure::Oversize),
                Some(text) => rule.endpoint.ask(&rule.name, direction, text).await,
            };
            let count = |outcome: &str| {
                crate::gateway::metrics::incr(
                    crate::gateway::metrics::GUARDRAIL_CLASSIFIER,
                    &[("rule", &rule.name), ("outcome", outcome)],
                );
            };
            match verdict {
                Ok(true) => count("allow"),
                Ok(false) => {
                    count("block");
                    return Judgement::Blocked(rule.name.clone());
                }
                Err(_) if rule.on_failure == OnFailure::Allow => count("failed_open"),
                Err(_) => {
                    count("failed_closed");
                    return Judgement::Unavailable(rule.name.clone());
                }
            }
        }
        Judgement::Pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classifiers(url: &str, rule: Value, endpoint: Value) -> Classifiers {
        let mut table = json!({"url": url});
        table
            .as_object_mut()
            .unwrap()
            .extend(endpoint.as_object().cloned().unwrap_or_default());
        let classifier: Classifier = serde_json::from_value(table).unwrap();
        let mut rule = rule;
        rule["name"] = json!("mod");
        rule["classifier"] = json!("c");
        let rule: Guardrail = serde_json::from_value(rule).unwrap();
        Classifiers::build(
            &[rule],
            &BTreeMap::from([("c".to_owned(), classifier)]),
            &|name| (name == "CLASSIFIER_TOKEN").then(|| "tok-123".to_owned()),
        )
        .unwrap()
        .unwrap()
    }

    async fn serve(body: &str, status: usize) -> (mockito::ServerGuard, mockito::Mock) {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/check")
            .with_status(status)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create_async()
            .await;
        (server, mock)
    }

    #[test]
    fn the_text_is_the_values_only_without_media_and_never_a_truncation() {
        let body = json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hello"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
            ]}],
            "audio_b64": "QUJD",
        });
        let text = text_of(&body, 1024).unwrap();
        // Every value (an object's members in key order, an array's in order); no key, no media, no
        // audio. Structure such as a role is sent too: a string the model sees is a string judged.
        assert_eq!(text, "hello\ntext\nimage_url\nuser\n");
        assert!(!text.contains("AAAA") && !text.contains("QUJD") && !text.contains("messages"));
        assert_eq!(
            text_of(&body, 10),
            None,
            "more than max is refused, not cut"
        );
    }

    #[test]
    fn a_verdict_is_exactly_allow_or_block() {
        assert_eq!(parse_verdict(br#"{"verdict":"allow"}"#), Ok(true));
        assert_eq!(
            parse_verdict(br#"{"verdict":"block","score":0.9}"#),
            Ok(false)
        );
        for bad in [
            &b""[..],
            b"not json",
            b"{}",
            br#"{"verdict":"ALLOW"}"#,
            br#"{"verdict":true}"#,
            br#"{"allow":true}"#,
            br#"["allow"]"#,
        ] {
            assert_eq!(parse_verdict(bad), Err(Failure::Answer), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn the_classifier_is_sent_the_text_the_rule_and_the_token_and_nothing_else() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/check")
            .match_header("authorization", "Bearer tok-123")
            .match_body(mockito::Matcher::Json(
                json!({"direction": "request", "rule": "mod", "text": "hello\n"}),
            ))
            .with_body(r#"{"verdict":"allow"}"#)
            .expect(1)
            .create_async()
            .await;
        let c = classifiers(
            &format!("{}/check", server.url()),
            json!({"scope": "request"}),
            json!({"token": {"env": "CLASSIFIER_TOKEN"}, "allow_insecure_http": true}),
        );
        assert_eq!(c.judge("t", false, &json!("hello")).await, Judgement::Pass);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn a_block_names_the_rule_and_an_allow_passes() {
        let (server, mock) = serve(r#"{"verdict":"block"}"#, 200).await;
        let c = classifiers(
            &format!("{}/check", server.url()),
            json!({"scope": "both"}),
            json!({"allow_insecure_http": true}),
        );
        assert_eq!(
            c.judge("t", false, &json!("x")).await,
            Judgement::Blocked("mod".into())
        );
        // An answer is read as text and judged by the same rule.
        assert_eq!(
            c.judge("t", true, &json!([{"message": {"content": "x"}}]))
                .await,
            Judgement::Blocked("mod".into())
        );
        mock.expect(2).assert_async().await;
    }

    #[tokio::test]
    async fn every_kind_of_failure_follows_the_rules_policy() {
        let (server, _) = serve("{}", 200).await;
        let url = format!("{}/check", server.url());
        let mut cases: Vec<(&str, String, Value)> =
            vec![("unknown verdict", url.clone(), json!({}))];
        let (bad_status, _) = serve(r#"{"verdict":"allow"}"#, 500).await;
        let (redirect, _) = serve(r#"{"verdict":"allow"}"#, 302).await;
        let (not_json, _) = serve("plain", 200).await;
        let (huge, _) = serve(
            &format!("{{\"verdict\":\"allow\",\"x\":\"{}\"}}", "a".repeat(5000)),
            200,
        )
        .await;
        cases.push(("500", format!("{}/check", bad_status.url()), json!({})));
        cases.push(("redirect", format!("{}/check", redirect.url()), json!({})));
        cases.push(("not json", format!("{}/check", not_json.url()), json!({})));
        cases.push((
            "oversize answer",
            format!("{}/check", huge.url()),
            json!({}),
        ));
        // Nothing listens on this port: a transport failure.
        cases.push(("refused", "http://127.0.0.1:9/check".into(), json!({})));
        for (label, url, extra) in cases {
            let mut endpoint = json!({"allow_insecure_http": true});
            endpoint
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().cloned().unwrap());
            let closed = classifiers(&url, json!({"scope": "request"}), endpoint.clone());
            assert_eq!(
                closed.judge("t", false, &json!("x")).await,
                Judgement::Unavailable("mod".into()),
                "{label}"
            );
            let open = classifiers(
                &url,
                json!({"scope": "request", "on_failure": "allow"}),
                endpoint,
            );
            assert_eq!(
                open.judge("t", false, &json!("x")).await,
                Judgement::Pass,
                "{label}"
            );
        }
    }

    #[tokio::test]
    async fn a_slow_classifier_is_cut_at_its_timeout_and_too_much_text_is_a_failure() {
        // Accepts the connection and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let c = classifiers(
            &format!("http://127.0.0.1:{port}/check"),
            json!({"scope": "request"}),
            json!({"allow_insecure_http": true, "timeout_ms": 150}),
        );
        let started = std::time::Instant::now();
        assert_eq!(
            c.judge("t", false, &json!("x")).await,
            Judgement::Unavailable("mod".into())
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        let (server, mock) = serve(r#"{"verdict":"allow"}"#, 200).await;
        let c = classifiers(
            &format!("{}/check", server.url()),
            json!({"scope": "request"}),
            json!({"allow_insecure_http": true, "max_bytes": 1024}),
        );
        assert_eq!(
            c.judge("t", false, &json!("a".repeat(2000))).await,
            Judgement::Unavailable("mod".into())
        );
        mock.expect(0).assert_async().await;
    }

    #[tokio::test]
    async fn a_call_over_the_concurrency_cap_fails_at_once() {
        let (server, mock) = serve(r#"{"verdict":"allow"}"#, 200).await;
        let c = classifiers(
            &format!("{}/check", server.url()),
            json!({"scope": "request"}),
            json!({"allow_insecure_http": true}),
        );
        let held = c.rules[0]
            .endpoint
            .slots
            .acquire_many(IN_FLIGHT as u32)
            .await
            .unwrap();
        assert_eq!(
            c.judge("t", false, &json!("x")).await,
            Judgement::Unavailable("mod".into())
        );
        drop(held);
        assert_eq!(c.judge("t", false, &json!("x")).await, Judgement::Pass);
        mock.expect(1).assert_async().await;
    }

    #[tokio::test]
    async fn a_rule_is_consulted_only_for_its_direction_and_its_tenants() {
        let (server, mock) = serve(r#"{"verdict":"block"}"#, 200).await;
        let c = classifiers(
            &format!("{}/check", server.url()),
            json!({"scope": "response", "tenants": ["platform"]}),
            json!({"allow_insecure_http": true}),
        );
        // A response rule never reads a request, and another tenant is not under it.
        assert_eq!(
            c.judge("platform", false, &json!("x")).await,
            Judgement::Pass
        );
        assert_eq!(
            c.judge("other", true, &json!([{"message": {"content": "x"}}]))
                .await,
            Judgement::Pass
        );
        mock.expect(0).assert_async().await;
        assert!(c.reads_responses("platform"));
        assert!(!c.reads_responses("other"));
        let request_only = classifiers(
            &format!("{}/check", server.url()),
            json!({"scope": "request"}),
            json!({"allow_insecure_http": true}),
        );
        assert!(!request_only.reads_responses("platform"));
    }

    #[test]
    fn the_audit_summary_names_the_host_and_never_the_path_or_token() {
        let classifier: Classifier = serde_json::from_value(json!({
            "url": "https://classifier.internal.example:8443/secret/path",
            "token": {"env": "CLASSIFIER_TOKEN"},
        }))
        .unwrap();
        let rule: Guardrail = serde_json::from_value(
            json!({"name": "mod", "classifier": "c", "scope": "both", "on_failure": "allow"}),
        )
        .unwrap();
        let summary = audit_summary(&[rule], &BTreeMap::from([("c".to_owned(), classifier)]));
        assert_eq!(
            summary,
            json!([{"name": "mod", "scope": "both", "kind": "classifier", "classifier": "c",
                "host": "classifier.internal.example:8443", "on_failure": "allow"}])
        );
        let text = summary.to_string();
        assert!(!text.contains("secret/path") && !text.contains("CLASSIFIER_TOKEN"));
    }
}
