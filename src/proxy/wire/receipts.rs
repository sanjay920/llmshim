//! Issued metadata for native clients that cannot retain canonical wire maps.
//! Records are private local files, scoped to the inbound credential digest.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_RECEIPT_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_MAX_ENTRIES: u64 = 100_000;
const DEFAULT_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_TTL_SECS: u64 = 30 * 24 * 60 * 60;
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONFIGURED_ENTRIES: u64 = 100_000;
const MAX_CONFIGURED_TOTAL_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const MAX_CONFIGURED_TTL_SECS: u64 = 10 * 365 * 24 * 60 * 60;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const JOURNAL_COMPACT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_JOURNAL_LINE_BYTES: usize = 1024;
const MANAGED_DIRECTORY: &str = ".retained-v1";
const LOCK_FILE: &str = ".receipt-retention-v1.lock";
const JOURNAL_FILE: &str = ".receipt-retention-v1.jsonl";
const JOURNAL_PENDING_FILE: &str = ".receipt-retention-v1.pending";
const DATA_PENDING_FILE: &str = ".receipt-write-v1.pending";
pub(super) const BUSY_ERROR_MESSAGE: &str = "native replay metadata is busy";

thread_local! {
    static HTTP_LOCK_TIMEOUT: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

pub(super) fn configured_http_lock_timeout() -> Duration {
    Duration::from_millis(configured_number(
        "LLMSHIM_REPLAY_RECEIPTS_LOCK_TIMEOUT_MS",
        DEFAULT_LOCK_TIMEOUT.as_millis() as u64,
        1,
        30_000,
    ))
}

pub(super) fn with_http_lock_timeout<T>(timeout: Duration, operation: impl FnOnce() -> T) -> T {
    HTTP_LOCK_TIMEOUT.with(|configured_timeout| {
        let previous = configured_timeout.replace(Some(timeout));
        struct RestoreTimeout<'a> {
            configured_timeout: &'a std::cell::Cell<Option<Duration>>,
            previous: Option<Duration>,
        }
        impl Drop for RestoreTimeout<'_> {
            fn drop(&mut self) {
                self.configured_timeout.set(self.previous);
            }
        }
        let _restore = RestoreTimeout {
            configured_timeout,
            previous,
        };
        operation()
    })
}

#[derive(Clone, Copy, Debug)]
struct Limits {
    max_entries: u64,
    max_total_bytes: u64,
    ttl_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            ttl_secs: DEFAULT_TTL_SECS,
        }
    }
}

impl Limits {
    fn from_env() -> Self {
        Self {
            max_entries: configured_number(
                "LLMSHIM_REPLAY_RECEIPTS_MAX_ENTRIES",
                DEFAULT_MAX_ENTRIES,
                1,
                MAX_CONFIGURED_ENTRIES,
            ),
            max_total_bytes: configured_number(
                "LLMSHIM_REPLAY_RECEIPTS_MAX_BYTES",
                DEFAULT_MAX_TOTAL_BYTES,
                MAX_RECEIPT_BYTES,
                MAX_CONFIGURED_TOTAL_BYTES,
            ),
            ttl_secs: configured_number(
                "LLMSHIM_REPLAY_RECEIPTS_TTL_SECS",
                DEFAULT_TTL_SECS,
                60,
                MAX_CONFIGURED_TTL_SECS,
            ),
        }
    }
}

