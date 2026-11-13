//! Client-certificate identity: read the names of a verified peer certificate,
//! and bind the one that matches a configured workload to the request.
//!
//! The TLS layer verifies the certificate against the configured authorities
//! during the handshake; nothing here trusts an unverified one. What this module
//! decides is how a verified certificate becomes an identity, and how that
//! identity reaches the key store without a caller being able to forge it.
//!
//! **The binding.** The key store reads the matched name from an internal header,
//! [`PEER_SAN_HEADER`]. [`bind_peer_identity`] removes that header from every
//! inbound request and then sets it only from the handshake. It runs outside
//! everything else, so no handler and no log line sees a caller-supplied value.

use crate::gateway::auth::{KeyStore, PEER_SAN_HEADER};
use crate::serve::config::canonical_san;
use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use rustls::pki_types::CertificateDer;
use std::sync::Arc;

/// What the TLS layer learned about a connection's verified peer certificate:
/// its names in canonical form (`dns:<lower-case name>`, `uri:<uri>`), and when
/// it stops being valid. Attached to each request of the connection by the TLS
/// acceptor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerSans {
    pub names: Arc<Vec<String>>,
    /// The leaf certificate's `notAfter`, in seconds since the Unix epoch. The
    /// certificate was valid when the handshake verified it, but a connection
    /// can outlive it (an HTTP/2 connection is reused for as long as both sides
    /// keep it), so each request is checked against this.
    pub not_after: Option<i64>,
}

