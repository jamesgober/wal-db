//! Injected I/O failures: disk-full on append, and fsync failure.
//!
//! Real disks fail. The `WalStore` seam lets these tests drive the exact error
//! paths a full disk or a failing flush would hit, and check the log's promises
//! hold: a failed append surfaces the error and never corrupts the records
//! already written, a failed flush is reported (never silently swallowed), and a
//! write failure fail-stops the log so nothing past the gap is read as durable.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use wal_db::{MemStore, Wal, WalError, WalStore};

/// An in-memory store that can be told to fail a particular write or every sync.
struct FaultyStore {
    inner: MemStore,
    writes: AtomicUsize,
    /// Fail the write whose 1-based count reaches this (`usize::MAX` = never).
    fail_write_on: AtomicUsize,
    fail_sync: AtomicBool,
}

impl FaultyStore {
    fn new() -> Self {
        FaultyStore {
            inner: MemStore::new(),
            writes: AtomicUsize::new(0),
            fail_write_on: AtomicUsize::new(usize::MAX),
            fail_sync: AtomicBool::new(false),
        }
    }

    fn fail_write_number(self, n: usize) -> Self {
        self.fail_write_on.store(n, Ordering::SeqCst);
        self
    }

    fn failing_sync(self) -> Self {
        self.fail_sync.store(true, Ordering::SeqCst);
        self
    }
}

impl WalStore for FaultyStore {
    fn write_at(&self, offset: u64, bytes: &[u8]) -> wal_db::Result<()> {
        let n = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
        if n >= self.fail_write_on.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected: disk full").into());
        }
        self.inner.write_at(offset, bytes)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> wal_db::Result<usize> {
        self.inner.read_at(offset, buf)
    }

    fn truncate(&self, len: u64) -> wal_db::Result<()> {
        self.inner.truncate(len)
    }

    fn sync(&self) -> wal_db::Result<()> {
        if self.fail_sync.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected: fsync failed").into());
        }
        self.inner.sync()
    }

    fn len(&self) -> wal_db::Result<u64> {
        self.inner.len()
    }
}

#[test]
fn disk_full_on_append_returns_error_and_keeps_earlier_records() {
    // Fail the fourth write (the fourth record's frame).
    let store = FaultyStore::new().fail_write_number(4);
    let wal = Wal::with_store(store).unwrap();

    let _ = wal.append(b"one").unwrap();
    let _ = wal.append(b"two").unwrap();
    let _ = wal.append(b"three").unwrap();

    let err = wal.append(b"four").unwrap_err();
    assert!(
        matches!(err, WalError::Io { .. }),
        "disk-full should be an Io error, got {err:?}"
    );

    // The three records written before the failure are intact and readable.
    let got: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|e| e.unwrap().into_data())
        .collect();
    assert_eq!(
        got,
        vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
    );
}

#[test]
fn sync_after_a_failed_write_reports_the_truncation() {
    let store = FaultyStore::new().fail_write_number(2);
    let wal = Wal::with_store(store).unwrap();

    let _ = wal.append(b"good").unwrap();
    let _ = wal.append(b"doomed").unwrap_err(); // second write fails -> poison

    // A sync whose range covers the failed record cannot promise durability.
    let err = wal.sync().unwrap_err();
    assert!(
        matches!(err, WalError::Corruption { .. }),
        "sync past a failed write should report corruption, got {err:?}"
    );

    // Syncing only the durable prefix still succeeds (nothing to flush past it).
    // The first record is readable.
    assert_eq!(wal.iter().unwrap().count(), 1);
}

#[test]
fn fsync_failure_surfaces_as_an_error() {
    let store = FaultyStore::new().failing_sync();
    let wal = Wal::with_store(store).unwrap();

    let _ = wal.append(b"record").unwrap(); // append reaches the page cache fine

    let err = wal.sync().unwrap_err();
    assert!(
        matches!(err, WalError::Io { .. }),
        "fsync failure should be an Io error, got {err:?}"
    );
}

#[test]
fn append_and_sync_propagates_a_write_failure() {
    let store = FaultyStore::new().fail_write_number(1); // the very first write fails
    let wal = Wal::with_store(store).unwrap();

    let err = wal.append_and_sync(b"never lands").unwrap_err();
    assert!(matches!(err, WalError::Io { .. }), "got {err:?}");
    assert_eq!(wal.iter().unwrap().count(), 0);
}
