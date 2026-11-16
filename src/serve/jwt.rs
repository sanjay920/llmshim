//! Workload JWT verification (0017), as a unit: parse, select the key, check the
//! signature, then the claims. Nothing here fetches a key set or touches a
//! request; the wiring comes in later slices.
//!
//! The rules the note pins, in the order they act:
//!
//! - the token is bounded and must be three base64url parts, the first a JSON
//!   object;
//! - `alg` is accepted only if the issuer's policy lists it, and the policy can
//!   only list `RS256`, `ES256` and `EdDSA` (no `none`, no `HS*`: a public key
//!   must never be usable as a shared secret);
//! - a header with `crit`, or `b64: false`, is refused;
//! - the key comes from the issuer's key set by `kid` (or, without one, as the
//!   only usable key) and must fit the algorithm; the claims are read only after
//!   the signature verifies;
//! - `iss` is exact, `aud` contains the audience, `exp` is required and at most
//!   24 hours away, `nbf` and `iat` may not be in the future beyond the skew, and
//!   `sub` is the whole identity.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ring::signature;
use serde_json::{Map, Value};

/// The longest token examined. Real workload tokens are a few kilobytes.
pub const MAX_TOKEN_BYTES: usize = 16 * 1024;
/// The furthest `exp` may lie ahead of now.
pub const MAX_LIFETIME_SECS: f64 = 24.0 * 3600.0;
/// The longest `sub` accepted.
pub const MAX_SUBJECT_BYTES: usize = 512;

/// A signature algorithm an issuer may be configured with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alg {
    Rs256,
    Es256,
    EdDsa,
}

impl Alg {
    /// The JOSE name. Anything else (`none`, `HS256`, `PS256`, ...) is `Err`, so
    /// a configuration naming it fails at `--check`.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "RS256" => Ok(Self::Rs256),
            "ES256" => Ok(Self::Es256),
            "EdDSA" => Ok(Self::EdDsa),
            "none" | "HS256" | "HS384" | "HS512" => Err(format!(
                "{name} is not accepted: a symmetric or unsigned token must never verify"
            )),
            other => Err(format!(
                "{other} is not supported; use RS256, ES256 or EdDSA"
            )),
        }
    }
}

/// One issuer's rules.
#[derive(Debug, Clone)]
pub struct Policy {
    pub issuer: String,
    pub audience: String,
    pub algorithms: Vec<Alg>,
    pub skew_secs: u64,
}

/// Why a token did not verify. The class, never the token, is what gets logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Not a token this verifier can read.
    Malformed,
    /// The algorithm is not one the issuer allows.
    Algorithm,
    /// No usable key for this token (unknown `kid`, none or several without one,
    /// or a key of the wrong kind).
    Key,
    Signature,
    /// A claim is missing, wrong or out of range; the name says which.
    Claim(&'static str),
}

/// A verified token's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub subject: String,
}

#[derive(Debug, Clone)]
enum Material {
    /// DER `RSAPublicKey`.
    Rsa(Vec<u8>),
    /// Uncompressed P-256 point.
    Ec(Vec<u8>),
    /// 32 raw bytes.
    Ed25519(Vec<u8>),
}

impl Material {
    fn fits(&self, alg: Alg) -> bool {
        matches!(
            (self, alg),
            (Self::Rsa(_), Alg::Rs256) | (Self::Ec(_), Alg::Es256) | (Self::Ed25519(_), Alg::EdDsa)
        )
    }
}

#[derive(Debug, Clone)]
struct VerifyKey {
    kid: Option<String>,
    /// A JWK `alg`, which must equal the token's when present.
    alg: Option<String>,
    material: Material,
}

/// An issuer's usable verification keys.
#[derive(Debug, Clone, Default)]
pub struct KeySet {
    keys: Vec<VerifyKey>,
}

fn b64(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

fn der_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else {
        let bytes: Vec<u8> = len
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        let mut out = vec![0x80 | bytes.len() as u8];
        out.extend(bytes);
        out
    }
}

