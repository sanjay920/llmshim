//! A directory every replica can read and write (note 0050): a local path, or a shared volume (a
//! `ReadWriteMany` claim, NFS, EFS, Azure Files). Objects are written to a temporary file and renamed,
//! so a reader never sees half an object, and are owner-only.

use super::{valid_name, BlobError, BlobStore, Listed};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct FileStore {
    root: PathBuf,
    max_object_bytes: usize,
}

impl FileStore {
    /// `root` is created (owner-only) if it is not there.
    pub fn new(root: &Path, max_object_bytes: usize) -> Result<Self, String> {
        std::fs::create_dir_all(root)
            .map_err(|e| format!("cannot create {}: {}", root.display(), e.kind()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Only a directory this process made is tightened; a mounted volume's mode is the
            // mount's (a changed mode on a shared volume is the operator's call).
            if std::fs::read_dir(root).is_ok_and(|mut d| d.next().is_none()) {
                let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700));
            }
        }
        let canonical = root
            .canonicalize()
            .map_err(|e| format!("cannot read {}: {}", root.display(), e.kind()))?;
        Ok(Self {
            root: canonical,
            max_object_bytes,
        })
    }

    fn path(&self, name: &str) -> Result<PathBuf, BlobError> {
        if !valid_name(name) {
            return Err(BlobError::Invalid);
        }
        Ok(self.root.join(name))
    }
}

fn classify(error: &std::io::Error) -> BlobError {
    match error.kind() {
        std::io::ErrorKind::NotFound => BlobError::NotFound,
        std::io::ErrorKind::PermissionDenied => BlobError::Denied,
        _ => BlobError::Unavailable,
    }
}

