use super::{
    auth::{auth_error, ChatGptAuth},
    tokens::Tokens,
};
use crate::error::Result;
use fs2::FileExt;
use serde_json::Value;
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::{sleep, Instant};

impl ChatGptAuth {
    fn storage_error(&self, _: impl std::fmt::Display) -> crate::error::ShimError {
        auth_error(
            500,
            &format!(
                "cannot read or update the OAuth cache {}",
                self.path.display()
            ),
        )
    }

    pub(super) fn read_document(&self) -> Result<Option<Value>> {
        let mut data = Vec::new();
        if let Some(protected_default_root) = &self.protected_default_root {
            let mut file_handle = crate::default_secret_file::open_default_secret_file(
                protected_default_root,
                &["chatgpt"],
                "auth.json",
            )
            .map_err(|error| self.storage_error(error))?;
            let Some(file_handle) = file_handle.as_mut() else {
                return Ok(None);
            };
            file_handle
                .read_to_end(&mut data)
                .map_err(|error| self.storage_error(error))?;
        } else {
            data = match std::fs::read(&self.path) {
                Ok(data) => data,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(self.storage_error(e)),
            };
        };
        serde_json::from_slice(&data).map(Some).map_err(|_| {
            auth_error(
                401,
                &format!(
                    "invalid OAuth cache {}; run `llmshim login chatgpt`",
                    self.path.display()
                ),
            )
        })
    }

    pub(super) fn read(&self) -> Result<Option<Tokens>> {
        let Some(document) = self.read_document()? else {
            return Ok(None);
        };
        let value = document.get("tokens").unwrap_or(&document).clone();
        let mut tokens: Tokens = serde_json::from_value(value).map_err(|_| {
            auth_error(
                401,
                &format!(
                    "invalid OAuth cache {}; run `llmshim login chatgpt`",
                    self.path.display()
                ),
            )
        })?;
        tokens.normalize();
        Ok(Some(tokens))
    }

    fn parent(&self) -> &Path {
        self.path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
    }

    pub(super) async fn lock(&self) -> Result<File> {
        let mut dir = std::fs::DirBuilder::new();
        dir.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            dir.mode(0o700);
        }
        dir.create(self.parent())
            .map_err(|error| self.storage_error(error))?;
        let mut lock_name = self.path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(PathBuf::from(lock_name))
            .map_err(|error| self.storage_error(error))?;
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(file),
                Err(e)
                    if e.raw_os_error() == fs2::lock_contended_error().raw_os_error()
                        && Instant::now() < deadline =>
                {
                    sleep(Duration::from_millis(50)).await
                }
                Err(_) => return Err(auth_error(503, "OAuth cache is busy; retry shortly")),
            }
        }
    }

    pub(super) fn save(&self, tokens: &Tokens, document: Option<Value>) -> Result<()> {
        let mut value = serde_json::to_value(tokens).map_err(|error| self.storage_error(error))?;
        if let Some(mut document) = document {
            if let Some(nested) = document.get_mut("tokens").and_then(Value::as_object_mut) {
                for key in [
                    "access_token",
                    "refresh_token",
                    "id_token",
                    "account_id",
                    "expires_at",
                ] {
                    // A field this refresh has no value for is left as the file had it: writing
                    // null over a string the CLI requires would break the CLI's own reader.
                    if !value[key].is_null() {
                        nested.insert(key.into(), value[key].clone());
                    }
                }
                document["last_refresh"] = serde_json::to_value(chrono::Utc::now())
                    .map_err(|error| self.storage_error(error))?;
                value = document;
            }
        }
        // NamedTempFile is owner-only on Unix. Atomic replacement means readers
        // see either complete generation, including rotated refresh tokens.
        let mut file = tempfile::NamedTempFile::new_in(self.parent())
            .map_err(|error| self.storage_error(error))?;
        file.write_all(&serde_json::to_vec(&value).map_err(|error| self.storage_error(error))?)
            .map_err(|error| self.storage_error(error))?;
        file.as_file()
            .sync_all()
            .map_err(|error| self.storage_error(error))?;
        file.persist(&self.path)
            .map_err(|error| self.storage_error(error))?;
        Ok(())
    }

    pub async fn logout(&self) -> Result<()> {
        let _lock = self.lock().await?;
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.storage_error(e)),
        }
    }
}