fn der_integer(bytes: &[u8]) -> Vec<u8> {
    let trimmed: Vec<u8> = bytes.iter().copied().skip_while(|b| *b == 0).collect();
    let mut body = Vec::new();
    if trimmed.first().is_none_or(|b| *b >= 0x80) {
        body.push(0);
    }
    body.extend(&trimmed);
    let mut out = vec![0x02];
    out.extend(der_length(body.len()));
    out.extend(body);
    out
}

fn rsa_public_key(n: &[u8], e: &[u8]) -> Option<Vec<u8>> {
    let modulus: Vec<u8> = n.iter().copied().skip_while(|b| *b == 0).collect();
    let bits = modulus.len() * 8 - modulus.first()?.leading_zeros() as usize;
    if !(2048..=8192).contains(&bits) || e.is_empty() {
        return None;
    }
    let mut body = der_integer(&modulus);
    body.extend(der_integer(e));
    let mut out = vec![0x30];
    out.extend(der_length(body.len()));
    out.extend(body);
    Some(out)
}

fn text<'a>(object: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    object.get(name)?.as_str()
}

impl VerifyKey {
    /// A key from one JWK, or `None` when it is not one this verifier can vouch
    /// for: encryption keys, other curves, a small RSA modulus, a key that may
    /// not verify.
    fn from_jwk(jwk: &Value) -> Option<Self> {
        let object = jwk.as_object()?;
        if let Some(usage) = object.get("use") {
            if usage.as_str() != Some("sig") {
                return None;
            }
        }
        if let Some(ops) = object.get("key_ops") {
            let ops = ops.as_array()?;
            if !ops.iter().any(|op| op.as_str() == Some("verify")) {
                return None;
            }
        }
        let material = match text(object, "kty")? {
            "RSA" => Material::Rsa(rsa_public_key(
                &b64(text(object, "n")?)?,
                &b64(text(object, "e")?)?,
            )?),
            "EC" => {
                if text(object, "crv")? != "P-256" {
                    return None;
                }
                let (x, y) = (b64(text(object, "x")?)?, b64(text(object, "y")?)?);
                if x.len() != 32 || y.len() != 32 {
                    return None;
                }
                let mut point = vec![0x04];
                point.extend(x);
                point.extend(y);
                Material::Ec(point)
            }
            "OKP" => {
                if text(object, "crv")? != "Ed25519" {
                    return None;
                }
                let x = b64(text(object, "x")?)?;
                if x.len() != 32 {
                    return None;
                }
                Material::Ed25519(x)
            }
            _ => return None,
        };
        Some(Self {
            kid: text(object, "kid").map(str::to_owned),
            alg: text(object, "alg").map(str::to_owned),
            material,
        })
    }
}

