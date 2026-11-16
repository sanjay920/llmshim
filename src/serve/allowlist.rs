//! Per-key network allowlists (0017).
//!
//! A `[[keys]]` entry may list `allowed_cidrs`. A request authenticated as such a
//! key is refused (403) unless the caller's address is inside one of them, before
//! it reaches an idempotency replay, a receipt, the queue, a rate bucket or a
//! budget. The check runs for every credential kind, so a stolen bearer key is
//! bound to the network as well as a certificate identity.
//!
//! The caller's address is the TCP peer, unless the peer is a trusted proxy
//! (`server.trusted_proxies`): then it is the right-most `X-Forwarded-For` entry
//! that is not itself a trusted proxy. With no trusted proxies the header has no
//! effect. A trusted peer that gives no usable address fails closed: the proxy's
//! own address never stands in for the client.

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, PoisonError, RwLock};

/// An IP network. IPv4-mapped IPv6 addresses are unmapped everywhere they are
/// compared, so a dual-stack listener's `::ffff:10.0.0.1` is `10.0.0.1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: u128,
    prefix: u8,
    v6: bool,
}

/// Unmap an IPv4-mapped IPv6 address.
pub fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

fn bits(ip: IpAddr) -> (u128, bool) {
    match unmap(ip) {
        IpAddr::V4(v4) => (u128::from(u32::from(v4)), false),
        IpAddr::V6(v6) => (u128::from(v6), true),
    }
}

fn mask(prefix: u8, v6: bool) -> u128 {
    let width: u32 = if v6 { 128 } else { 32 };
    let prefix = u32::from(prefix).min(width);
    if prefix == 0 {
        0
    } else if prefix == 128 {
        u128::MAX
    } else {
        ((1u128 << prefix) - 1) << (width - prefix)
    }
}

impl Cidr {
    /// `a.b.c.d/n`, `x::y/n`, or a bare address (a host network). A network with
    /// host bits set, or inside `::ffff:0:0/96` (write it as IPv4), is refused.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (address, length) = match text.split_once('/') {
            Some((address, length)) => (address, Some(length)),
            None => (text, None),
        };
        let ip = IpAddr::from_str(address.trim())
            .map_err(|_| "is not an IP address or CIDR".to_string())?;
        if let IpAddr::V6(v6) = ip {
            if v6.to_ipv4_mapped().is_some() {
                return Err("is an IPv4-mapped address; write the IPv4 network".into());
            }
        }
        let v6 = ip.is_ipv6();
        let width: u8 = if v6 { 128 } else { 32 };
        let prefix = match length {
            None => width,
            Some(length) => length
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|prefix| *prefix <= width)
                .ok_or_else(|| format!("has a prefix length outside 0 to {width}"))?,
        };
        let (value, _) = bits(ip);
        let masked = value & mask(prefix, v6);
        if masked != value {
            return Err("has host bits set; write the network address".into());
        }
        Ok(Self {
            network: masked,
            prefix,
            v6,
        })
    }

    /// Whether `ip` (unmapped first) is inside this network.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let (value, v6) = bits(ip);
        v6 == self.v6 && value & mask(self.prefix, self.v6) == self.network
    }
}

/// The caller's address, or `None` when it cannot be known (which refuses a key
/// that has an allowlist).
pub fn client_address(
    peer: Option<IpAddr>,
    forwarded: &[&str],
    trusted: &[Cidr],
) -> Option<IpAddr> {
    let peer = unmap(peer?);
    if !trusted.iter().any(|net| net.contains(peer)) {
        return Some(peer);
    }
    // Right to left across every header line, in order.
    let entries: Vec<&str> = forwarded
        .iter()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .collect();
    for entry in entries.into_iter().rev() {
        // A bare address only: a port, brackets, a zone id or an empty entry is
        // not something to guess about.
        let ip = unmap(IpAddr::from_str(entry).ok()?);
        if !trusted.iter().any(|net| net.contains(ip)) {
            return Some(ip);
        }
    }
    None
}

