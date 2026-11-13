//! TLS material for `llmshim serve`: load the PEM files, and reload them when
//! they change so a renewed certificate needs no restart.

use axum_server::tls_rustls::RustlsConfig;
use llmshim::serve::config::Tls;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

/// Build the server configuration from the PEM files, and check that the key
/// belongs to the certificate. With `client_ca_file`, every client must present
/// a certificate signed by one of those authorities or the handshake fails.
/// Messages name files and never their contents.
fn build_server_config(tls: &Tls) -> Result<Arc<rustls::ServerConfig>, String> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let chain = CertificateDer::pem_file_iter(&tls.cert_file)
        .and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>())
        .map_err(|error| format!("cannot read the certificate chain: {error}"))?;
    if chain.is_empty() {
        return Err("the certificate file holds no certificate".into());
    }
    let key = PrivateKeyDer::from_pem_file(&tls.key_file)
        .map_err(|error| format!("cannot read the private key: {error}"))?;
    let builder = rustls::ServerConfig::builder();
    let builder = match &tls.client_ca_file {
        Some(path) => {
            let mut roots = rustls::RootCertStore::empty();
            let authorities = CertificateDer::pem_file_iter(path)
                .and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>())
                .map_err(|error| {
                    format!(
                        "cannot read the client CA bundle {}: {error}",
                        path.display()
                    )
                })?;
            if authorities.is_empty() {
                return Err(format!(
                    "the client CA bundle {} holds no certificate",
                    path.display()
                ));
            }
            for authority in authorities {
                roots
                    .add(authority)
                    .map_err(|error| format!("a client CA certificate is not usable: {error}"))?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .map_err(|error| format!("cannot build the client verifier: {error}"))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    let mut config = builder.with_single_cert(chain, key).map_err(|error| {
        format!("the key does not match the certificate or is unusable: {error}")
    })?;
    // `RustlsConfig::from_config` leaves the protocols to the caller.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Read the certificate chain and key (and the client CA bundle, if any), and
/// check that they belong together.
pub(crate) async fn load(tls: &Tls) -> Result<RustlsConfig, String> {
    // Installing twice is harmless; the first provider wins.
    let _ = rustls::crypto::ring::default_provider().install_default();
    build_server_config(tls)
        .map(RustlsConfig::from_config)
        .map_err(|error| {
            format!(
                "cannot load the TLS certificate {} and key {}: {error}",
                tls.cert_file.display(),
                tls.key_file.display()
            )
        })
}

/// A digest of a file's contents. Contents rather than timestamps or sizes:
/// a renewed certificate often has the same length as the one it replaces, and
/// some filesystems only keep one-second timestamps. The files are small, so
/// reading both every interval costs nothing.
type FileState = [u8; 32];

fn file_state(path: &std::path::Path) -> Option<FileState> {
    // `read` follows symlinks, which is how certificate managers swap in a
    // renewed secret.
    let contents = std::fs::read(path).ok()?;
    Some(Sha256::digest(&contents).into())
}

/// The state of both files. `None` for a file that cannot be read right now,
/// which is also a change worth noticing.
type Signature = (Option<FileState>, Option<FileState>, Option<FileState>);

fn signature(tls: &Tls) -> Signature {
    (
        file_state(&tls.cert_file),
        file_state(&tls.key_file),
        tls.client_ca_file.as_deref().and_then(file_state),
    )
}

/// Reload `config` whenever the files change. A replacement that does not load
/// (a half-written pair, a bad file) is reported once and ignored: the previous
/// certificate stays in service, and the next change is tried again.
pub(crate) fn watch(config: RustlsConfig, tls: Tls, label: &'static str) {
    let interval = Duration::from_secs(
        tls.reload_interval_secs
            .unwrap_or(Tls::DEFAULT_RELOAD_INTERVAL_SECS),
    );
    tokio::spawn(async move {
        let mut last = signature(&tls);
        loop {
            tokio::time::sleep(interval).await;
            let now = signature(&tls);
            if now == last {
                continue;
            }
            // Remember the state even if loading fails, so one bad file is
            // reported once instead of every interval.
            last = now;
            match build_server_config(&tls) {
                Ok(rebuilt) => {
                    config.reload_from_config(rebuilt);
                    eprintln!("{label}: reloaded the TLS certificate");
                }
                Err(error) => eprintln!(
                    "{label}: the TLS files changed but do not load ({error}); \
                     keeping the previous certificate"
                ),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tls_in(dir: &std::path::Path) -> Tls {
        let toml = format!(
            "cert_file = {:?}\nkey_file = {:?}",
            dir.join("c.pem"),
            dir.join("k.pem")
        );
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn a_change_to_either_file_changes_the_signature() {
        let dir = std::env::temp_dir().join(format!("llmshim-tls-sig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tls = tls_in(&dir);
        let missing = signature(&tls);
        assert_eq!(
            missing,
            (None, None, None),
            "unreadable files are a state too"
        );
        std::fs::write(&tls.cert_file, "one").unwrap();
        std::fs::write(&tls.key_file, "a").unwrap();
        let first = signature(&tls);
        assert_ne!(first, missing);
        assert_eq!(
            first,
            signature(&tls),
            "unchanged files, unchanged signature"
        );
        std::fs::write(&tls.key_file, "ab").unwrap();
        assert_ne!(first, signature(&tls), "a key change is noticed");
        // Same length, different content: still a change.
        std::fs::write(&tls.key_file, "ba").unwrap();
        let same_length = signature(&tls);
        std::fs::write(&tls.key_file, "ab").unwrap();
        assert_ne!(
            same_length,
            signature(&tls),
            "equal-length content is distinguished"
        );
        let second = signature(&tls);
        std::fs::write(&tls.cert_file, "three").unwrap();
        assert_ne!(second, signature(&tls), "a certificate change is noticed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn loading_reports_the_files_and_never_their_contents() {
        let dir = std::env::temp_dir().join(format!("llmshim-tls-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tls = tls_in(&dir);
        std::fs::write(&tls.cert_file, "not a certificate").unwrap();
        std::fs::write(&tls.key_file, "PRIVATE-KEY-MATERIAL-DO-NOT-PRINT").unwrap();
        let error = load(&tls).await.err().unwrap();
        assert!(
            error.contains("c.pem") && error.contains("k.pem"),
            "{error}"
        );
        assert!(!error.contains("PRIVATE-KEY-MATERIAL"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
