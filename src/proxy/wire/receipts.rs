//! Issued metadata for native clients that cannot retain canonical wire maps.
//! Records are private local files, scoped to the inbound credential digest.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{io::Write, path::PathBuf};
#[derive(Debug)]
pub struct Receipts {
    root: PathBuf,
}
impl Receipts {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn from_env() -> Self {
        let root = std::env::var_os("LLMSHIM_REPLAY_RECEIPTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::data_local_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("llmshim/replay-receipts")
            });
        Self::new(root)
    }
    fn path(&self, scope: &str, kind: &str, key: &Value) -> PathBuf {
        let mut digest = Sha256::new();
        for field in [
            b"llmshim-native-receipt-v1".as_slice(),
            scope.as_bytes(),
            kind.as_bytes(),
            key.to_string().as_bytes(),
        ] {
            digest.update((field.len() as u64).to_be_bytes());
            digest.update(field);
        }
        self.root.join(format!("{:x}.json", digest.finalize()))
    }
    pub fn put(&self, scope: &str, kind: &str, key: &Value, value: &Value) -> Result<(), String> {
        let bytes = serde_json::to_vec(value).map_err(|_| error())?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(error());
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.root).map_err(|_| error())?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.root).map_err(|_| error())?;
        temp.write_all(&bytes).map_err(|_| error())?;
        temp.as_file().sync_all().map_err(|_| error())?;
        temp.persist(self.path(scope, kind, key))
            .map_err(|_| error())?;
        Ok(())
    }
    pub fn get(&self, scope: &str, kind: &str, key: &Value) -> Result<Option<Value>, String> {
        let path = self.path(scope, kind, key);
        match std::fs::metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(error()),
            Ok(meta) if meta.len() > 4 * 1024 * 1024 => return Err(error()),
            _ => {}
        }
        let data = std::fs::read(path).map_err(|_| error())?;
        serde_json::from_slice(&data).map(Some).map_err(|_| error())
    }
}
fn error() -> String {
    "native replay metadata is unavailable".into()
}