type Table = HashMap<String, Arc<[Cidr]>>;

/// The allowlists in force, by credential scope (the digest that scopes a key's
/// state: a token's SHA-256, a certificate workload's `mtls:<name>`). Replaced as
/// one value on a keys reload.
#[derive(Clone, Default)]
pub struct AllowTable(Arc<RwLock<Arc<Table>>>);

impl AllowTable {
    pub fn new(table: Table) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(table))))
    }

    pub fn replace(&self, table: Table) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(table);
    }

    fn current(&self) -> Arc<Table> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Changes whenever any key's allowlist does, so a reload that changes only
    /// an allowlist is a change, and goes through the same debounce as a key.
    pub(crate) fn fingerprint_of(table: &Table) -> String {
        let mut lines: Vec<String> = table
            .iter()
            .map(|(scope, nets)| {
                format!(
                    "{:x}|{}",
                    Sha256::digest(scope.as_bytes()),
                    nets.iter()
                        .map(|n| format!("{}/{}/{}", n.network, n.prefix, n.v6))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            })
            .collect();
        lines.sort();
        format!("{:x}", Sha256::digest(lines.join("\n").as_bytes()))
    }

    pub(crate) fn fingerprint(&self) -> String {
        Self::fingerprint_of(&self.current())
    }
}

