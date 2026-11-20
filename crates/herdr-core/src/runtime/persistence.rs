//! Ordered registry persistence, without disk IO on the core state lock.
//!
//! The writer never stops. A write that fails (a full disk) is kept and tried
//! again, backing off, and only the newest snapshot is ever written. The caller
//! who asked hears of the failure at once; nothing waits for a disk that may
//! stay full.
use crate::paths::write_atomic;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub(super) struct RegistryWriter {
    queue: Arc<(Mutex<Pending>, Condvar)>,
    progress: watch::Receiver<Progress>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Pending {
    latest: Option<(u64, Vec<u8>)>,
    submitted: u64,
    closed: bool,
}

/// How far the writer got: the newest snapshot it tried, and why that failed
/// (with when saving started to fail, ms since the epoch).
#[derive(Clone, Default)]
struct Progress {
    tried: u64,
    failed: Option<String>,
    since_ms: u64,
}

/// The wait before the first retry of a failed write; it doubles up to `RETRY_MAX` times this.
fn retry_base() -> Duration {
    let ms = std::env::var("HC_SAVE_RETRY_MS").ok().and_then(|v| v.parse().ok());
    Duration::from_millis(ms.unwrap_or(if cfg!(test) { 10 } else { 250 }))
}
const RETRY_MAX: u32 = 20;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One write, which may fail or panic: either is a failed write, never the end of the writer.
fn attempt(write: &mut impl FnMut(&[u8]) -> io::Result<()>, bytes: &[u8]) -> io::Result<()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write(bytes)))
        .unwrap_or_else(|_| Err(io::Error::other("the write panicked")))
}

impl RegistryWriter {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        Self::start(move |bytes| write_atomic(&path, bytes))
    }

    pub(super) fn start(mut write: impl FnMut(&[u8]) -> io::Result<()> + Send + 'static) -> io::Result<Self> {
        let queue = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let (done, progress) = watch::channel(Progress::default());
        let work = queue.clone();
        let worker = std::thread::Builder::new().name("registry-writer".into()).spawn(move || {
            let (mutex, changed) = &*work;
            let base = retry_base();
            let mut backoff = base;
            // The newest snapshot not saved yet, and while failing, when to try it again.
            let mut held: Option<(u64, Vec<u8>)> = None;
            let mut next_try: Option<Instant> = None;
            let mut failing = false;
            let mut since_ms = 0;
            let mut last_try = false;
            let mut pending = lock(mutex);
            loop {
                if let Some(newest) = pending.latest.take() {
                    held = Some(newest);
                }
                let closing = pending.closed;
                // Once closed it tries what it still holds one last time, then ends.
                let finished = closing && last_try;
                let due = held.is_some() && !finished && (closing || next_try.is_none_or(|at| Instant::now() >= at));
                if due {
                    let (version, bytes) = held.take().expect("due means held");
                    drop(pending);
                    match attempt(&mut write, &bytes) {
                        Ok(()) => {
                            if failing {
                                crate::log!("core: saving terminals again");
                            }
                            (failing, next_try, backoff) = (false, None, base);
                            done.send_replace(Progress { tried: version, ..Default::default() });
                        }
                        Err(e) => {
                            if !failing {
                                crate::log!("core: cannot save terminals, trying again: {e}");
                                since_ms = super::now_ms();
                            }
                            failing = true;
                            next_try = Some(Instant::now() + backoff);
                            backoff = (backoff * 2).min(base * RETRY_MAX);
                            held = Some((version, bytes));
                            done.send_replace(Progress { tried: version, failed: Some(e.to_string()), since_ms });
                        }
                    }
                    last_try = closing;
                    pending = lock(mutex);
                } else if closing && (held.is_none() || finished) {
                    break;
                } else {
                    pending = match next_try.filter(|_| held.is_some()) {
                        Some(at) => {
                            let wait = at.saturating_duration_since(Instant::now());
                            changed.wait_timeout(pending, wait).unwrap_or_else(|poisoned| poisoned.into_inner()).0
                        }
                        None => changed.wait(pending).unwrap_or_else(|poisoned| poisoned.into_inner()),
                    };
                }
            }
        })?;
        Ok(Self { queue, progress, worker: Some(worker) })
    }

    /// Called under the core lock: snapshots enter the queue in state order.
    pub fn save(&self, bytes: Vec<u8>) {
        let (mutex, changed) = &*self.queue;
        let mut pending = lock(mutex);
        pending.submitted += 1;
        pending.latest = Some((pending.submitted, bytes));
        changed.notify_one();
    }

    /// While saving fails: when it started to (ms since the epoch) and why the newest try failed.
    pub fn failing(&self) -> Option<(u64, String)> {
        let progress = self.progress.borrow();
        progress.failed.clone().map(|why| (progress.since_ms, why))
    }

    /// Wait for the current snapshot, not for unrelated future saves. Never hold the core lock.
    /// While the writer is failing the answer is the failure, at once: it keeps trying on its own.
    pub async fn flush(&self) -> io::Result<()> {
        let version = lock(&self.queue.0).submitted;
        let mut progress = self.progress.clone();
        let failed = progress
            .wait_for(|p| p.tried >= version || p.failed.is_some())
            .await
            .map_err(|_| io::Error::other("registry writer stopped"))?
            .failed
            .clone();
        failed.map_or(Ok(()), |why| Err(io::Error::other(why)))
    }
}

