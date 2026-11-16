//! A destination for JSON lines: `"stdout"` or an absolute file path.
//!
//! Callers never wait for the destination. A line is queued to a thread of its
//! own that does the writing, so a stalled pipe or a slow disk cannot hold a
//! request (or a concurrency permit, or a lock inside the gateway) while it
//! drains. The queue is bounded: when the writer cannot keep up, new lines are
//! dropped and counted instead of growing memory without limit, and the first
//! drop is reported on stderr. A write that fails (a full disk, a closed pipe)
//! is counted the same way and never fails a request.
//!
//! [`JsonLines::close`] drains the queue and stops the thread, so lines queued
//! at shutdown are not lost.

use serde::Serialize;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Lines that may wait for the writer before new ones are dropped.
const QUEUE_LINES: usize = 8192;
/// Lines dropped because the writer fell behind, by `log`.
pub(crate) const LINES_DROPPED: &str = "llmshim_log_lines_dropped_total";
/// Lines the destination refused (a full disk, a closed pipe), by `log`.
pub(crate) const WRITE_FAILURES: &str = "llmshim_log_write_failures_total";

enum Message {
    Line(String),
    /// Acknowledged once every line queued before it has been written.
    #[cfg_attr(not(test), allow(dead_code))]
    Barrier(SyncSender<()>),
}

struct Inner {
    name: String,
    /// `"access"` or `"usage"`: the `log` label of the `/metrics` series.
    log: &'static str,
    sender: Mutex<Option<SyncSender<Message>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    dropped: AtomicU64,
    failed: Arc<AtomicU64>,
}