impl PeerSans {
    /// Read the names and expiry of the leaf of a verified chain.
    pub fn from_chain(chain: &[CertificateDer<'_>]) -> Self {
        Self {
            names: Arc::new(peer_sans(chain)),
            not_after: peer_not_after(chain),
        }
    }
}

/// The leaf certificate's `notAfter` as Unix seconds, if it parses.
pub fn peer_not_after(chain: &[CertificateDer<'_>]) -> Option<i64> {
    use x509_parser::prelude::{FromDer, X509Certificate};
    let (_, certificate) = X509Certificate::from_der(chain.first()?.as_ref()).ok()?;
    Some(certificate.validity().not_after.timestamp())
}

/// The subject alternative names of the leaf certificate in `chain` (the first
/// certificate a peer presents). A certificate that does not parse, or has none,
/// yields no names and so no identity.
pub fn peer_sans(chain: &[CertificateDer<'_>]) -> Vec<String> {
    use x509_parser::extensions::GeneralName;
    use x509_parser::prelude::{FromDer, X509Certificate};
    let Some(leaf) = chain.first() else {
        return Vec::new();
    };
    let Ok((_, certificate)) = X509Certificate::from_der(leaf.as_ref()) else {
        return Vec::new();
    };
    let Ok(Some(extension)) = certificate.subject_alternative_name() else {
        return Vec::new();
    };
    extension
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::DNSName(name) => Some(canonical_san(&format!("dns:{name}"))),
            GeneralName::URI(uri) => Some(format!("uri:{uri}")),
            _ => None,
        })
        .collect()
}

/// The configured name a peer's certificate matches, if it matches exactly one.
/// A certificate that carries two names configured for two workloads is
/// ambiguous, and which of them it would be depends on the order the issuer
/// wrote them in, so it is no identity at all. A certificate past its `notAfter`
/// is none either, even on a connection that was established while it was valid.
fn matched_name(peer: &PeerSans, is_workload: impl Fn(&str) -> bool, now: i64) -> Option<String> {
    if peer.not_after.is_some_and(|not_after| now >= not_after) {
        return None;
    }
    let mut matches = peer
        .names
        .iter()
        .filter(|name| is_workload(name))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter();
    match (matches.next(), matches.next()) {
        (Some(name), None) => Some(name.clone()),
        _ => None,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Remove [`PEER_SAN_HEADER`] from the request. Every listener runs this, so a
/// caller can never supply the header whatever the listener does with it.
pub async fn strip_peer_header(mut request: Request, next: Next) -> Response {
    request.headers_mut().remove(PEER_SAN_HEADER);
    next.run(request).await
}

/// Strip [`PEER_SAN_HEADER`] from the request, then set it to the name of the
/// connection's verified certificate that `keys` lists as a workload (see
/// [`matched_name`]). Without a certificate, or with no usable match, the
/// header is simply absent. `keys` is asked on every request, so a reload that
/// adds or removes a workload takes effect on the next one, on connections
/// already open.
pub async fn bind_peer_identity(keys: Arc<KeyStore>, mut request: Request, next: Next) -> Response {
    request.headers_mut().remove(PEER_SAN_HEADER);
    let matched = request
        .extensions()
        .get::<PeerSans>()
        .and_then(|peer| matched_name(peer, |name| keys.has_peer(name), unix_now()));
    if let Some(name) = matched {
        // A name with characters a header cannot carry is not bound; the caller
        // is then unidentified, which fails closed.
        if let Ok(value) = HeaderValue::from_str(&name) {
            request.headers_mut().insert(PEER_SAN_HEADER, value);
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn certificate(sans: Vec<rcgen::SanType>) -> CertificateDer<'static> {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::default();
        params.subject_alt_names = sans;
        params.self_signed(&key).unwrap().der().clone()
    }

    #[test]
    fn dns_and_uri_names_are_read_and_canonicalised() {
        let der = certificate(vec![
            rcgen::SanType::DnsName("Checkout.Payments.SVC".try_into().unwrap()),
            rcgen::SanType::URI("spiffe://corp/payments/checkout".try_into().unwrap()),
            rcgen::SanType::IpAddress("10.0.0.1".parse().unwrap()),
        ]);
        let names = peer_sans(&[der]);
        assert_eq!(
            names,
            vec![
                "dns:checkout.payments.svc".to_string(),
                "uri:spiffe://corp/payments/checkout".to_string()
            ]
        );
    }

    fn peer(names: &[&str], not_after: Option<i64>) -> PeerSans {
        PeerSans {
            names: Arc::new(names.iter().map(|n| n.to_string()).collect()),
            not_after,
        }
    }

    fn known(names: &[&str]) -> impl Fn(&str) -> bool {
        let names: HashSet<String> = names.iter().map(|n| n.to_string()).collect();
        move |name| names.contains(name)
    }

    #[test]
    fn a_certificate_naming_two_workloads_is_no_identity() {
        let known = known(&["dns:a.corp", "dns:b.corp"]);
        let now = 1_000;
        assert_eq!(
            matched_name(&peer(&["dns:a.corp"], None), &known, now).as_deref(),
            Some("dns:a.corp")
        );
        // The same name twice is still one workload.
        assert_eq!(
            matched_name(&peer(&["dns:a.corp", "dns:a.corp"], None), &known, now).as_deref(),
            Some("dns:a.corp")
        );
        // Which of two it would be depends on the order the issuer wrote them.
        for order in [["dns:a.corp", "dns:b.corp"], ["dns:b.corp", "dns:a.corp"]] {
            assert_eq!(matched_name(&peer(&order, None), &known, now), None);
        }
        // A name nothing configures does not count, so one match among them does.
        assert_eq!(
            matched_name(&peer(&["dns:other.corp", "dns:b.corp"], None), &known, now).as_deref(),
            Some("dns:b.corp")
        );
        assert_eq!(matched_name(&peer(&[], None), &known, now), None);
    }

    #[test]
    fn a_connection_outliving_its_certificate_loses_the_identity() {
        let known = known(&["dns:a.corp"]);
        let peer = peer(&["dns:a.corp"], Some(2_000));
        assert!(matched_name(&peer, &known, 1_999).is_some());
        assert_eq!(
            matched_name(&peer, &known, 2_000),
            None,
            "notAfter is exclusive"
        );
        assert_eq!(matched_name(&peer, &known, 9_999), None);
    }

    #[test]
    fn the_expiry_is_read_from_the_leaf() {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::default();
        params.not_after = rcgen::date_time_ymd(2031, 1, 2);
        let der = params.self_signed(&key).unwrap().der().clone();
        let expected = 1_925_078_400; // 2031-01-02T00:00:00Z
        assert_eq!(peer_not_after(&[der]), Some(expected));
        assert_eq!(peer_not_after(&[]), None);
    }

    #[test]
    fn nothing_parses_to_no_names() {
        assert!(peer_sans(&[]).is_empty());
        assert!(peer_sans(&[CertificateDer::from(vec![1, 2, 3])]).is_empty());
        assert!(peer_sans(&[certificate(vec![])]).is_empty());
    }
}