#[async_trait]
impl BlobStore for FileStore {
    async fn put(&self, name: &str, bytes: Vec<u8>) -> Result<(), BlobError> {
        let path = self.path(name)?;
        tokio::task::spawn_blocking(move || {
            let parent = path.parent().ok_or(BlobError::Invalid)?;
            std::fs::create_dir_all(parent).map_err(|e| classify(&e))?;
            let temporary = parent.join(format!(
                ".tmp-{}",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
            ));
            {
                use std::io::Write;
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create(true).truncate(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(&temporary).map_err(|e| classify(&e))?;
                file.write_all(&bytes)
                    .and_then(|()| file.sync_all())
                    .map_err(|e| {
                        let _ = std::fs::remove_file(&temporary);
                        classify(&e)
                    })?;
            }
            std::fs::rename(&temporary, &path).map_err(|e| {
                let _ = std::fs::remove_file(&temporary);
                classify(&e)
            })
        })
        .await
        .map_err(|_| BlobError::Unavailable)?
    }

    async fn get(&self, name: &str, max: usize) -> Result<Vec<u8>, BlobError> {
        let path = self.path(name)?;
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let file = std::fs::File::open(&path).map_err(|e| classify(&e))?;
            let mut bytes = Vec::new();
            // One byte past the cap tells a larger object from one of exactly `max`.
            file.take(max as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| classify(&e))?;
            if bytes.len() > max {
                return Err(BlobError::TooLarge);
            }
            Ok(bytes)
        })
        .await
        .map_err(|_| BlobError::Unavailable)?
    }

    async fn delete(&self, name: &str) -> Result<(), BlobError> {
        let path = self.path(name)?;
        tokio::task::spawn_blocking(move || {
            // The object, and the temporary file a crashed write may have left under it.
            let temporary = path.parent().map(|parent| {
                parent.join(format!(
                    ".tmp-{}",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
                ))
            });
            for target in std::iter::once(path.clone()).chain(temporary) {
                match std::fs::remove_file(&target) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(classify(&e)),
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| BlobError::Unavailable)?
    }

    async fn list(&self) -> Result<Vec<Listed>, BlobError> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            fn walk(dir: &Path, root: &Path, out: &mut Vec<Listed>) -> std::io::Result<()> {
                for entry in std::fs::read_dir(dir)? {
                    let entry = entry?;
                    let kind = entry.file_type()?;
                    if kind.is_symlink() {
                        // Never followed: a link in the store is not an object.
                        continue;
                    }
                    let path = entry.path();
                    if kind.is_dir() {
                        walk(&path, root, out)?;
                    } else if let (Ok(relative), Ok(meta)) =
                        (path.strip_prefix(root), entry.metadata())
                    {
                        // A temporary file left by a crashed write is listed as what it is: an
                        // object to age out. Its name is only valid once the `.tmp-` is stripped,
                        // so it is reported under the name it would have had.
                        let key = relative.to_string_lossy().replace('\\', "/");
                        let key = match key.rsplit_once("/.tmp-") {
                            Some((dir, file)) => format!("{dir}/{file}"),
                            None => key,
                        };
                        out.push(Listed {
                            key,
                            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                        });
                    }
                }
                Ok(())
            }
            let mut out = Vec::new();
            walk(&root, &root, &mut out).map_err(|e| classify(&e))?;
            Ok(out)
        })
        .await
        .map_err(|_| BlobError::Unavailable)?
    }

    fn max_object_bytes(&self) -> usize {
        self.max_object_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::blobstore::{get_sealed, put_sealed, sweep};
    use std::time::Duration;

    fn dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "llmshim-blob-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[tokio::test]
    async fn put_get_delete_and_a_missing_object() {
        let root = dir("basic");
        let store = FileStore::new(&root, 1024).unwrap();
        store.put("p/20260101/a", b"hello".to_vec()).await.unwrap();
        assert_eq!(store.get("p/20260101/a", 1024).await.unwrap(), b"hello");
        // Replacing is atomic and complete.
        store
            .put("p/20260101/a", b"hello again".to_vec())
            .await
            .unwrap();
        assert_eq!(
            store.get("p/20260101/a", 1024).await.unwrap(),
            b"hello again"
        );
        assert_eq!(store.get("p/20260101/a", 5).await, Err(BlobError::TooLarge));
        assert_eq!(
            store.get("p/20260101/b", 1024).await,
            Err(BlobError::NotFound)
        );
        store.delete("p/20260101/a").await.unwrap();
        store.delete("p/20260101/a").await.unwrap();
        assert_eq!(
            store.get("p/20260101/a", 1024).await,
            Err(BlobError::NotFound)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_name_cannot_leave_the_directory() {
        let root = dir("escape");
        let store = FileStore::new(&root, 1024).unwrap();
        let outside = root
            .parent()
            .unwrap()
            .join(format!("outside-{}", std::process::id()));
        for bad in ["../x", "a/../../x", "/etc/passwd", "a//b", "."] {
            assert_eq!(
                store.put(bad, b"x".to_vec()).await,
                Err(BlobError::Invalid),
                "{bad}"
            );
            assert_eq!(store.get(bad, 10).await, Err(BlobError::Invalid), "{bad}");
            assert_eq!(store.delete(bad).await, Err(BlobError::Invalid), "{bad}");
        }
        assert!(!outside.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn objects_are_owner_only_and_a_symlink_is_not_an_object() {
        use std::os::unix::fs::PermissionsExt;
        let root = dir("modes");
        let store = FileStore::new(&root, 1024).unwrap();
        store.put("p/20260101/a", b"x".to_vec()).await.unwrap();
        let mode = std::fs::metadata(root.canonicalize().unwrap().join("p/20260101/a"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        // A link planted in the store is not listed, so it is never swept or followed by the sweeper.
        let target = dir("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("precious"), b"keep").unwrap();
        std::os::unix::fs::symlink(&target, root.canonicalize().unwrap().join("link")).unwrap();
        let listed = store.list().await.unwrap();
        assert!(listed
            .iter()
            .all(|o| !o.key.contains("precious") && !o.key.starts_with("link")));
        assert!(sweep(&store, Duration::ZERO).await.is_ok());
        assert!(target.join("precious").exists());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(target);
    }

    #[tokio::test]
    async fn the_sweeper_removes_old_objects_and_the_leftovers_of_a_crashed_write() {
        let root = dir("sweep");
        let store = FileStore::new(&root, 1024).unwrap();
        let canonical = root.canonicalize().unwrap();
        store.put("p/20260101/old", b"x".to_vec()).await.unwrap();
        // A write that died between create and rename.
        std::fs::write(canonical.join("p/20260101/.tmp-orphan"), b"x").unwrap();
        // Aged by setting the modification time, not by waiting: no deadline for a loaded machine.
        let two_hours_ago = SystemTime::now() - Duration::from_secs(7200);
        for file in ["p/20260101/old", "p/20260101/.tmp-orphan"] {
            std::fs::OpenOptions::new()
                .write(true)
                .open(canonical.join(file))
                .unwrap()
                .set_modified(two_hours_ago)
                .unwrap();
        }
        store.put("p/20260101/new", b"x".to_vec()).await.unwrap();
        let removed = sweep(&store, Duration::from_secs(3600)).await.unwrap();
        assert_eq!(removed, 2);
        assert!(store.get("p/20260101/new", 10).await.is_ok());
        assert_eq!(
            store.get("p/20260101/old", 10).await,
            Err(BlobError::NotFound)
        );
        assert!(!canonical.join("p/20260101/.tmp-orphan").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn two_stores_on_one_directory_see_each_other_as_two_replicas_do() {
        let root = dir("shared");
        let (first, second) = (
            FileStore::new(&root, 1 << 20).unwrap(),
            FileStore::new(&root, 1 << 20).unwrap(),
        );
        let reference = put_sealed(&first, "media/", &"x".repeat(40_000))
            .await
            .unwrap();
        assert_eq!(get_sealed(&second, &reference).await.unwrap().len(), 40_000);
        second.delete(&reference.name).await.unwrap();
        assert_eq!(
            get_sealed(&first, &reference).await,
            Err(BlobError::NotFound)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
