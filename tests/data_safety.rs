//! Opens and truncations that once cut or deleted synced data now refuse
//! instead, leaving the files as they were (1.4). One test per case; each
//! was a destructive open before.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wal_db::{Lsn, MemStore, OpenPolicy, Wal, WalConfig, WalError, WalStore};

fn seg_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".wal"))
        .collect();
    names.sort();
    names
}

fn count(wal: &Wal<impl WalStore>) -> usize {
    wal.iter().unwrap().filter(Result::is_ok).count()
}

fn write_segmented(dir: &Path, seg: u64, n: usize) -> Vec<Lsn> {
    let wal = Wal::open_segmented(dir, seg).unwrap();
    let lsns = (0..n)
        .map(|i| {
            wal.append(format!("record number {i:04}").as_bytes())
                .unwrap()
        })
        .collect();
    wal.sync().unwrap();
    lsns
}

const POLICIES: [OpenPolicy; 2] = [OpenPolicy::TruncateTail, OpenPolicy::Strict];

#[test]
fn test_a_file_that_is_not_a_log_is_refused_and_kept() {
    for policy in POLICIES {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("important.db");
        let text = "precious user data, not a wal\n".repeat(200);
        fs::write(&path, &text).unwrap();
        let opened = Wal::open_with(&path, WalConfig::new().with_open_policy(policy));
        assert!(
            matches!(opened, Err(WalError::Corruption { offset: 0, .. })),
            "{policy:?}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), text, "{policy:?}");
    }
}

#[test]
fn test_another_segment_size_is_refused_and_the_log_kept() {
    for new_size in [32u64, 4096] {
        for policy in POLICIES {
            let dir = tempfile::tempdir().unwrap();
            let _ = write_segmented(dir.path(), 64, 40);
            let before = seg_files(dir.path());
            let opened = Wal::open_segmented_with(
                dir.path(),
                new_size,
                WalConfig::new().with_open_policy(policy),
            );
            assert!(
                matches!(opened, Err(WalError::Corruption { .. })),
                "size {new_size}, {policy:?}"
            );
            assert_eq!(seg_files(dir.path()), before);
            let wal = Wal::open_segmented(dir.path(), 64).unwrap();
            assert_eq!(count(&wal), 40);
        }
    }
}

/// A log written before 1.4 has no `meta` file: the segment lengths tell the
/// size, the first open records it, and another size is still refused.
#[test]
fn test_a_log_from_before_1_4_has_its_segment_size_checked_and_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let _ = write_segmented(dir.path(), 64, 40);
    fs::remove_file(dir.path().join("meta")).unwrap();
    assert!(matches!(
        Wal::open_segmented(dir.path(), 32),
        Err(WalError::Corruption { .. })
    ));
    assert!(matches!(
        Wal::open_segmented(dir.path(), 4096),
        Err(WalError::Corruption { .. })
    ));
    assert!(!dir.path().join("meta").exists());
    let wal = Wal::open_segmented(dir.path(), 64).unwrap();
    assert_eq!(count(&wal), 40);
    drop(wal);
    assert!(dir.path().join("meta").exists());
}

#[test]
fn test_a_lost_head_marker_after_a_prefix_drop_is_refused() {
    for policy in POLICIES {
        let dir = tempfile::tempdir().unwrap();
        let lsns = write_segmented(dir.path(), 64, 40);
        {
            let wal = Wal::open_segmented(dir.path(), 64).unwrap();
            let _ = wal.truncate_before(lsns[30]).unwrap();
        }
        fs::remove_file(dir.path().join("head")).unwrap();
        let before = seg_files(dir.path());
        let opened =
            Wal::open_segmented_with(dir.path(), 64, WalConfig::new().with_open_policy(policy));
        assert!(
            matches!(opened, Err(WalError::Corruption { .. })),
            "{policy:?}"
        );
        assert_eq!(seg_files(dir.path()), before, "{policy:?}");
    }
}

#[test]
fn test_truncate_before_an_offset_that_is_not_a_record_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let lsns = write_segmented(dir.path(), 64, 40);
    {
        let wal = Wal::open_segmented(dir.path(), 64).unwrap();
        assert!(matches!(
            wal.truncate_before(Lsn::new(lsns[20].get() + 1)),
            Err(WalError::Corruption { .. })
        ));
        // A real boundary still works.
        assert_eq!(wal.truncate_before(lsns[20]).unwrap(), lsns[20]);
    }
    let wal = Wal::open_segmented(dir.path(), 64).unwrap();
    assert_eq!(count(&wal), 20);
}