impl KeySet {
    /// The usable keys of a JWKS document. A key that is not usable is left out
    /// rather than failing the set (an issuer may publish encryption keys beside
    /// its signing keys). A `kid` that appears twice names nothing: neither copy
    /// is kept, so which key a token meant never depends on document order.
    pub fn from_jwks(document: &[u8]) -> Result<Self, String> {
        let value: Value =
            serde_json::from_slice(document).map_err(|_| "is not valid JSON".to_string())?;
        let keys = value
            .get("keys")
            .and_then(Value::as_array)
            .ok_or_else(|| "has no \"keys\" array".to_string())?;
        let usable: Vec<VerifyKey> = keys.iter().filter_map(VerifyKey::from_jwk).collect();
        let repeated: Vec<&String> = usable
            .iter()
            .filter_map(|key| key.kid.as_ref())
            .filter(|kid| {
                usable
                    .iter()
                    .filter(|other| other.kid.as_ref() == Some(*kid))
                    .count()
                    > 1
            })
            .collect();
        let keys = usable
            .iter()
            .filter(|key| key.kid.as_ref().is_none_or(|kid| !repeated.contains(&kid)))
            .cloned()
            .collect();
        Ok(Self { keys })
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn select(&self, kid: Option<&str>, alg: Alg, alg_name: &str) -> Option<&VerifyKey> {
        let fitting = |key: &&VerifyKey| {
            key.material.fits(alg) && key.alg.as_deref().is_none_or(|a| a == alg_name)
        };
        match kid {
            Some(kid) => self
                .keys
                .iter()
                .filter(fitting)
                .find(|key| key.kid.as_deref() == Some(kid)),
            None => {
                let mut candidates = self.keys.iter().filter(fitting);
                let only = candidates.next()?;
                candidates.next().is_none().then_some(only)
            }
        }
    }
}

/// Whether a bearer value has the shape of a JWT: three base64url parts, the
/// first a JSON object with a string `alg`. Decides only whether the value is
/// treated as a token or looked up as a key (0017).
pub fn is_jwt_shaped(token: &str) -> bool {
    if token.len() > MAX_TOKEN_BYTES {
        return false;
    }
    let mut parts = token.split('.');
    let (Some(head), Some(_), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    b64(head)
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("alg").map(Value::is_string))
        .unwrap_or(false)
}

/// The token's own, unverified, `iss`: read for one purpose, choosing which
/// issuer's rules apply. Nothing else from an unverified token is trusted.
pub fn peek_issuer(token: &str) -> Option<String> {
    if token.len() > MAX_TOKEN_BYTES {
        return None;
    }
    let payload = token.split('.').nth(1)?;
    let value: Value = serde_json::from_slice(&b64(payload)?).ok()?;
    value.get("iss")?.as_str().map(str::to_owned)
}

fn object(part: &str) -> Option<Map<String, Value>> {
    match serde_json::from_slice::<Value>(&b64(part)?).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Verify `token` against `policy` and `keys` at `now` (seconds since the epoch).
pub fn verify(token: &str, keys: &KeySet, policy: &Policy, now: u64) -> Result<Verified, Failure> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(Failure::Malformed);
    }
    let parts: Vec<&str> = token.split('.').collect();
    let [head, body, sig] = parts[..] else {
        return Err(Failure::Malformed);
    };
    if head.is_empty() || body.is_empty() || sig.is_empty() {
        return Err(Failure::Malformed);
    }
    let header = object(head).ok_or(Failure::Malformed)?;
    if header.contains_key("crit") || header.get("b64") == Some(&Value::Bool(false)) {
        return Err(Failure::Malformed);
    }
    let alg_name = text(&header, "alg").ok_or(Failure::Malformed)?;
    let alg = Alg::parse(alg_name)
        .ok()
        .filter(|alg| policy.algorithms.contains(alg))
        .ok_or(Failure::Algorithm)?;
    let key = keys
        .select(text(&header, "kid"), alg, alg_name)
        .ok_or(Failure::Key)?;
    let signature = b64(sig).ok_or(Failure::Malformed)?;
    let signed = &token[..head.len() + 1 + body.len()];
    let check = |algorithm: &'static dyn signature::VerificationAlgorithm, public: &[u8]| {
        signature::UnparsedPublicKey::new(algorithm, public)
            .verify(signed.as_bytes(), &signature)
            .map_err(|_| Failure::Signature)
    };
    match &key.material {
        Material::Rsa(public) => check(&signature::RSA_PKCS1_2048_8192_SHA256, public)?,
        Material::Ec(public) => check(&signature::ECDSA_P256_SHA256_FIXED, public)?,
        Material::Ed25519(public) => check(&signature::ED25519, public)?,
    }

    // Only now are the claims believed.
    let claims = object(body).ok_or(Failure::Malformed)?;
    if text(&claims, "iss") != Some(policy.issuer.as_str()) {
        return Err(Failure::Claim("iss"));
    }
    let audience_ok = match claims.get("aud") {
        Some(Value::String(one)) => one == &policy.audience,
        Some(Value::Array(many)) => many.iter().any(|a| a.as_str() == Some(&policy.audience)),
        _ => false,
    };
    if !audience_ok {
        return Err(Failure::Claim("aud"));
    }
    let (now, skew) = (now as f64, policy.skew_secs as f64);
    let number = |name: &'static str| -> Result<Option<f64>, Failure> {
        match claims.get(name) {
            None => Ok(None),
            Some(value) => value
                .as_f64()
                .filter(|n| n.is_finite())
                .map(Some)
                .ok_or(Failure::Claim(name)),
        }
    };
    let exp = number("exp")?.ok_or(Failure::Claim("exp"))?;
    if exp + skew <= now || exp > now + MAX_LIFETIME_SECS {
        return Err(Failure::Claim("exp"));
    }
    if number("nbf")?.is_some_and(|nbf| nbf > now + skew) {
        return Err(Failure::Claim("nbf"));
    }
    if number("iat")?.is_some_and(|iat| iat > now + skew) {
        return Err(Failure::Claim("iat"));
    }
    let subject = text(&claims, "sub")
        .filter(|s| !s.is_empty() && s.len() <= MAX_SUBJECT_BYTES)
        .ok_or(Failure::Claim("sub"))?;
    Ok(Verified {
        subject: subject.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, Ed25519KeyPair, KeyPair, RsaKeyPair};
    use serde_json::json;

    const NOW: u64 = 1_800_000_000;
    // Throwaway keys made for these tests; they sign nothing real.
    const RSA_2048: &str = include_str!("jwt_test_rsa_2048.b64");

    fn policy(algs: &[Alg]) -> Policy {
        Policy {
            issuer: "https://issuer.example".into(),
            audience: "llmshim".into(),
            algorithms: algs.to_vec(),
            skew_secs: 30,
        }
    }

    fn claims() -> Value {
        json!({"iss": "https://issuer.example", "aud": "llmshim", "sub": "svc:a",
               "exp": NOW + 600, "iat": NOW - 5})
    }

    fn encode(value: &Value) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap())
    }

    fn unsigned(header: &Value, claims: &Value) -> String {
        format!("{}.{}", encode(header), encode(claims))
    }

    enum Signer {
        Rsa(RsaKeyPair),
        Ec(EcdsaKeyPair),
        Ed(Ed25519KeyPair),
    }

    impl Signer {
        fn sign(&self, message: &str) -> String {
            let rng = SystemRandom::new();
            let bytes = match self {
                Signer::Rsa(pair) => {
                    let mut out = vec![0; pair.public().modulus_len()];
                    pair.sign(
                        &signature::RSA_PKCS1_SHA256,
                        &rng,
                        message.as_bytes(),
                        &mut out,
                    )
                    .unwrap();
                    out
                }
                Signer::Ec(pair) => pair
                    .sign(&rng, message.as_bytes())
                    .unwrap()
                    .as_ref()
                    .to_vec(),
                Signer::Ed(pair) => pair.sign(message.as_bytes()).as_ref().to_vec(),
            };
            URL_SAFE_NO_PAD.encode(bytes)
        }

        fn jwk(&self, kid: &str) -> Value {
            match self {
                Signer::Rsa(pair) => {
                    let (n, e) = rsa_parts(pair.public().as_ref());
                    json!({"kty": "RSA", "kid": kid,
                        "n": URL_SAFE_NO_PAD.encode(n),
                        "e": URL_SAFE_NO_PAD.encode(e)})
                }
                Signer::Ec(pair) => {
                    let point = pair.public_key().as_ref();
                    json!({"kty": "EC", "crv": "P-256", "kid": kid,
                        "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
                        "y": URL_SAFE_NO_PAD.encode(&point[33..65])})
                }
                Signer::Ed(pair) => json!({"kty": "OKP", "crv": "Ed25519", "kid": kid,
                    "x": URL_SAFE_NO_PAD.encode(pair.public_key().as_ref())}),
            }
        }

        fn token(&self, kid: Option<&str>, alg: &str, claims: &Value) -> String {
            let mut header = json!({"alg": alg, "typ": "JWT"});
            if let Some(kid) = kid {
                header["kid"] = json!(kid);
            }
            let message = unsigned(&header, claims);
            let signature = self.sign(&message);
            format!("{message}.{signature}")
        }
    }

    /// `(n, e)` from a DER `RSAPublicKey`, which is all ring exposes.
    fn rsa_parts(der: &[u8]) -> (Vec<u8>, Vec<u8>) {
        fn read(input: &[u8]) -> (&[u8], &[u8]) {
            let length = input[1] as usize;
            let (header, length) = if length < 0x80 {
                (2, length)
            } else {
                let count = length & 0x7f;
                let value = input[2..2 + count]
                    .iter()
                    .fold(0usize, |acc, b| (acc << 8) | *b as usize);
                (2 + count, value)
            };
            (&input[header..header + length], &input[header + length..])
        }
        let (body, _) = read(der);
        let (n, rest) = read(body);
        let (e, _) = read(rest);
        (n.to_vec(), e.to_vec())
    }

    fn rsa() -> Signer {
        let der = base64::engine::general_purpose::STANDARD
            .decode(RSA_2048.trim())
            .unwrap();
        Signer::Rsa(RsaKeyPair::from_pkcs8(&der).unwrap())
    }

    fn ec() -> Signer {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .unwrap();
        Signer::Ec(
            EcdsaKeyPair::from_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                pkcs8.as_ref(),
                &rng,
            )
            .unwrap(),
        )
    }

    fn ed() -> Signer {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        Signer::Ed(Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap())
    }

    fn set(keys: &[Value]) -> KeySet {
        KeySet::from_jwks(json!({ "keys": keys }).to_string().as_bytes()).unwrap()
    }

    fn all() -> [Alg; 3] {
        [Alg::Rs256, Alg::Es256, Alg::EdDsa]
    }

    #[test]
    fn every_supported_algorithm_verifies_and_yields_the_subject() {
        for (signer, alg) in [(rsa(), "RS256"), (ec(), "ES256"), (ed(), "EdDSA")] {
            let keys = set(&[signer.jwk("k1")]);
            let token = signer.token(Some("k1"), alg, &claims());
            assert_eq!(
                verify(&token, &keys, &policy(&all()), NOW),
                Ok(Verified {
                    subject: "svc:a".into()
                }),
                "{alg}"
            );
        }
    }

    #[test]
    fn the_algorithm_is_the_issuers_not_the_tokens() {
        assert!(Alg::parse("none").is_err());
        assert!(Alg::parse("HS256").is_err());
        assert!(Alg::parse("PS256").is_err());
        let signer = ed();
        let keys = set(&[signer.jwk("k")]);
        let token = signer.token(Some("k"), "EdDSA", &claims());
        // Valid, but the issuer allows only RS256.
        assert_eq!(
            verify(&token, &keys, &policy(&[Alg::Rs256]), NOW),
            Err(Failure::Algorithm)
        );
        // `alg: none` and `HS256` never reach a key.
        for alg in ["none", "HS256", "hs256", ""] {
            let forged = format!("{}.sig", unsigned(&json!({"alg": alg}), &claims()));
            assert_eq!(
                verify(&forged, &keys, &policy(&all()), NOW),
                Err(Failure::Algorithm),
                "{alg:?}"
            );
        }
    }

    #[test]
    fn a_public_key_cannot_be_used_as_a_shared_secret() {
        // The classic confusion: an HS256 token "signed" with the RSA public key.
        let signer = rsa();
        let keys = set(&[signer.jwk("k")]);
        let header = json!({"alg": "HS256", "kid": "k"});
        let message = unsigned(&header, &claims());
        let forged = format!("{message}.{}", URL_SAFE_NO_PAD.encode(b"anything"));
        assert_eq!(
            verify(&forged, &keys, &policy(&all()), NOW),
            Err(Failure::Algorithm)
        );
    }

    #[test]
    fn a_key_of_the_wrong_kind_never_verifies_for_the_algorithm() {
        let rsa_signer = rsa();
        let ec_signer = ec();
        let keys = set(&[rsa_signer.jwk("k")]);
        // An ES256 token naming an RSA key: no usable key for that algorithm.
        let token = ec_signer.token(Some("k"), "ES256", &claims());
        assert_eq!(
            verify(&token, &keys, &policy(&all()), NOW),
            Err(Failure::Key)
        );
    }

    #[test]
    fn a_wrong_signature_or_tampered_body_fails() {
        let signer = ed();
        let keys = set(&[signer.jwk("k")]);
        let token = signer.token(Some("k"), "EdDSA", &claims());
        let other = ed().token(Some("k"), "EdDSA", &claims());
        assert_eq!(
            verify(&other, &keys, &policy(&all()), NOW),
            Err(Failure::Signature)
        );
        let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
        let mut tampered = claims();
        tampered["sub"] = json!("svc:other");
        parts[1] = encode(&tampered);
        assert_eq!(
            verify(&parts.join("."), &keys, &policy(&all()), NOW),
            Err(Failure::Signature)
        );
    }

    #[test]
    fn the_key_is_chosen_by_kid_or_as_the_only_usable_one() {
        let (a, b) = (ed(), ed());
        let keys = set(&[a.jwk("a"), b.jwk("b")]);
        let by_b = b.token(Some("b"), "EdDSA", &claims());
        assert!(verify(&by_b, &keys, &policy(&all()), NOW).is_ok());
        let unknown = b.token(Some("zzz"), "EdDSA", &claims());
        assert_eq!(
            verify(&unknown, &keys, &policy(&all()), NOW),
            Err(Failure::Key)
        );
        // No kid: ambiguous with two keys, fine with one.
        let none = b.token(None, "EdDSA", &claims());
        assert_eq!(
            verify(&none, &keys, &policy(&all()), NOW),
            Err(Failure::Key)
        );
        let single = set(&[b.jwk("b")]);
        assert!(verify(&none, &single, &policy(&all()), NOW).is_ok());
    }

    #[test]
    fn a_repeated_kid_names_nothing() {
        let (a, b) = (ed(), ed());
        let keys = set(&[a.jwk("same"), b.jwk("same")]);
        assert!(keys.is_empty());
    }

    #[test]
    fn unusable_keys_are_left_out_and_the_rest_still_work() {
        let signer = ed();
        // A 1024-bit modulus (ring will not even load such a pair, so a
        // synthetic public key stands in).
        let small = json!({"kty": "RSA", "kid": "small",
            "n": URL_SAFE_NO_PAD.encode([0xC3u8; 128]), "e": "AQAB"});
        let mut encryption = signer.jwk("enc");
        encryption["use"] = json!("enc");
        let mut no_verify = signer.jwk("nv");
        no_verify["key_ops"] = json!(["sign"]);
        let mut other_curve = ec().jwk("p384");
        other_curve["crv"] = json!("P-384");
        let keys = set(&[
            small,
            encryption,
            no_verify,
            other_curve,
            json!({"kty": "oct", "k": "AAAA", "kid": "sym"}),
            signer.jwk("good"),
        ]);
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn a_jwk_alg_must_match_the_tokens() {
        let signer = ed();
        let mut jwk = signer.jwk("k");
        jwk["alg"] = json!("RS256");
        let keys = set(&[jwk]);
        let token = signer.token(Some("k"), "EdDSA", &claims());
        assert_eq!(
            verify(&token, &keys, &policy(&all()), NOW),
            Err(Failure::Key)
        );
    }

    #[test]
    fn headers_that_change_how_a_token_is_read_are_refused() {
        let signer = ed();
        let keys = set(&[signer.jwk("k")]);
        for extra in [("crit", json!(["exp"])), ("b64", json!(false))] {
            let mut header = json!({"alg": "EdDSA", "kid": "k"});
            header[extra.0] = extra.1;
            let message = unsigned(&header, &claims());
            let token = format!("{message}.{}", signer.sign(&message));
            assert_eq!(
                verify(&token, &keys, &policy(&all()), NOW),
                Err(Failure::Malformed),
                "{}",
                extra.0
            );
        }
    }

    #[test]
    fn claims_are_checked_after_the_signature_and_exactly() {
        let signer = ed();
        let keys = set(&[signer.jwk("k")]);
        let case = |mutate: &dyn Fn(&mut Value)| {
            let mut c = claims();
            mutate(&mut c);
            verify(
                &signer.token(Some("k"), "EdDSA", &c),
                &keys,
                &policy(&all()),
                NOW,
            )
        };
        assert_eq!(
            case(&|c| c["iss"] = json!("https://issuer.example/")),
            Err(Failure::Claim("iss"))
        );
        assert_eq!(
            case(&|c| c["aud"] = json!("other")),
            Err(Failure::Claim("aud"))
        );
        assert!(case(&|c| c["aud"] = json!(["x", "llmshim"])).is_ok());
        assert_eq!(
            case(&|c| c["aud"] = json!(["x"])),
            Err(Failure::Claim("aud"))
        );
        assert_eq!(
            case(&|c| {
                c.as_object_mut().unwrap().remove("aud");
            }),
            Err(Failure::Claim("aud"))
        );
        assert_eq!(
            case(&|c| {
                c.as_object_mut().unwrap().remove("exp");
            }),
            Err(Failure::Claim("exp"))
        );
        assert_eq!(
            case(&|c| c["exp"] = json!("soon")),
            Err(Failure::Claim("exp"))
        );
        assert_eq!(
            case(&|c| {
                c.as_object_mut().unwrap().remove("sub");
            }),
            Err(Failure::Claim("sub"))
        );
        assert_eq!(case(&|c| c["sub"] = json!("")), Err(Failure::Claim("sub")));
        assert_eq!(
            case(&|c| c["sub"] = json!("x".repeat(513))),
            Err(Failure::Claim("sub"))
        );
        assert_eq!(case(&|c| c["sub"] = json!(7)), Err(Failure::Claim("sub")));
    }

    #[test]
    fn time_claims_hold_at_the_skew_edges() {
        let signer = ed();
        let keys = set(&[signer.jwk("k")]);
        let at = |name: &str, value: u64| {
            let mut c = claims();
            c[name] = json!(value);
            verify(
                &signer.token(Some("k"), "EdDSA", &c),
                &keys,
                &policy(&all()),
                NOW,
            )
        };
        // exp: expired by more than the skew fails; within it passes.
        assert!(at("exp", NOW - 29).is_ok());
        assert_eq!(at("exp", NOW - 30), Err(Failure::Claim("exp")));
        // exp more than 24 hours away fails, with or without iat.
        assert!(at("exp", NOW + 86_400).is_ok());
        assert_eq!(at("exp", NOW + 86_401), Err(Failure::Claim("exp")));
        let mut no_iat = claims();
        no_iat.as_object_mut().unwrap().remove("iat");
        no_iat["exp"] = json!(NOW + 10 * 86_400);
        assert_eq!(
            verify(
                &signer.token(Some("k"), "EdDSA", &no_iat),
                &keys,
                &policy(&all()),
                NOW
            ),
            Err(Failure::Claim("exp")),
            "omitting iat buys nothing"
        );
        // nbf and iat may not be ahead of now by more than the skew.
        assert!(at("nbf", NOW + 30).is_ok());
        assert_eq!(at("nbf", NOW + 31), Err(Failure::Claim("nbf")));
        assert!(at("iat", NOW + 30).is_ok());
        assert_eq!(at("iat", NOW + 31), Err(Failure::Claim("iat")));
    }

    #[test]
    fn oversized_and_malformed_tokens_are_refused() {
        let keys = set(&[]);
        let p = policy(&all());
        assert_eq!(
            verify(&"a".repeat(MAX_TOKEN_BYTES + 1), &keys, &p, NOW),
            Err(Failure::Malformed)
        );
        for bad in [
            "",
            "a",
            "a.b",
            "a.b.c.d",
            "..",
            "a..c",
            "!!.!!.!!",
            "e30.e30.e30",
        ] {
            assert!(
                matches!(
                    verify(bad, &keys, &p, NOW),
                    Err(Failure::Malformed | Failure::Algorithm)
                ),
                "{bad:?}"
            );
        }
        // A header that is JSON but not an object.
        let token = format!("{}.{}.c2ln", encode(&json!([1])), encode(&claims()));
        assert_eq!(verify(&token, &keys, &p, NOW), Err(Failure::Malformed));
    }

    #[test]
    fn token_shape_and_unverified_issuer() {
        let signer = ed();
        let token = signer.token(Some("k"), "EdDSA", &claims());
        assert!(is_jwt_shaped(&token));
        assert_eq!(
            peek_issuer(&token).as_deref(),
            Some("https://issuer.example")
        );
        for not in [
            "plain-key-0123456789abcdef",
            "a.b.c",
            "a.b",
            "e30.e30.e30",
            "",
        ] {
            assert!(!is_jwt_shaped(not), "{not:?}");
        }
        assert!(!is_jwt_shaped(&"a".repeat(MAX_TOKEN_BYTES + 1)));
        assert_eq!(peek_issuer("nonsense"), None);
    }
}