impl Inner {
    /// Stop accepting lines, write what is queued and stop the thread.
    fn shutdown(&self) {
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
        let worker = self.worker.lock().ok().and_then(|mut worker| worker.take());
        if let Some(worker) = worker {
            let _ = worker.join();
            let (dropped, failed) = (
                self.dropped.load(Ordering::Relaxed),
                self.failed.load(Ordering::Relaxed),
            );
            if dropped > 0 || failed > 0 {
                eprintln!(
                    "{}: {dropped} line(s) dropped because the writer fell behind, \
                     {failed} line(s) could not be written",
                    self.name
                );
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Appends one JSON object per line. Cloning shares the destination.
#[derive(Clone)]
pub(crate) struct JsonLines {
    inner: Arc<Inner>,
}

impl JsonLines {
    /// `"stdout"`, or an absolute file path created owner-only (0600) and
    /// appended to. An existing file keeps its mode. `log` names which log this
    /// is (`"access"` or `"usage"`) in the `/metrics` series its drops and write
    /// failures feed.
    pub(crate) fn open(target: &str, log: &'static str) -> Result<Self, String> {
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
        Ok(Self::spawn(target.to_owned(), log, writer, QUEUE_LINES))
    }

    #[cfg(test)]
    pub(crate) fn to_writer(writer: Box<dyn Write + Send>) -> Self {
        Self::spawn("log".to_owned(), "test", writer, QUEUE_LINES)
    }

    #[cfg(test)]
    fn to_writer_with_capacity(writer: Box<dyn Write + Send>, capacity: usize) -> Self {
        Self::spawn("log".to_owned(), "test", writer, capacity)
    }

    fn spawn(
        name: String,
        log: &'static str,
        mut writer: Box<dyn Write + Send>,
        capacity: usize,
    ) -> Self {
        let (sender, receiver) = sync_channel::<Message>(capacity);
        let failed = Arc::new(AtomicU64::new(0));
        let (worker_failed, worker_name) = (failed.clone(), name.clone());
        let worker = std::thread::Builder::new()
            .name("llmshim-log".to_owned())
            .spawn(move || {
                for message in receiver {
                    match message {
                        Message::Line(line) => {
                            // One `write_all` of the whole line, so a failure
                            // cannot leave half a line that the next is glued to.
                            let written = writer
                                .write_all(line.as_bytes())
                                .and_then(|()| writer.flush());
                            if written.is_err() {
                                crate::gateway::metrics::incr(WRITE_FAILURES, &[("log", log)]);
                                if worker_failed.fetch_add(1, Ordering::Relaxed) == 0 {
                                    eprintln!(
                                        "warning: {worker_name}: cannot write log lines; \
                                         further failures are counted, not printed"
                                    );
                                }
                            }
                        }
                        Message::Barrier(acknowledge) => {
                            let _ = acknowledge.send(());
                        }
                    }
                }
            });
        // A thread that cannot start leaves the log unable to write: lines are
        // dropped (and counted) rather than the server refusing to run.
        let (sender, worker) = match worker {
            Ok(worker) => (Some(sender), Some(worker)),
            Err(error) => {
                eprintln!("warning: {name}: cannot start the log writer: {error}");
                (None, None)
            }
        };
        Self {
            inner: Arc::new(Inner {
                name,
                log,
                sender: Mutex::new(sender),
                worker: Mutex::new(worker),
                dropped: AtomicU64::new(0),
                failed,
            }),
        }
    }

    /// Queue one record. Never blocks and never fails the caller.
    pub(crate) fn write<T: Serialize>(&self, record: &T) {
        let Ok(mut line) = serde_json::to_string(record) else {
            return;
        };
        line.push('\n');
        let Ok(sender) = self.inner.sender.lock() else {
            return;
        };
        let Some(sender) = sender.as_ref() else {
            return;
        };
        match sender.try_send(Message::Line(line)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                crate::gateway::metrics::incr(LINES_DROPPED, &[("log", self.inner.log)]);
                if self.inner.dropped.fetch_add(1, Ordering::Relaxed) == 0 {
                    eprintln!(
                        "warning: {}: the log writer is not keeping up; dropping lines \
                         (the count is reported at shutdown)",
                        self.inner.name
                    );
                }
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Wait until every line queued so far has been written (or five seconds
    /// pass). For tests and for a caller that must read the destination back.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn sync(&self) {
        let sender = self.inner.sender.lock().ok().and_then(|s| s.clone());
        if let Some(sender) = sender {
            let (acknowledge, wait) = sync_channel(1);
            if sender.send(Message::Barrier(acknowledge)).is_ok() {
                let _ = wait.recv_timeout(Duration::from_secs(5));
            }
        }
    }

    /// Write what is queued and stop the writer. Lines written afterwards are
    /// ignored. Call at shutdown.
    pub(crate) fn close(&self) {
        self.inner.shutdown();
    }

    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn failed(&self) -> u64 {
        self.inner.failed.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;
    use std::time::Instant;

    /// A writer the test can read back.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn a_file_target_is_created_owner_only_and_appended_to() {
        let dir = std::env::temp_dir().join(format!("llmshim-jsonl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.jsonl");
        let target = path.to_str().unwrap();
        for index in 0..2 {
            let log = JsonLines::open(target, "test").unwrap();
            log.write(&serde_json::json!({"n": index}));
            log.close();
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
        assert!(JsonLines::open(dir.join("missing/dir/log").to_str().unwrap(), "test").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lines_arrive_whole_and_in_order() {
        let out = Shared::default();
        let log = JsonLines::to_writer(Box::new(out.clone()));
        for n in 0..500 {
            log.write(&serde_json::json!({"n": n}));
        }
        log.sync();
        let text = out.text();
        assert!(text.ends_with('\n'));
        let numbers: Vec<u64> = text
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["n"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(numbers, (0..500).collect::<Vec<_>>());
    }

    #[test]
    fn closing_writes_what_is_queued_and_later_lines_are_ignored() {
        let out = Shared::default();
        let log = JsonLines::to_writer(Box::new(out.clone()));
        for n in 0..100 {
            log.write(&serde_json::json!({"n": n}));
        }
        log.close();
        assert_eq!(out.text().lines().count(), 100);
        log.write(&serde_json::json!({"late": true}));
        log.close();
        assert_eq!(out.text().lines().count(), 100, "nothing after close");
    }

    /// A writer that blocks until the test opens the gate.
    struct Gated {
        open: Arc<(Mutex<bool>, Condvar)>,
        out: Shared,
    }

    impl Write for Gated {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            let (lock, condition) = &*self.open;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = condition.wait(open).unwrap();
            }
            self.out.write(buffer)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_stalled_destination_never_blocks_the_caller_and_overflow_is_counted() {
        let open = Arc::new((Mutex::new(false), Condvar::new()));
        let out = Shared::default();
        let log = JsonLines::to_writer_with_capacity(
            Box::new(Gated {
                open: open.clone(),
                out: out.clone(),
            }),
            4,
        );
        let started = Instant::now();
        for n in 0..200 {
            log.write(&serde_json::json!({"n": n}));
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "write must not wait for the destination"
        );
        // One line is inside the stalled write and four are queued.
        assert!(log.dropped() >= 190, "dropped {}", log.dropped());
        // Releasing the destination delivers what was kept, whole and in order.
        {
            let (lock, condition) = &*open;
            *lock.lock().unwrap() = true;
            condition.notify_all();
        }
        log.sync();
        let delivered = out.text().lines().count() as u64;
        assert_eq!(delivered + log.dropped(), 200);
        // The same drops are a /metrics series, by log (this test's label is
        // "test"; the series is global, so only a lower bound is asserted). After
        // the gate is open: a failing assertion above this point would leave the
        // writer stalled and the test hanging on its drop instead of failing.
        let text = crate::gateway::metrics::render();
        let series = text
            .lines()
            .find(|line| line.starts_with("llmshim_log_lines_dropped_total{log=\"test\"}"))
            .unwrap_or_else(|| panic!("no dropped series in {text}"));
        let counted: u64 = series.rsplit(' ').next().unwrap().parse().unwrap();
        assert!(counted >= log.dropped(), "{series}");

        assert!(out
            .text()
            .lines()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok()));
    }

    #[test]
    fn a_failing_destination_is_counted_and_never_reaches_the_caller() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let log = JsonLines::to_writer(Box::new(Broken));
        for n in 0..5 {
            log.write(&serde_json::json!({"n": n}));
        }
        log.sync();
        assert_eq!(log.failed(), 5);
        let text = crate::gateway::metrics::render();
        let series = text
            .lines()
            .find(|line| line.starts_with("llmshim_log_write_failures_total{log=\"test\"}"))
            .unwrap_or_else(|| panic!("no failure series in {text}"));
        let counted: u64 = series.rsplit(' ').next().unwrap().parse().unwrap();
        assert!(counted >= 5, "{series}");
    }
}
