//! AWS Signature Version 4 for S3 requests (note 0050), with `ring` and nothing else: HMAC-SHA256 over
//! a canonical request. Checked against the examples AWS publishes in its signing documentation.

use ring::{digest, hmac};

/// Characters left as they are in a SigV4 URI component: letters, digits and `-_.~`.
fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
}

/// Percent-encode `text`, leaving `/` as it is when `keep_slash` (a path) and encoding it otherwise
/// (a query component). Uppercase hex, as SigV4 requires.
pub(super) fn encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if unreserved(byte) || (keep_slash && byte == b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, bytes).as_ref())
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> hmac::Tag {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), message)
}

/// What the signer needs of a request. `headers` are every header to sign, names lowercase, and
/// must include `host`, `x-amz-content-sha256` and `x-amz-date`.
pub(super) struct Request<'a> {
    pub method: &'a str,
    /// The URI path, already percent-encoded (`/bucket/key`).
    pub path: &'a str,
    /// Query parameters, unencoded; the signer encodes and sorts them.
    pub query: &'a [(String, String)],
    pub headers: &'a [(String, String)],
    pub payload_sha256: &'a str,
}

pub(super) struct Credentials<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
}

/// The `SignedHeaders` list and the signature, for `amz_date` (`YYYYMMDDTHHMMSSZ`).
pub(super) fn sign(
    credentials: &Credentials<'_>,
    region: &str,
    amz_date: &str,
    request: &Request<'_>,
) -> (String, String) {
    let mut query: Vec<(String, String)> = request
        .query
        .iter()
        .map(|(k, v)| (encode(k, false), encode(v, false)))
        .collect();
    query.sort();
    let canonical_query = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(name, value)| {
            // Leading and trailing space trimmed, runs of spaces collapsed.
            let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
            (name.to_ascii_lowercase(), value)
        })
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(n, v)| format!("{n}:{v}\n")).collect();
    let signed_headers = headers
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        request.method,
        request.path,
        canonical_query,
        canonical_headers,
        signed_headers,
        request.payload_sha256
    );
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac_sha256(
        format!("AWS4{}", credentials.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac_sha256(k_date.as_ref(), region.as_bytes());
    let k_service = hmac_sha256(k_region.as_ref(), b"s3");
    let k_signing = hmac_sha256(k_service.as_ref(), b"aws4_request");
    let signature = hex(hmac_sha256(k_signing.as_ref(), string_to_sign.as_bytes()).as_ref());
    (signed_headers, signature)
}

/// The `Authorization` header value.
pub(super) fn authorization(
    credentials: &Credentials<'_>,
    region: &str,
    amz_date: &str,
    signed_headers: &str,
    signature: &str,
) -> String {
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{}/{region}/s3/aws4_request,SignedHeaders={signed_headers},Signature={signature}",
        credentials.access_key_id,
        &amz_date[..8]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // The examples of AWS's "Signature Calculations for the Authorization Header" (S3 documentation):
    // a fixed key pair (published, not real), the bucket `examplebucket`, 2013-05-24.
    const CREDENTIALS: Credentials<'static> = Credentials {
        access_key_id: "AKIAIOSFODNN7EXAMPLE",
        secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    };
    const DATE: &str = "20130524T000000Z";
    const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    #[test]
    fn a_get_with_a_range_matches_the_published_signature() {
        let headers = pairs(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA),
            ("x-amz-date", DATE),
        ]);
        let (signed, signature) = sign(
            &CREDENTIALS,
            "us-east-1",
            DATE,
            &Request {
                method: "GET",
                path: "/test.txt",
                query: &[],
                headers: &headers,
                payload_sha256: EMPTY_SHA,
            },
        );
        assert_eq!(signed, "host;range;x-amz-content-sha256;x-amz-date");
        assert_eq!(
            signature,
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn a_put_with_an_encoded_key_matches_the_published_signature() {
        let body = "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072";
        assert_eq!(sha256_hex(b"Welcome to Amazon S3."), body);
        let headers = pairs(&[
            ("date", "Fri, 24 May 2013 00:00:00 GMT"),
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", body),
            ("x-amz-date", DATE),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
        ]);
        let (signed, signature) = sign(
            &CREDENTIALS,
            "us-east-1",
            DATE,
            &Request {
                method: "PUT",
                path: &encode("/test$file.text", true),
                query: &[],
                headers: &headers,
                payload_sha256: body,
            },
        );
        assert_eq!(
            signed,
            "date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class"
        );
        assert_eq!(
            signature,
            "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        );
    }

    #[test]
    fn a_listing_with_a_query_matches_the_published_signature() {
        let headers = pairs(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY_SHA),
            ("x-amz-date", DATE),
        ]);
        let query = pairs(&[("prefix", "J"), ("max-keys", "2")]);
        let (_, signature) = sign(
            &CREDENTIALS,
            "us-east-1",
            DATE,
            &Request {
                method: "GET",
                path: "/",
                query: &query,
                headers: &headers,
                payload_sha256: EMPTY_SHA,
            },
        );
        assert_eq!(
            signature,
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
    }

    #[test]
    fn encoding_keeps_the_unreserved_set_and_slashes_only_in_a_path() {
        assert_eq!(encode("a b/c~d_e-f.g", true), "a%20b/c~d_e-f.g");
        assert_eq!(encode("a/b=c&d", false), "a%2Fb%3Dc%26d");
        assert_eq!(encode("é", true), "%C3%A9");
    }
}