#[derive(Clone, Debug)]
struct Entry {
    bytes: u64,
    issued_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PendingWrite {
    key: String,
    bytes: u64,
    issued_at: u64,
    content_digest: String,
    shadows_legacy: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct JournalHeader {
    version: u8,
    generation: String,
    legacy_count: u64,
    legacy_bytes: u64,
    legacy_complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum JournalRecord {
    Put {
        key: String,
        bytes: u64,
        issued_at: u64,
        shadows_legacy: bool,
    },
    Remove {
        key: String,
    },
    ShadowLegacy {
        key: String,
    },
    Baseline {
        legacy_count: u64,
        legacy_bytes: u64,
        legacy_complete: bool,
    },
    Prepare {
        pending: PendingWrite,
    },
    Abort,
}

#[derive(Debug, Default)]
struct CachedIndex {
    generation: String,
    journal_offset: u64,
    legacy_count: u64,
    legacy_bytes: u64,
    legacy_complete: bool,
    entries: HashMap<String, Entry>,
    oldest: BTreeSet<(u64, String)>,
    shadows: HashSet<String>,
    managed_bytes: u64,
    pending: Option<PendingWrite>,
}

#[derive(Debug)]
pub struct Receipts {
    root: PathBuf,
    limits: Limits,
    index: Mutex<CachedIndex>,
    rescan_incomplete_baseline: bool,
    rescan_attempted: AtomicBool,
    lock_timeout: Option<Duration>,
    #[cfg(test)]
    full_reloads: std::sync::atomic::AtomicUsize,
}

impl Receipts {
    pub fn new(root: PathBuf) -> Self {
        Self::with_limits(root, Limits::default())
    }

    fn with_limits(root: PathBuf, limits: Limits) -> Self {
        Self {
            root,
            limits,
            index: Mutex::new(CachedIndex::default()),
            rescan_incomplete_baseline: false,
            rescan_attempted: AtomicBool::new(false),
            lock_timeout: None,
            #[cfg(test)]
            full_reloads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn from_env() -> Self {
        let root = std::env::var_os("LLMSHIM_REPLAY_RECEIPTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::data_local_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("llmshim/replay-receipts")
            });
        let mut receipts = Self::with_limits(root, Limits::from_env());
        receipts.rescan_incomplete_baseline =
            std::env::var_os("LLMSHIM_REPLAY_RECEIPTS_RESCAN_INCOMPLETE_BASELINE")
                .is_some_and(|value| value == "1");
        receipts
    }

    fn key(&self, scope: &str, kind: &str, key: &Value) -> String {
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
        format!("{:x}", digest.finalize())
    }

    fn legacy_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.json"))
    }

    fn managed_root(&self) -> PathBuf {
        self.root.join(MANAGED_DIRECTORY)
    }

    fn managed_path(&self, key: &str) -> PathBuf {
        self.managed_root().join(format!("{key}.json"))
    }

    pub fn put(&self, scope: &str, kind: &str, key: &Value, value: &Value) -> Result<(), String> {
        let serialized_value = serde_json::to_vec(value).map_err(|_| error())?;
        if serialized_value.len() as u64 > MAX_RECEIPT_BYTES {
            return Err(error());
        }

        self.prepare_directories()?;
        let lock_file = self.lock_file()?;
        self.acquire_lock(&lock_file)?;
        let mut index = self.index.lock().map_err(|_| error())?;
        self.refresh_index(&mut index)?;
        self.reconcile_pending(&mut index)?;
        self.rescan_baseline_if_requested(&mut index)?;

        let receipt_key = self.key(scope, kind, key);
        let legacy_exists = if index.shadows.contains(&receipt_key) {
            true
        } else {
            match read_bounded(&self.legacy_path(&receipt_key), MAX_RECEIPT_BYTES) {
                Ok(legacy_bytes) => {
                    let legacy_value: Value =
                        serde_json::from_slice(&legacy_bytes).map_err(|_| error())?;
                    if !index.entries.contains_key(&receipt_key) && legacy_value == *value {
                        return Ok(());
                    }
                    true
                }
                Err(ReadError::NotFound) => false,
                Err(ReadError::Other) => return Err(error()),
            }
        };

        if !index.legacy_complete {
            return Err(error());
        }
        if legacy_exists
            && !index.shadows.contains(&receipt_key)
            && index.shadows.len() as u64 >= self.limits.max_entries
        {
            return Err(error());
        }

        let issued_at = unix_time_secs()?;
        self.expire_entries(&mut index, issued_at)?;
        self.evict_for_capacity(&mut index, &receipt_key, serialized_value.len() as u64)?;

        let pending = PendingWrite {
            key: receipt_key.clone(),
            bytes: serialized_value.len() as u64,
            issued_at,
            content_digest: format!("{:x}", Sha256::digest(&serialized_value)),
            shadows_legacy: legacy_exists,
        };
        self.append_record(
            &mut index,
            &JournalRecord::Prepare {
                pending: pending.clone(),
            },
        )?;

        let pending_path = self.managed_root().join(DATA_PENDING_FILE);
        write_private_file(&pending_path, &serialized_value)?;
        sync_directory(&self.managed_root())?;
        persist_fixed_file(&pending_path, &self.managed_path(&receipt_key))?;
        sync_directory(&self.managed_root())?;
        self.append_record(
            &mut index,
            &JournalRecord::Put {
                key: receipt_key,
                bytes: serialized_value.len() as u64,
                issued_at,
                shadows_legacy: legacy_exists,
            },
        )?;
        self.compact_if_needed(&mut index)?;
        Ok(())
    }

    pub fn get(&self, scope: &str, kind: &str, key: &Value) -> Result<Option<Value>, String> {
        self.prepare_directories()?;
        let lock_file = self.lock_file()?;
        self.acquire_lock(&lock_file)?;
        let mut index = self.index.lock().map_err(|_| error())?;
        self.refresh_index(&mut index)?;
        self.reconcile_pending(&mut index)?;
        self.rescan_baseline_if_requested(&mut index)?;

        let receipt_key = self.key(scope, kind, key);
        if let Some(entry) = index.entries.get(&receipt_key).cloned() {
            if is_expired(entry.issued_at, unix_time_secs()?, self.limits.ttl_secs) {
                self.remove_entry(&mut index, &receipt_key)?;
                self.compact_if_needed(&mut index)?;
                return Ok(None);
            }
            return parse_receipt(&self.managed_path(&receipt_key));
        }
        if index.shadows.contains(&receipt_key) {
            return Ok(None);
        }
        parse_receipt(&self.legacy_path(&receipt_key))
    }

    fn prepare_directories(&self) -> Result<(), String> {
        create_private_directory(&self.root)?;
        create_private_directory(&self.managed_root())
    }

    fn lock_file(&self) -> Result<File, String> {
        open_private_append(&self.root.join(LOCK_FILE))
    }

    fn acquire_lock(&self, lock_file: &File) -> Result<(), String> {
        let lock_timeout = self
            .lock_timeout
            .or_else(|| HTTP_LOCK_TIMEOUT.with(std::cell::Cell::get));
        let Some(lock_timeout) = lock_timeout else {
            return lock_file.lock_exclusive().map_err(|_| error());
        };
        let started_at = Instant::now();
        loop {
            match lock_file.try_lock_exclusive() {
                Ok(()) => return Ok(()),
                Err(io_error)
                    if io_error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
                        && started_at.elapsed() < lock_timeout =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(io_error)
                    if io_error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    return Err(BUSY_ERROR_MESSAGE.into())
                }
                Err(_) => return Err(error()),
            }
        }
    }

    fn journal_path(&self) -> PathBuf {
        self.root.join(JOURNAL_FILE)
    }

    fn refresh_index(&self, index: &mut CachedIndex) -> Result<(), String> {
        self.ensure_journal()?;
        let journal_path = self.journal_path();
        let metadata = std::fs::metadata(&journal_path).map_err(|_| error())?;
        if metadata.len() > MAX_JOURNAL_BYTES {
            return Err(error());
        }

        let mut journal = File::open(&journal_path).map_err(|_| error())?;
        let (header, header_end) = read_header(&mut journal)?;
        if header.version != 1 {
            return Err(error());
        }
        if index.generation != header.generation
            || index.journal_offset < header_end
            || index.journal_offset > metadata.len()
        {
            #[cfg(test)]
            self.full_reloads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            *index = CachedIndex {
                generation: header.generation,
                journal_offset: header_end,
                legacy_count: header.legacy_count,
                legacy_bytes: header.legacy_bytes,
                legacy_complete: header.legacy_complete,
                ..CachedIndex::default()
            };
        }

        journal
            .seek(SeekFrom::Start(index.journal_offset))
            .map_err(|_| error())?;
        let mut reader = BufReader::new(journal);
        let mut record_bytes = Vec::new();
        loop {
            record_bytes.clear();
            let bytes_read = reader
                .read_until(b'\n', &mut record_bytes)
                .map_err(|_| error())?;
            if bytes_read == 0 {
                break;
            }
            if record_bytes.len() > MAX_JOURNAL_LINE_BYTES || !record_bytes.ends_with(b"\n") {
                return Err(error());
            }
            let record = serde_json::from_slice(&record_bytes).map_err(|_| error())?;
            apply_record(index, record)?;
            index.journal_offset = index
                .journal_offset
                .checked_add(bytes_read as u64)
                .ok_or_else(error)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn full_reload_count(&self) -> usize {
        self.full_reloads.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn set_lock_timeout(&mut self, timeout: Duration) {
        self.lock_timeout = Some(timeout);
    }

    fn ensure_journal(&self) -> Result<(), String> {
        let path = self.journal_path();
        if path.exists() {
            return Ok(());
        }

        let baseline = scan_legacy_footprint(&self.root, self.limits)?;
        let pending_path = self.root.join(JOURNAL_PENDING_FILE);
        let mut pending_journal = open_private_truncate(&pending_path)?;
        let header = JournalHeader {
            version: 1,
            generation: new_generation(),
            legacy_count: baseline.count,
            legacy_bytes: baseline.bytes,
            legacy_complete: baseline.complete,
        };
        write_json_line(&mut pending_journal, &header)?;
        pending_journal.sync_all().map_err(|_| error())?;
        drop(pending_journal);
        persist_fixed_file(&pending_path, &path)?;
        sync_directory(&self.root)
    }

    fn rescan_baseline_if_requested(&self, index: &mut CachedIndex) -> Result<(), String> {
        if index.legacy_complete
            || !self.rescan_incomplete_baseline
            || self.rescan_attempted.swap(true, Ordering::SeqCst)
        {
            return Ok(());
        }
        let baseline = scan_legacy_footprint(&self.root, self.limits)?;
        self.append_record(
            index,
            &JournalRecord::Baseline {
                legacy_count: baseline.count,
                legacy_bytes: baseline.bytes,
                legacy_complete: baseline.complete,
            },
        )
    }

    fn reconcile_pending(&self, index: &mut CachedIndex) -> Result<(), String> {
        let Some(pending) = index.pending.clone() else {
            return Ok(());
        };
        let pending_path = self.managed_root().join(DATA_PENDING_FILE);
        match std::fs::remove_file(&pending_path) {
            Ok(()) => {
                sync_directory(&self.managed_root())?;
                return self.append_record(index, &JournalRecord::Abort);
            }
            Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(error()),
        }

        let target_matches = match read_bounded(&self.managed_path(&pending.key), MAX_RECEIPT_BYTES)
        {
            Ok(bytes) => {
                bytes.len() as u64 == pending.bytes
                    && format!("{:x}", Sha256::digest(&bytes)) == pending.content_digest
            }
            Err(ReadError::NotFound) => false,
            Err(ReadError::Other) => return Err(error()),
        };
        if target_matches {
            self.append_record(
                index,
                &JournalRecord::Put {
                    key: pending.key,
                    bytes: pending.bytes,
                    issued_at: pending.issued_at,
                    shadows_legacy: pending.shadows_legacy,
                },
            )
        } else {
            self.append_record(index, &JournalRecord::Abort)
        }
    }

    fn expire_entries(&self, index: &mut CachedIndex, now: u64) -> Result<(), String> {
        let expired_keys: Vec<String> = index
            .oldest
            .iter()
            .take_while(|(issued_at, _)| is_expired(*issued_at, now, self.limits.ttl_secs))
            .map(|(_, key)| key.clone())
            .collect();
        for key in expired_keys {
            self.remove_entry(index, &key)?;
        }
        Ok(())
    }

    fn evict_for_capacity(
        &self,
        index: &mut CachedIndex,
        replacement_key: &str,
        new_bytes: u64,
    ) -> Result<(), String> {
        if !index.legacy_complete {
            return Err(error());
        }
        loop {
            let replaced_bytes = index
                .entries
                .get(replacement_key)
                .map_or(0, |entry| entry.bytes);
            let resulting_count = index
                .legacy_count
                .checked_add(index.entries.len() as u64)
                .and_then(|count| {
                    count.checked_add(u64::from(!index.entries.contains_key(replacement_key)))
                })
                .ok_or_else(error)?;
            let resulting_bytes = index
                .legacy_bytes
                .checked_add(index.managed_bytes)
                .and_then(|bytes| bytes.checked_sub(replaced_bytes))
                .and_then(|bytes| bytes.checked_add(new_bytes))
                .ok_or_else(error)?;
            if resulting_count <= self.limits.max_entries
                && resulting_bytes <= self.limits.max_total_bytes
            {
                return Ok(());
            }

            let eviction_key = index
                .oldest
                .iter()
                .find(|(_, key)| key != replacement_key)
                .map(|(_, key)| key.clone())
                .ok_or_else(error)?;
            self.remove_entry(index, &eviction_key)?;
        }
    }

    fn remove_entry(&self, index: &mut CachedIndex, key: &str) -> Result<(), String> {
        match std::fs::remove_file(self.managed_path(key)) {
            Ok(()) => sync_directory(&self.managed_root())?,
            Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(error()),
        }
        self.append_record(
            index,
            &JournalRecord::Remove {
                key: key.to_owned(),
            },
        )
    }

    fn append_record(&self, index: &mut CachedIndex, record: &JournalRecord) -> Result<(), String> {
        let mut serialized_record = serde_json::to_vec(record).map_err(|_| error())?;
        serialized_record.push(b'\n');
        if serialized_record.len() > MAX_JOURNAL_LINE_BYTES {
            return Err(error());
        }
        let mut journal = open_private_append(&self.journal_path())?;
        let current_bytes = journal.metadata().map_err(|_| error())?.len();
        if current_bytes
            .checked_add(serialized_record.len() as u64)
            .filter(|bytes| *bytes <= MAX_JOURNAL_BYTES)
            .is_none()
        {
            return Err(error());
        }
        journal.write_all(&serialized_record).map_err(|_| error())?;
        journal.sync_all().map_err(|_| error())?;
        apply_record(index, record.clone())?;
        index.journal_offset = current_bytes + serialized_record.len() as u64;
        Ok(())
    }

    fn compact_if_needed(&self, index: &mut CachedIndex) -> Result<(), String> {
        if index.pending.is_some() || index.journal_offset < JOURNAL_COMPACT_BYTES {
            return Ok(());
        }
        let pending_path = self.root.join(JOURNAL_PENDING_FILE);
        let mut pending_journal = open_private_truncate(&pending_path)?;
        let header = JournalHeader {
            version: 1,
            generation: new_generation(),
            legacy_count: index.legacy_count,
            legacy_bytes: index.legacy_bytes,
            legacy_complete: index.legacy_complete,
        };
        write_json_line(&mut pending_journal, &header)?;
        for key in &index.shadows {
            write_json_line(
                &mut pending_journal,
                &JournalRecord::ShadowLegacy { key: key.clone() },
            )?;
        }
        for (issued_at, key) in &index.oldest {
            let entry = index.entries.get(key).ok_or_else(error)?;
            write_json_line(
                &mut pending_journal,
                &JournalRecord::Put {
                    key: key.clone(),
                    bytes: entry.bytes,
                    issued_at: *issued_at,
                    shadows_legacy: false,
                },
            )?;
        }
        if pending_journal.metadata().map_err(|_| error())?.len() > MAX_JOURNAL_BYTES {
            return Err(error());
        }
        pending_journal.sync_all().map_err(|_| error())?;
        drop(pending_journal);
        persist_fixed_file(&pending_path, &self.journal_path())?;
        sync_directory(&self.root)?;
        index.generation = header.generation;
        index.journal_offset = std::fs::metadata(self.journal_path())
            .map_err(|_| error())?
            .len();
        Ok(())
    }
}

fn apply_record(index: &mut CachedIndex, record: JournalRecord) -> Result<(), String> {
    match record {
        JournalRecord::Put {
            key,
            bytes,
            issued_at,
            shadows_legacy,
        } => {
            if !is_receipt_key(&key) || bytes > MAX_RECEIPT_BYTES {
                return Err(error());
            }
            if !index.entries.contains_key(&key)
                && index.entries.len() as u64 >= MAX_CONFIGURED_ENTRIES
            {
                return Err(error());
            }
            if shadows_legacy
                && !index.shadows.contains(&key)
                && index.shadows.len() as u64 >= MAX_CONFIGURED_ENTRIES
            {
                return Err(error());
            }
            if let Some(previous) = index
                .entries
                .insert(key.clone(), Entry { bytes, issued_at })
            {
                index.managed_bytes = index
                    .managed_bytes
                    .checked_sub(previous.bytes)
                    .ok_or_else(error)?;
                index.oldest.remove(&(previous.issued_at, key.clone()));
            }
            index.managed_bytes = index.managed_bytes.checked_add(bytes).ok_or_else(error)?;
            index.oldest.insert((issued_at, key.clone()));
            if shadows_legacy {
                index.shadows.insert(key);
            }
            index.pending = None;
        }
        JournalRecord::Remove { key } => {
            if !is_receipt_key(&key) {
                return Err(error());
            }
            if let Some(previous) = index.entries.remove(&key) {
                index.managed_bytes = index
                    .managed_bytes
                    .checked_sub(previous.bytes)
                    .ok_or_else(error)?;
                index.oldest.remove(&(previous.issued_at, key));
            }
        }
        JournalRecord::ShadowLegacy { key } => {
            if !is_receipt_key(&key) {
                return Err(error());
            }
            if !index.shadows.contains(&key) && index.shadows.len() as u64 >= MAX_CONFIGURED_ENTRIES
            {
                return Err(error());
            }
            index.shadows.insert(key);
        }
        JournalRecord::Baseline {
            legacy_count,
            legacy_bytes,
            legacy_complete,
        } => {
            if legacy_complete {
                index.legacy_count = legacy_count;
                index.legacy_bytes = legacy_bytes;
            } else {
                index.legacy_count = index.legacy_count.max(legacy_count);
                index.legacy_bytes = index.legacy_bytes.max(legacy_bytes);
            }
            index.legacy_complete = legacy_complete;
        }
        JournalRecord::Prepare { pending } => {
            if !is_receipt_key(&pending.key)
                || !is_receipt_key(&pending.content_digest)
                || pending.bytes > MAX_RECEIPT_BYTES
                || index.pending.is_some()
            {
                return Err(error());
            }
            index.pending = Some(pending);
        }
        JournalRecord::Abort => index.pending = None,
    }
    Ok(())
}

struct LegacyFootprint {
    count: u64,
    bytes: u64,
    complete: bool,
}

fn scan_legacy_footprint(root: &Path, limits: Limits) -> Result<LegacyFootprint, String> {
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    let mut inspected_entries = 0_u64;
    for directory_entry in std::fs::read_dir(root).map_err(|_| error())? {
        let directory_entry = directory_entry.map_err(|_| error())?;
        inspected_entries = inspected_entries.checked_add(1).ok_or_else(error)?;
        if inspected_entries > limits.max_entries.saturating_add(16) {
            return Ok(LegacyFootprint {
                count,
                bytes,
                complete: false,
            });
        }
        let file_name = directory_entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !is_legacy_receipt_name(file_name) {
            continue;
        }
        let metadata = directory_entry.metadata().map_err(|_| error())?;
        if !metadata.is_file() {
            continue;
        }
        count = count.checked_add(1).ok_or_else(error)?;
        bytes = bytes.checked_add(metadata.len()).ok_or_else(error)?;
        if count > limits.max_entries || bytes > limits.max_total_bytes {
            return Ok(LegacyFootprint {
                count,
                bytes,
                complete: false,
            });
        }
    }
    Ok(LegacyFootprint {
        count,
        bytes,
        complete: true,
    })
}

fn is_legacy_receipt_name(file_name: &str) -> bool {
    file_name.strip_suffix(".json").is_some_and(is_receipt_key)
}

fn is_receipt_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn configured_number(name: &str, default: u64, minimum: u64, maximum: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value >= minimum && *value <= maximum)
        .unwrap_or(default)
}

fn read_header(journal: &mut File) -> Result<(JournalHeader, u64), String> {
    journal.seek(SeekFrom::Start(0)).map_err(|_| error())?;
    let mut reader = BufReader::new(journal);
    let mut header_bytes = Vec::new();
    let bytes_read = reader
        .by_ref()
        .take((MAX_JOURNAL_LINE_BYTES + 1) as u64)
        .read_until(b'\n', &mut header_bytes)
        .map_err(|_| error())?;
    if bytes_read == 0 || bytes_read > MAX_JOURNAL_LINE_BYTES || !header_bytes.ends_with(b"\n") {
        return Err(error());
    }
    let header = serde_json::from_slice(&header_bytes).map_err(|_| error())?;
    Ok((header, bytes_read as u64))
}

fn write_json_line(mut writer: impl Write, value: &impl Serialize) -> Result<(), String> {
    serde_json::to_writer(&mut writer, value).map_err(|_| error())?;
    writer.write_all(b"\n").map_err(|_| error())
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|_| error())
}

fn open_private_append(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.create(true).append(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|_| error())?;
    set_private_permissions(&file)?;
    Ok(file)
}

fn open_private_truncate(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|_| error())?;
    set_private_permissions(&file)?;
    Ok(file)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = open_private_truncate(path)?;
    file.write_all(bytes).map_err(|_| error())?;
    file.sync_all().map_err(|_| error())
}

fn persist_fixed_file(pending_path: &Path, destination_path: &Path) -> Result<(), String> {
    let pending_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(pending_path)
        .map_err(|_| error())?;
    let temporary_path =
        tempfile::TempPath::try_from_path(pending_path.to_owned()).map_err(|_| error())?;
    tempfile::NamedTempFile::from_parts(pending_file, temporary_path)
        .persist(destination_path)
        .map(|_| ())
        .map_err(|_| error())
}

#[cfg(unix)]
fn set_private_permissions(file: &File) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|_| error())
}

