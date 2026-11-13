# 0011: Mutual TLS and certificate-derived identity

Status: implemented for the main listener.

## What it is

```toml
[server.tls]
cert_file = "/etc/llmshim/tls/tls.crt"
key_file  = "/etc/llmshim/tls/tls.key"
client_ca_file = "/etc/llmshim/tls/ca.crt"   # require client certificates

[[keys]]
name = "checkout"
tenant = "payments"
client_san = "uri:spiffe://corp/payments/checkout"
tier = 2
rpm = 1200
```

With `client_ca_file`, **every** connection to the listener must present a
certificate signed by one of those authorities, or the TLS handshake fails and no
request is ever read. A `[[keys]]` entry with `client_san` then authenticates a
workload by that certificate alone: no bearer token to distribute, store or
rotate. The same limits, budgets and tenant apply as for any other key.

`client_san` is `dns:<name>` or `uri:<uri>` and matches a subject alternative name
of the verified certificate. It names the **workload**, not one certificate: a
service mesh or SPIFFE issuer renews a certificate every few hours, so pinning a
fingerprint would break constantly. DNS names compare case-insensitively, URIs
exactly.

## Rules

- **Verified first.** rustls checks the chain against the configured authorities
  during the handshake (`WebPkiClientVerifier`). Nothing here reads an unverified
  certificate.
- **A bearer token decides alone.** A request that sends `Authorization: Bearer`
  is authenticated by the token; a valid certificate never rescues a wrong one,
  and bearer keys keep working beside certificate keys.
- **Unknown workload fails closed.** A certificate the authority signed, naming a
  workload no key lists, completes the handshake and gets 401.
- **Main listener only.** `admin.tls.client_ca_file` is refused; scrapers and
  probes keep using the admin token (0005).
- Validation: a `client_san` key needs `server.tls.client_ca_file` (otherwise no
  certificate is ever requested and the key could never match); a key sets
  exactly one of `secret`, `sha256` and `client_san`; duplicate names collide
  (DNS case-insensitively).

## How the identity reaches the gateway, and why it cannot be forged

The gateway authenticates from request headers. After the handshake, an acceptor
reads the verified peer's names (`llmshim::serve::mtls::peer_sans`, using
`x509-parser`) and attaches them to every request of that connection. An
outermost middleware, `bind_peer_identity`, then:

1. **removes** the internal header `x-llmshim-peer-san` from the request, always;
2. sets it to the first of the certificate's names that a configured key lists,
   if any.

The key store (`KeyStore::Mutual`) reads that header. Because the middleware
strips whatever the caller sent before setting its own value, and runs outside
every other layer, the access log and the gateway see only values set from the
handshake. The workload's credential scope is derived in a space tokens cannot
reach, so idempotency and receipt state never collide with a token's.

A test pins this by **mutation**: with the strip disabled, an unknown workload
that sends `x-llmshim-peer-san: uri:spiffe://corp/payments/checkout` is let in
(HTTP 200). With the strip in place it, and a genuine workload claiming another
workload's name, both keep their real identity.

## Renewal

The server certificate, its key and the client CA bundle are watched together
(0007). A rotated CA bundle takes effect for new connections without a restart; a
bundle that does not load is ignored and the previous one stays.

## What is proven

Against a real server with a real CA, real client certificates and real
handshakes: a checkout and a batch workload are each attributed to their tenant
(read from the access log, which resolves callers with the same key store); no
certificate, a certificate from an unknown authority, and a self-signed
certificate all fail the handshake and no request completes; an unknown workload
is 401; forged headers change nothing; a wrong bearer token is refused despite a
valid certificate; and `--check` rejects a `client_san` key without
`client_ca_file`.

## Not done

- **Optional client certificates** (accept both certificate and token callers on
  one listener without requiring every connection to have a certificate). Today
  the listener either requires a certificate or does not ask for one.
- **Revocation** (CRL, OCSP). Short-lived certificates are the intended answer; a
  CRL file watched like the CA bundle is a later item.
- **Matching on subject fields**, SAN wildcards, or mapping a SAN pattern to a
  tenant. Each workload is listed; a pattern mapping belongs with the control
  plane.
- **Certificate expiry metrics.**