impl Drop for RegistryWriter {
    fn drop(&mut self) {
        let (mutex, changed) = &*self.queue;
        lock(mutex).closed = true;
        changed.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn a_slow_write_keeps_only_the_newest_pending_snapshot_and_drop_flushes_it() {
        let (started, writes) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let writer = RegistryWriter::start(move |bytes| {
            started.send(bytes.to_vec()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
        .unwrap();
        writer.save(vec![1]);
        assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [1]);
        writer.save(vec![2]);
        writer.save(vec![3]);
        let (finished, done) = mpsc::channel();
        let closing = std::thread::spawn(move || {
            drop(writer);
            finished.send(()).unwrap();
        });
        assert!(done.try_recv().is_err());
        release.send(()).unwrap();
        assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [3]);
        assert!(done.try_recv().is_err());
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(5)).unwrap();
        closing.join().unwrap();
        assert!(writes.try_recv().is_err());
    }

    #[tokio::test]
    async fn flush_waits_for_the_atomic_replacement() {
        let root = std::env::temp_dir().join(format!("hc-writer-{}", crate::platform::random_hex()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("terminals.json");
        let writer = RegistryWriter::new(path.clone()).unwrap();
        writer.save(b"first".to_vec());
        writer.save(b"last".to_vec());
        writer.flush().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"last");
        assert!(!path.with_extension("tmp").exists());
        drop(writer);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_flush_does_not_wait_for_later_snapshots() {
        let (started, writes) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let writer = RegistryWriter::start(move |bytes| {
            started.send(bytes.to_vec()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
        .unwrap();
        writer.save(vec![1]);
        assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [1]);
        {
            let waiting = writer.flush();
            tokio::pin!(waiting);
            assert!(tokio::time::timeout(Duration::from_millis(10), &mut waiting).await.is_err());
            writer.save(vec![2]);
            release.send(()).unwrap();
            assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [2]);
            tokio::time::timeout(Duration::from_secs(1), &mut waiting).await.unwrap().unwrap();
        }
        release.send(()).unwrap();
        drop(writer);
    }

    /// A write that fails while `failing` is set, and otherwise sends what it got.
    fn flaky(failing: &Arc<AtomicBool>) -> (RegistryWriter, mpsc::Receiver<Vec<u8>>) {
        let (wrote, writes) = mpsc::channel();
        let failing = failing.clone();
        let writer = RegistryWriter::start(move |bytes| {
            if failing.load(Ordering::SeqCst) {
                return Err(io::Error::other("disk full"));
            }
            wrote.send(bytes.to_vec()).unwrap();
            Ok(())
        })
        .unwrap();
        (writer, writes)
    }

    /// Waits for the writer to report a save again: it reports a write after making it.
    async fn saved(writer: &RegistryWriter) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while writer.flush().await.is_err() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the writer saves again");
    }

    #[tokio::test]
    async fn a_failed_write_is_tried_again_and_never_stops_the_writer() {
        let failing = Arc::new(AtomicBool::new(true));
        let (writer, writes) = flaky(&failing);
        // The caller hears of the failure at once, and so does the next one,
        // for the newer snapshot: nothing waits for a disk that may stay full.
        writer.save(vec![1]);
        assert_eq!(writer.flush().await.unwrap_err().to_string(), "disk full");
        let (since, why) = writer.failing().expect("saving fails");
        assert!(since > 0 && why == "disk full");
        writer.save(vec![2]);
        assert_eq!(writer.flush().await.unwrap_err().to_string(), "disk full");
        assert!(writes.try_recv().is_err());
        // The disk comes back: the newest snapshot is written by itself, and a flush is good again.
        failing.store(false, Ordering::SeqCst);
        assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [2]);
        saved(&writer).await;
        assert!(writer.failing().is_none());
        // Still the same writer.
        writer.save(vec![3]);
        writer.flush().await.unwrap();
        assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [3]);
    }

    #[tokio::test]
    async fn a_panicking_write_is_a_failed_write() {
        let calls = Arc::new(AtomicU32::new(0));
        let counted = calls.clone();
        let (wrote, writes) = mpsc::channel();
        let writer = RegistryWriter::start(move |bytes| {
            assert!(counted.fetch_add(1, Ordering::SeqCst) > 0, "the first write panics");
            wrote.send(bytes.to_vec()).unwrap();
            Ok(())
        })
        .unwrap();
        writer.save(vec![1]);
        assert!(writer.flush().await.is_err());
        assert_eq!(writes.recv_timeout(Duration::from_secs(5)).unwrap(), [1]);
        saved(&writer).await;
    }

    #[test]
    fn dropping_a_writer_whose_disk_is_full_still_ends() {
        let failing = Arc::new(AtomicBool::new(true));
        let (writer, writes) = flaky(&failing);
        writer.save(vec![1]);
        drop(writer);
        assert!(writes.try_recv().is_err());
    }
}