#[test]
fn test_a_record_above_a_lowered_limit_is_refused_not_cut() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.wal");
    {
        let wal = Wal::open(&path).unwrap();
        let _ = wal.append(b"small").unwrap();
        let _ = wal.append(&[7u8; 1000]).unwrap();
        for _ in 0..10 {
            let _ = wal.append(b"later record").unwrap();
        }
        wal.sync().unwrap();
    }
    let len = fs::metadata(&path).unwrap().len();
    assert!(matches!(
        Wal::open_with(&path, WalConfig::new().with_max_record_size(500)),
        Err(WalError::Corruption { .. })
    ));
    assert_eq!(fs::metadata(&path).unwrap().len(), len);
    let wal = Wal::open(&path).unwrap();
    assert_eq!(count(&wal), 12);
}

/// A truncation that ends exactly on a segment boundary removes the
/// segment after it instead of emptying it.
#[test]
fn test_truncate_after_at_a_segment_boundary_removes_the_next_segment() {
    let dir = tempfile::tempdir().unwrap();
    // 24-byte payloads frame to 32 bytes: two records per 64-byte segment.
    let wal = Wal::open_segmented(dir.path(), 64).unwrap();
    let lsns: Vec<Lsn> = (0..6).map(|_| wal.append(&[1u8; 24]).unwrap()).collect();
    wal.sync().unwrap();
    assert_eq!(seg_files(dir.path()).len(), 3);
    wal.truncate_after(lsns[1]).unwrap(); // ends at 64
    assert_eq!(seg_files(dir.path()).len(), 1);
    let _ = wal.append(&[2u8; 24]).unwrap();
    wal.sync().unwrap();
    drop(wal);
    let wal = Wal::open_segmented(dir.path(), 64).unwrap();
    assert_eq!(count(&wal), 3);
}

/// A store that panics on its `panic_on`-th write.
struct PanicStore {
    inner: MemStore,
    writes: Mutex<u32>,
    panic_on: u32,
}

impl WalStore for PanicStore {
    fn write_at(&self, offset: u64, bytes: &[u8]) -> wal_db::Result<()> {
        let n = {
            let mut w = self.writes.lock().unwrap();
            *w += 1;
            *w
        };
        assert!(n != self.panic_on, "store bug");
        self.inner.write_at(offset, bytes)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> wal_db::Result<usize> {
        self.inner.read_at(offset, buf)
    }
    fn truncate(&self, len: u64) -> wal_db::Result<()> {
        self.inner.truncate(len)
    }
    fn sync(&self) -> wal_db::Result<()> {
        Ok(())
    }
    fn len(&self) -> wal_db::Result<u64> {
        self.inner.len()
    }
}

#[test]
fn test_a_panicking_write_fails_later_syncs_instead_of_hanging() {
    let wal = Arc::new(
        Wal::with_store(PanicStore {
            inner: MemStore::new(),
            writes: Mutex::new(0),
            panic_on: 2,
        })
        .unwrap(),
    );
    let _ = wal.append(b"one").unwrap();
    let w = Arc::clone(&wal);
    assert!(std::thread::spawn(move || w.append(b"two")).join().is_err());
    let _ = wal.append(b"three").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let w = Arc::clone(&wal);
    let _ = std::thread::spawn(move || {
        let _ = tx.send(w.sync().is_err());
    });
    // The range the panicking write reserved is poisoned: the sync fails
    // at once rather than waiting for it.
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true));
}

/// A store whose syncs fail once `fail` is set.
struct FailSyncStore {
    inner: MemStore,
    fail: Arc<Mutex<bool>>,
}

impl WalStore for FailSyncStore {
    fn write_at(&self, offset: u64, bytes: &[u8]) -> wal_db::Result<()> {
        self.inner.write_at(offset, bytes)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> wal_db::Result<usize> {
        self.inner.read_at(offset, buf)
    }
    fn truncate(&self, len: u64) -> wal_db::Result<()> {
        self.inner.truncate(len)
    }
    fn sync(&self) -> wal_db::Result<()> {
        if *self.fail.lock().unwrap() {
            return Err(WalError::from(std::io::Error::other("EIO")));
        }
        Ok(())
    }
    fn len(&self) -> wal_db::Result<u64> {
        self.inner.len()
    }
}

/// A failed sync inside `truncate_after` is permanent like any other, and
/// the log's end has moved, so nothing lands past a gap.
#[test]
fn test_a_failed_sync_in_truncate_after_is_sticky() {
    let fail = Arc::new(Mutex::new(false));
    let wal = Wal::with_store(FailSyncStore {
        inner: MemStore::new(),
        fail: Arc::clone(&fail),
    })
    .unwrap();
    let keep = wal.append(b"keep").unwrap();
    let _ = wal.append(b"drop").unwrap();
    wal.sync().unwrap();
    *fail.lock().unwrap() = true;
    assert!(wal.truncate_after(keep).is_err());
    *fail.lock().unwrap() = false;
    let next = wal.append(b"next").unwrap();
    assert_eq!(next.get(), 12, "appended at the truncated end");
    assert!(wal.sync().is_err(), "the earlier failed flush is permanent");
}