/// Refuse a request authenticated as a key whose allowlist does not hold the
/// caller. A request that does not authenticate passes through: the gateway
/// answers it (401), and it has no key to be bound.
pub(crate) async fn enforce(
    table: AllowTable,
    trusted: Arc<[Cidr]>,
    request: Request,
    next: Next,
) -> Response {
    let current = table.current();
    if current.is_empty() || crate::gateway::http::ADMIN_PATHS.contains(&request.uri().path()) {
        return next.run(request).await;
    }
    let native = crate::proxy::wire::native_route(request.uri().path()).map(|route| route.wire);
    let mut headers = request.headers().clone();
    if native.is_some() {
        crate::proxy::wire::normalize_auth(&mut headers);
    }
    // Keyed by the credential itself, not by asking the key store: the store can
    // be swapped by a reload between this check and the gateway's own, and a
    // check that needed it to identify the caller would pass an unknown one
    // through to a store that then knows it.
    let Some(scope) = crate::gateway::auth::credential_scope_of(&headers) else {
        return next.run(request).await;
    };
    let Some(allowed) = current.get(&scope) else {
        return next.run(request).await;
    };
    let peer = request
        .extensions()
        .get::<SocketAddr>()
        .map(|address| address.ip());
    let forwarded: Vec<&str> = request
        .headers()
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    // A header line that is not text is a line we cannot read: fail closed by
    // refusing to trust any of the header.
    let unreadable = forwarded.len() != request.headers().get_all("x-forwarded-for").iter().count();
    let address = if unreadable {
        None
    } else {
        client_address(peer, &forwarded, &trusted)
    };
    if address.is_some_and(|ip| allowed.iter().any(|net| net.contains(ip))) {
        return next.run(request).await;
    }
    let message = "This key cannot be used from this address";
    match native {
        Some(wire) => crate::proxy::wire::fail(wire, StatusCode::FORBIDDEN, message),
        None => (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": {"code": "forbidden", "message": message}})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(text: &str) -> Cidr {
        Cidr::parse(text).unwrap()
    }
    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn networks_match_by_prefix_in_both_families() {
        assert!(net("10.0.0.0/8").contains(ip("10.255.0.1")));
        assert!(!net("10.0.0.0/8").contains(ip("11.0.0.1")));
        assert!(net("2001:db8::/32").contains(ip("2001:db8:1::5")));
        assert!(!net("2001:db8::/32").contains(ip("2001:db9::1")));
        assert!(net("192.0.2.7").contains(ip("192.0.2.7")), "bare is a host");
        assert!(!net("192.0.2.7").contains(ip("192.0.2.8")));
        assert!(net("0.0.0.0/0").contains(ip("203.0.113.9")));
        assert!(!net("0.0.0.0/0").contains(ip("::1")), "families stay apart");
        assert!(net("::/0").contains(ip("::1")));
    }

    #[test]
    fn prefix_edges_hold_in_both_families() {
        assert!(net("10.1.2.3/32").contains(ip("10.1.2.3")));
        assert!(!net("10.1.2.3/32").contains(ip("10.1.2.2")));
        assert!(net("2001:db8::1/128").contains(ip("2001:db8::1")));
        assert!(!net("2001:db8::1/128").contains(ip("2001:db8::2")));
        assert!(
            net("0.0.0.0/0").contains(ip("::ffff:1.2.3.4")),
            "mapped is v4"
        );
        assert!(!net("::/0").contains(ip("::ffff:1.2.3.4")), "and not v6");
    }

    #[test]
    fn a_mapped_address_is_the_ipv4_address_everywhere() {
        assert!(net("10.0.0.0/8").contains(ip("::ffff:10.1.2.3")));
        assert!(!net("10.0.0.0/8").contains(ip("::ffff:11.1.2.3")));
        assert_eq!(unmap(ip("::ffff:10.1.2.3")), ip("10.1.2.3"));
    }

    #[test]
    fn bad_networks_are_refused_with_a_reason() {
        for bad in [
            "10.1.2.3/8",
            "10.0.0.0/33",
            "::/129",
            "::ffff:10.0.0.0/104",
            "nonsense",
            "10.0.0.0/x",
            "",
        ] {
            assert!(Cidr::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_peer_is_the_client_unless_it_is_a_trusted_proxy() {
        let trusted = [net("10.0.0.0/8")];
        // Not a proxy: a forged header is ignored.
        assert_eq!(
            client_address(Some(ip("203.0.113.9")), &["10.9.9.9"], &trusted),
            Some(ip("203.0.113.9"))
        );
        assert_eq!(
            client_address(Some(ip("203.0.113.9")), &["10.9.9.9"], &[]),
            Some(ip("203.0.113.9"))
        );
        // A proxy: the right-most entry that is not a proxy.
        assert_eq!(
            client_address(
                Some(ip("10.0.0.1")),
                &["198.51.100.1, 203.0.113.9, 10.0.0.2"],
                &trusted
            ),
            Some(ip("203.0.113.9"))
        );
        // Lines are joined in order.
        assert_eq!(
            client_address(
                Some(ip("10.0.0.1")),
                &["198.51.100.1", "203.0.113.9"],
                &trusted
            ),
            Some(ip("203.0.113.9"))
        );
        // A mapped proxy peer is still the proxy.
        assert_eq!(
            client_address(Some(ip("::ffff:10.0.0.1")), &["203.0.113.9"], &trusted),
            Some(ip("203.0.113.9"))
        );
    }

    #[test]
    fn a_trusted_peer_without_a_usable_address_fails_closed() {
        let trusted = [net("10.0.0.0/8")];
        let proxy = Some(ip("10.0.0.1"));
        assert_eq!(client_address(proxy, &[], &trusted), None, "no header");
        assert_eq!(
            client_address(proxy, &["10.0.0.5"], &trusted),
            None,
            "all proxies"
        );
        for junk in ["203.0.113.9:443", "[2001:db8::1]", "fe80::1%eth0", "", "x"] {
            assert_eq!(client_address(proxy, &[junk], &trusted), None, "{junk:?}");
        }
        assert_eq!(
            client_address(proxy, &["203.0.113.9, junk"], &trusted),
            None,
            "the right-most entry decides"
        );
        assert_eq!(
            client_address(None, &["203.0.113.9"], &trusted),
            None,
            "no peer"
        );
    }
}
