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

use crate::gateway::auth::PEER_SAN_HEADER;
use crate::serve::config::canonical_san;
use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use rustls::pki_types::CertificateDer;
use std::collections::HashSet;
use std::sync::Arc;

/// The names in a verified peer certificate, in canonical form: `dns:<lower-case
/// name>` and `uri:<uri>`. Attached to each request of a connection by the TLS
/// acceptor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerSans(pub Arc<Vec<String>>);

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

/// Strip [`PEER_SAN_HEADER`] from the request, then set it to the first name of
/// the connection's verified certificate that `known` contains. Without a
/// certificate, or with no matching name, the header is simply absent.
pub async fn bind_peer_identity(
    known: Arc<HashSet<String>>,
    mut request: Request,
    next: Next,
) -> Response {
    request.headers_mut().remove(PEER_SAN_HEADER);
    let matched = request
        .extensions()
        .get::<PeerSans>()
        .and_then(|peer| peer.0.iter().find(|name| known.contains(*name)).cloned());
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

    #[test]
    fn nothing_parses_to_no_names() {
        assert!(peer_sans(&[]).is_empty());
        assert!(peer_sans(&[CertificateDer::from(vec![1, 2, 3])]).is_empty());
        assert!(peer_sans(&[certificate(vec![])]).is_empty());
    }
}