#[cfg(not(unix))]
fn set_private_permissions(_file: &File) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| error())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

enum ReadError {
    NotFound,
    Other,
}

fn read_bounded(path: &Path, maximum_bytes: u64) -> Result<Vec<u8>, ReadError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ReadError::NotFound)
        }
        Err(_) => return Err(ReadError::Other),
    };
    let mut bytes = Vec::new();
    file.take(maximum_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadError::Other)?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(ReadError::Other);
    }
    Ok(bytes)
}

fn parse_receipt(path: &Path) -> Result<Option<Value>, String> {
    match read_bounded(path, MAX_RECEIPT_BYTES) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| error()),
        Err(ReadError::NotFound) => Ok(None),
        Err(ReadError::Other) => Err(error()),
    }
}

fn unix_time_secs() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| error())
}

fn is_expired(issued_at: u64, now: u64, ttl_secs: u64) -> bool {
    now.saturating_sub(issued_at) >= ttl_secs
}

fn new_generation() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn error() -> String {
    "native replay metadata is unavailable".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{process::Command, sync::Arc, thread};

    fn limits(max_entries: u64, max_total_bytes: u64, ttl_secs: u64) -> Limits {
        Limits {
            max_entries,
            max_total_bytes,
            ttl_secs,
        }
    }

    fn managed_file_count(root: &Path) -> usize {
        std::fs::read_dir(root.join(MANAGED_DIRECTORY))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(is_legacy_receipt_name)
            })
            .count()
    }

    #[test]
    fn count_and_byte_budgets_evict_oldest_and_survive_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let first = json!({"data":"11111111"});
        let second = json!({"data":"22222222"});
        let serialized_bytes = serde_json::to_vec(&first).unwrap().len() as u64;
        let store = Receipts::with_limits(
            directory.path().to_owned(),
            limits(2, serialized_bytes * 2, 3600),
        );

        store.put("scope", "call", &json!("one"), &first).unwrap();
        store.put("scope", "call", &json!("two"), &second).unwrap();
        store
            .put(
                "scope",
                "call",
                &json!("three"),
                &json!({"data":"33333333"}),
            )
            .unwrap();

        let reopened = Receipts::with_limits(
            directory.path().to_owned(),
            limits(2, serialized_bytes * 2, 3600),
        );
        let retained = ["one", "two", "three"]
            .into_iter()
            .filter(|key| {
                reopened
                    .get("scope", "call", &json!(key))
                    .unwrap()
                    .is_some()
            })
            .count();
        assert_eq!(retained, 2);
        assert_eq!(managed_file_count(directory.path()), 2);
    }

    #[test]
    fn legacy_replacement_never_resurfaces_after_managed_expiry() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            Receipts::with_limits(directory.path().to_owned(), limits(10, 1024 * 1024, 3600));
        store.prepare_directories().unwrap();
        let receipt_key = store.key("scope", "call", &json!("same"));
        write_private_file(&store.legacy_path(&receipt_key), br#"{"version":"old"}"#).unwrap();

        store
            .put("scope", "call", &json!("same"), &json!({"version":"new"}))
            .unwrap();
        assert_eq!(
            store.get("scope", "call", &json!("same")).unwrap(),
            Some(json!({"version":"new"}))
        );
        assert!(store.legacy_path(&receipt_key).exists());

        let reopened =
            Receipts::with_limits(directory.path().to_owned(), limits(10, 1024 * 1024, 0));
        assert_eq!(reopened.get("scope", "call", &json!("same")).unwrap(), None);
    }

    #[test]
    fn aggregate_byte_budget_is_enforced_independently_of_count() {
        let directory = tempfile::tempdir().unwrap();
        let value = json!({"data":"1234567890"});
        let serialized_bytes = serde_json::to_vec(&value).unwrap().len() as u64;
        let store = Receipts::with_limits(
            directory.path().to_owned(),
            limits(10, serialized_bytes * 2, 3600),
        );
        for ordinal in 0..3 {
            store.put("scope", "call", &json!(ordinal), &value).unwrap();
        }
        assert_eq!(managed_file_count(directory.path()), 2);
    }

    #[test]
    fn oversized_legacy_baseline_is_frozen_without_deleting_data() {
        let directory = tempfile::tempdir().unwrap();
        let store = Receipts::with_limits(directory.path().to_owned(), limits(1, 1024, 3600));
        store.prepare_directories().unwrap();
        for ordinal in 0..2 {
            let receipt_key = store.key("scope", "call", &json!(ordinal));
            write_private_file(&store.legacy_path(&receipt_key), b"null").unwrap();
        }

        assert!(store
            .put("scope", "call", &json!("new"), &json!({"value":1}))
            .is_err());
        assert_eq!(
            store.get("scope", "call", &json!(0)).unwrap(),
            Some(Value::Null)
        );
        assert_eq!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_str()
                    .is_some_and(is_legacy_receipt_name))
                .count(),
            2
        );
    }

    #[test]
    fn concurrent_instances_share_one_finite_index() {
        let directory = tempfile::tempdir().unwrap();
        let root = Arc::new(directory.path().to_owned());
        let mut threads = Vec::new();
        for worker in 0..8 {
            let root = root.clone();
            threads.push(thread::spawn(move || {
                let store = Receipts::with_limits((*root).clone(), limits(20, 1024 * 1024, 3600));
                for ordinal in 0..20 {
                    let key = json!(format!("{worker}-{ordinal}"));
                    store
                        .put(
                            "scope",
                            "call",
                            &key,
                            &json!({"worker":worker,"ordinal":ordinal}),
                        )
                        .unwrap();
                }
            }));
        }
        for worker in threads {
            worker.join().unwrap();
        }
        assert!(managed_file_count(directory.path()) <= 20);
        let reopened =
            Receipts::with_limits(directory.path().to_owned(), limits(20, 1024 * 1024, 3600));
        reopened.get("scope", "call", &json!("7-19")).unwrap();
    }

    #[test]
    fn receipt_subprocess_helper() {
        let Some(root) = std::env::var_os("LLMSHIM_TEST_RECEIPT_PROCESS_ROOT") else {
            return;
        };
        let ordinal = std::env::var("LLMSHIM_TEST_RECEIPT_PROCESS_ORDINAL").unwrap();
        let store = Receipts::with_limits(PathBuf::from(root), limits(12, 1024 * 1024, 3600));
        store
            .put(
                "scope",
                "call",
                &json!(ordinal),
                &json!({"process":ordinal}),
            )
            .unwrap();
    }

    #[test]
    fn subprocesses_coordinate_through_the_shared_lock() {
        let directory = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for ordinal in 0..24 {
            children.push(
                Command::new(&executable)
                    .args([
                        "--exact",
                        "proxy::wire::receipts::tests::receipt_subprocess_helper",
                        "--nocapture",
                    ])
                    .env("LLMSHIM_TEST_RECEIPT_PROCESS_ROOT", directory.path())
                    .env("LLMSHIM_TEST_RECEIPT_PROCESS_ORDINAL", ordinal.to_string())
                    .spawn()
                    .unwrap(),
            );
        }
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        assert!(managed_file_count(directory.path()) <= 12);
        let reopened =
            Receipts::with_limits(directory.path().to_owned(), limits(12, 1024 * 1024, 3600));
        reopened.get("scope", "call", &json!("23")).unwrap();
    }

    #[test]
    fn crash_stages_reconcile_without_unaccounted_growth() {
        for stage in ["prepared", "pending", "renamed"] {
            let directory = tempfile::tempdir().unwrap();
            let store =
                Receipts::with_limits(directory.path().to_owned(), limits(2, 1024 * 1024, 3600));
            store.prepare_directories().unwrap();
            let lock_file = store.lock_file().unwrap();
            lock_file.lock_exclusive().unwrap();
            let mut index = store.index.lock().unwrap();
            store.refresh_index(&mut index).unwrap();
            let value = json!({"stage":stage});
            let bytes = serde_json::to_vec(&value).unwrap();
            let receipt_key = store.key("scope", "call", &json!(stage));
            let pending = PendingWrite {
                key: receipt_key.clone(),
                bytes: bytes.len() as u64,
                issued_at: unix_time_secs().unwrap(),
                content_digest: format!("{:x}", Sha256::digest(&bytes)),
                shadows_legacy: false,
            };
            store
                .append_record(&mut index, &JournalRecord::Prepare { pending })
                .unwrap();
            if stage == "pending" {
                write_private_file(&store.managed_root().join(DATA_PENDING_FILE), &bytes).unwrap();
            } else if stage == "renamed" {
                write_private_file(&store.managed_path(&receipt_key), &bytes).unwrap();
            }
            drop(index);
            drop(lock_file);
            drop(store);

            let reopened =
                Receipts::with_limits(directory.path().to_owned(), limits(2, 1024 * 1024, 3600));
            let actual = reopened.get("scope", "call", &json!(stage)).unwrap();
            assert_eq!(actual, (stage == "renamed").then_some(value));
            reopened
                .put("scope", "call", &json!("after"), &json!({"ok":true}))
                .unwrap();
            assert!(managed_file_count(directory.path()) <= 2);
        }
    }

    #[test]
    fn corrupt_or_oversized_receipts_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let store = Receipts::with_limits(
            directory.path().to_owned(),
            limits(10, 32 * 1024 * 1024, 3600),
        );
        store
            .put("scope", "call", &json!("valid"), &json!({"ok":true}))
            .unwrap();
        std::fs::write(store.journal_path(), b"truncated").unwrap();
        let reopened = Receipts::with_limits(
            directory.path().to_owned(),
            limits(10, 32 * 1024 * 1024, 3600),
        );
        assert!(reopened.get("scope", "call", &json!("valid")).is_err());

        let oversized_directory = tempfile::tempdir().unwrap();
        let oversized = Receipts::new(oversized_directory.path().to_owned());
        oversized.prepare_directories().unwrap();
        let receipt_key = oversized.key("scope", "call", &json!("large"));
        let file = File::create(oversized.legacy_path(&receipt_key)).unwrap();
        file.set_len(MAX_RECEIPT_BYTES + 1).unwrap();
        assert!(oversized.get("scope", "call", &json!("large")).is_err());
    }
}
