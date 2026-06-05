//! Model-checked concurrency tests.
//!
//! Run with `RUSTFLAGS="--cfg loom" cargo test --test loom_wal`. Under `--cfg
//! loom` the library's atomics, mutex, and condvar become loom's instrumented
//! types, and `loom::model` explores every meaningful interleaving of the
//! threads below — exhaustively, not by luck. Without the cfg the file is empty.
//!
//! Thread and record counts are kept tiny on purpose: loom's state space grows
//! fast, and a two-writer model already exercises every ordering that matters
//! for the reservation and the commit handshake.

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering};
use wal_db::{Result, Wal, WalStore};

/// An in-memory store built from loom primitives, with a counter of how many
/// times `sync` (the fsync) was issued.
struct LoomStore {
    data: loom::sync::Mutex<Vec<u8>>,
    fsyncs: Arc<AtomicUsize>,
}

impl LoomStore {
    fn new(fsyncs: Arc<AtomicUsize>) -> Self {
        LoomStore {
            data: loom::sync::Mutex::new(Vec::new()),
            fsyncs,
        }
    }

    fn lock(&self) -> loom::sync::MutexGuard<'_, Vec<u8>> {
        self.data.lock().unwrap()
    }
}

impl WalStore for LoomStore {
    fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        let start = offset as usize;
        let end = start + bytes.len();
        let mut data = self.lock();
        if data.len() < end {
            data.resize(end, 0);
        }
        data[start..end].copy_from_slice(bytes);
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let data = self.lock();
        let start = offset as usize;
        if start >= data.len() {
            return Ok(0);
        }
        let n = (data.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n)
    }

    fn truncate(&self, len: u64) -> Result<()> {
        self.lock().truncate(len as usize);
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        let _ = self.fsyncs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.lock().len() as u64)
    }
}

/// Two writers append concurrently. Whatever the interleaving, the two records
/// get distinct LSNs, occupy disjoint byte ranges, and both recover intact —
/// no overlap, no reorder, no loss.
#[test]
fn loom_concurrent_append_no_overlap_or_loss() {
    loom::model(|| {
        let fsyncs = Arc::new(AtomicUsize::new(0));
        let wal = Arc::new(Wal::with_store(LoomStore::new(fsyncs)).unwrap());

        let w1 = Arc::clone(&wal);
        let t1 = loom::thread::spawn(move || w1.append(b"A").unwrap().get());
        let w2 = Arc::clone(&wal);
        let t2 = loom::thread::spawn(move || w2.append(b"B").unwrap().get());

        let lsn1 = t1.join().unwrap();
        let lsn2 = t2.join().unwrap();

        // Distinct reservations.
        assert_ne!(lsn1, lsn2);

        // Both records recover, and they are exactly "A" and "B" in some order —
        // proof that neither overwrote the other.
        let mut payloads: Vec<Vec<u8>> = wal
            .iter()
            .unwrap()
            .map(|r| r.unwrap().into_data())
            .collect();
        payloads.sort();
        assert_eq!(payloads, vec![b"A".to_vec(), b"B".to_vec()]);
    });
}

/// Two writers each append-and-sync. Whatever the interleaving, the fsync count
/// never exceeds the number of syncers (group commit coalesces, never
/// duplicates), and both records end up durable — every appender returns only
/// after its record is on stable storage.
#[test]
fn loom_group_commit_coalesces_and_is_durable() {
    loom::model(|| {
        let fsyncs = Arc::new(AtomicUsize::new(0));
        let wal = Arc::new(Wal::with_store(LoomStore::new(Arc::clone(&fsyncs))).unwrap());

        let w1 = Arc::clone(&wal);
        let t1 = loom::thread::spawn(move || {
            let _ = w1.append_and_sync(b"A").unwrap();
        });
        let w2 = Arc::clone(&wal);
        let t2 = loom::thread::spawn(move || {
            let _ = w2.append_and_sync(b"B").unwrap();
        });

        t1.join().unwrap();
        t2.join().unwrap();

        // At most one fsync per syncer; coalescing may make it fewer.
        let count = fsyncs.load(Ordering::SeqCst);
        assert!(
            count >= 1 && count <= 2,
            "fsync count {count} out of bounds"
        );

        // Both records are durable and recover.
        assert_eq!(wal.iter().unwrap().count(), 2);
    });
}
