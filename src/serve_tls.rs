//! TLS material for `llmshim serve`: load the PEM files, and reload them when
//! they change so a renewed certificate needs no restart.

use axum_server::tls_rustls::RustlsConfig;
use llmshim::serve::config::Tls;
use sha2::{Digest, Sha256};
use std::time::Duration;

/// A digest of a file's contents. Contents rather than timestamps or sizes:
/// a renewed certificate often has the same length as the one it replaces, and
/// some filesystems only keep one-second timestamps. The files are small, so
/// reading both every interval costs nothing.
type FileState = [u8; 32];

/// The state of both files. `None` for a file that cannot be read right now,
/// which is also a change worth noticing.
pub(crate) type Signature = (Option<FileState>, Option<FileState>);

/// What was read from the two files, once. The reload loads from exactly these
/// bytes and the signature is taken from them, so what is loaded and what is
/// remembered as loaded cannot disagree (a renewal that lands between two
/// separate reads would otherwise be loaded but not noticed, or noticed but
/// never loaded).
struct Snapshot {
    cert: Option<Vec<u8>>,
    key: Option<Vec<u8>>,
}

impl Snapshot {
    /// `read` follows symlinks, which is how certificate managers swap in a
    /// renewed secret. Async, so a hung network volume stalls a task rather
    /// than a runtime worker.
    async fn take(tls: &Tls) -> Self {
        Self {
            cert: tokio::fs::read(&tls.cert_file).await.ok(),
            key: tokio::fs::read(&tls.key_file).await.ok(),
        }
    }

    fn signature(&self) -> Signature {
        let state = |bytes: &Option<Vec<u8>>| bytes.as_ref().map(|b| Sha256::digest(b).into());
        (state(&self.cert), state(&self.key))
    }
}

/// The files of one listener and what was loaded from them at startup, for the
/// reload watcher.
#[derive(Clone)]
pub(crate) struct Watched {
    files: Tls,
    loaded: Signature,
}

/// Read the certificate chain and key, and check that they belong together.
/// The message names the files, never their contents.
pub(crate) async fn load(tls: &Tls) -> Result<(RustlsConfig, Watched), String> {
    // Installing twice is harmless; the first provider wins.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let snapshot = Snapshot::take(tls).await;
    let named = || {
        format!(
            "the TLS certificate {} and key {}",
            tls.cert_file.display(),
            tls.key_file.display()
        )
    };
    let (Some(cert), Some(key)) = (snapshot.cert.clone(), snapshot.key.clone()) else {
        return Err(format!("cannot read {}", named()));
    };
    let config = RustlsConfig::from_pem(cert, key)
        .await
        .map_err(|error| format!("cannot load {}: {error}", named()))?;
    Ok((
        config,
        Watched {
            files: tls.clone(),
            loaded: snapshot.signature(),
        },
    ))
}

/// Reload `config` whenever the files change. A replacement that does not load
/// (a half-written pair, a bad file) is reported once and ignored: the previous
/// certificate stays in service. It is also tried again every interval, because
/// a failure can be transient (a descriptor limit, a swap in progress); only a
/// successful load, never a failed one, counts as having applied the files.
pub(crate) fn watch(config: RustlsConfig, watched: Watched, label: &'static str) {
    let Watched { files: tls, loaded } = watched;
    let interval = Duration::from_secs(
        tls.reload_interval_secs
            .unwrap_or(Tls::DEFAULT_RELOAD_INTERVAL_SECS),
    );
    tokio::spawn(async move {
        // The state of the files that is in service. It starts as what the
        // startup load read, not as a fresh read here, so a renewal that landed
        // between that load and this task is still picked up.
        let mut applied = loaded;
        // The state last reported as failing, so a bad pair is logged once.
        let mut reported: Option<Signature> = None;
        loop {
            tokio::time::sleep(interval).await;
            let snapshot = Snapshot::take(&tls).await;
            let now = snapshot.signature();
            if now == applied {
                continue;
            }
            let result = match (snapshot.cert, snapshot.key) {
                (Some(cert), Some(key)) => config
                    .reload_from_pem(cert, key)
                    .await
                    .map_err(|error| error.to_string()),
                _ => Err("a file cannot be read".to_string()),
            };
            match result {
                Ok(()) => {
                    applied = now;
                    reported = None;
                    eprintln!("{label}: reloaded the TLS certificate");
                }
                Err(error) => {
                    if reported != Some(now) {
                        eprintln!(
                            "{label}: the TLS files changed but do not load ({error}); \
                             keeping the previous certificate"
                        );
                        reported = Some(now);
                    }
                }
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
        let signature = |tls: &Tls| {
            let tls = tls.clone();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move { Snapshot::take(&tls).await.signature() })
        };
        let missing = signature(&tls);
        assert_eq!(missing, (None, None), "unreadable files are a state too");
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

    fn pair() -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = rcgen::CertificateParams::new(vec!["localhost".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (certificate.pem(), key.serialize_pem())
    }

    fn fast_tls_in(dir: &std::path::Path) -> Tls {
        let toml = format!(
            "cert_file = {:?}\nkey_file = {:?}\nreload_interval_secs = 1",
            dir.join("c.pem"),
            dir.join("k.pem")
        );
        toml::from_str(&toml).unwrap()
    }

    /// Identifies the configuration in service: a reload replaces it.
    fn in_service(config: &RustlsConfig) -> usize {
        std::sync::Arc::as_ptr(&config.get_inner()) as *const () as usize
    }

    async fn changes_within(config: &RustlsConfig, was: usize, seconds: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
        while std::time::Instant::now() < deadline {
            if in_service(config) != was {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_renewal_between_the_startup_load_and_the_watcher_is_not_lost() {
        let dir = std::env::temp_dir().join(format!("llmshim-tls-gap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tls = fast_tls_in(&dir);
        let (cert, key) = pair();
        std::fs::write(&tls.cert_file, cert).unwrap();
        std::fs::write(&tls.key_file, key).unwrap();
        let (config, watched) = load(&tls).await.unwrap();
        let before = in_service(&config);
        // The certificate is renewed after startup loaded it but before the
        // watcher began: the watcher must compare with what was loaded, not with
        // whatever it finds when it first looks.
        let (cert, key) = pair();
        std::fs::write(&tls.cert_file, cert).unwrap();
        std::fs::write(&tls.key_file, key).unwrap();
        watch(config.clone(), watched, "test");
        assert!(
            changes_within(&config, before, 6).await,
            "the renewed certificate was never loaded"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_bad_replacement_is_ignored_and_a_later_good_one_is_applied() {
        let dir = std::env::temp_dir().join(format!("llmshim-tls-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tls = fast_tls_in(&dir);
        let (cert, key) = pair();
        std::fs::write(&tls.cert_file, cert).unwrap();
        std::fs::write(&tls.key_file, key).unwrap();
        let (config, watched) = load(&tls).await.unwrap();
        watch(config.clone(), watched, "test");
        let before = in_service(&config);
        std::fs::write(&tls.cert_file, "half a certificate").unwrap();
        assert!(
            !changes_within(&config, before, 3).await,
            "a pair that does not load must leave the old certificate in service"
        );
        let (cert, key) = pair();
        std::fs::write(&tls.cert_file, cert).unwrap();
        std::fs::write(&tls.key_file, key).unwrap();
        assert!(
            changes_within(&config, before, 6).await,
            "the good pair was never applied after the bad one"
        );
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
