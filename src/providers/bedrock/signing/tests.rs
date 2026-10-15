use super::*;
use serde_json::Value;

#[test]
fn aws_published_signing_vectors() {
    for case in ["get-vanilla", "post-vanilla", "post-x-www-form-urlencoded"] {
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bedrock/signing");
        let context: Value = serde_json::from_str(
            &std::fs::read_to_string(directory.join(format!("{case}-context.json"))).unwrap(),
        )
        .unwrap();
        let raw = std::fs::read_to_string(directory.join(format!("{case}-request.txt"))).unwrap();
        let raw = raw.replace("\r\n", "\n");
        let (head, body) = raw.split_once("\n\n").unwrap_or((&raw, ""));
        let mut lines = head.lines();
        let mut first = lines.next().unwrap().split_whitespace();
        let method = first.next().unwrap().parse().unwrap();
        let path = first.next().unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        for line in lines {
            let (name, value) = line.split_once(':').unwrap();
            headers.append(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_str(value.trim()).unwrap(),
            );
        }
        let host = headers["host"].to_str().unwrap();
        let mut request =
            reqwest::Request::new(method, format!("https://{host}{path}").parse().unwrap());
        *request.headers_mut() = headers;
        *request.body_mut() = Some(body.to_owned().into());
        let credentials = Credentials::new(
            context["credentials"]["access_key_id"].as_str().unwrap(),
            context["credentials"]["secret_access_key"]
                .as_str()
                .unwrap(),
            None,
            None,
            "fixture",
        );
        let time =
            chrono::DateTime::parse_from_rfc3339(context["timestamp"].as_str().unwrap()).unwrap();
        let mut settings = aws_sigv4::http_request::SigningSettings::default();
        settings.payload_checksum_kind = if context["sign_body"] == true {
            aws_sigv4::http_request::PayloadChecksumKind::XAmzSha256
        } else {
            aws_sigv4::http_request::PayloadChecksumKind::NoHeader
        };
        sign_request(
            &mut request,
            credentials,
            context["region"].as_str().unwrap(),
            context["service"].as_str().unwrap(),
            time.into(),
            settings,
        )
        .unwrap();
        let expected =
            std::fs::read_to_string(directory.join(format!("{case}-header-signature.txt")))
                .unwrap();
        assert!(
            request.headers()["authorization"]
                .to_str()
                .unwrap()
                .ends_with(&format!("Signature={}", expected.trim())),
            "{case}"
        );
        assert_eq!(request.body().unwrap().as_bytes().unwrap(), body.as_bytes());
    }
}
