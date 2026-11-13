//! A destination for JSON lines: `"stdout"` or an absolute file path.

use serde::Serialize;
use std::io::Write;
use std::sync::Mutex;

/// Appends one JSON object per line. A write that fails (full disk, closed
/// pipe) is dropped: logging must never fail a request.
pub(crate) struct JsonLines {
    writer: Mutex<Box<dyn Write + Send>>,
}

impl JsonLines {
    /// `"stdout"`, or an absolute file path created owner-only (0600) and
    /// appended to.
    pub(crate) fn open(target: &str) -> Result<Self, String> {
        let writer: Box<dyn Write + Send> = if target == "stdout" {
            Box::new(std::io::stdout())
        } else {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            Box::new(
                options
                    .open(target)
                    .map_err(|error| format!("cannot open {target}: {}", error.kind()))?,
            )
        };
        Ok(Self::to_writer(writer))
    }

    pub(crate) fn to_writer(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Mutex::new(writer),
        }
    }

    pub(crate) fn write<T: Serialize>(&self, record: &T) {
        if let Ok(line) = serde_json::to_string(record) {
            if let Ok(mut writer) = self.writer.lock() {
                let _ = writeln!(writer, "{line}");
                let _ = writer.flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_target_is_created_owner_only_and_appended_to() {
        let dir = std::env::temp_dir().join(format!("llmshim-jsonl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.jsonl");
        let target = path.to_str().unwrap();
        for index in 0..2 {
            JsonLines::open(target)
                .unwrap()
                .write(&serde_json::json!({"n": index}));
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text
            .lines()
            .all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        assert!(JsonLines::open(dir.join("missing/dir/log").to_str().unwrap()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
