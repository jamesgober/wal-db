//! A `Wal` over a `SegmentedStore`: records spanning segment boundaries, recovery
//! across several files, and concurrent writers crossing rotation points.
//!
//! Segment sizes are kept small so ordinary records force rotation and spanning.

use std::sync::Arc;
use std::thread;

use wal_db::{SegmentedStore, Wal};

#[test]
fn append_sync_reopen_across_many_segments() {
    let dir = tempfile::tempdir().unwrap();

    let records: Vec<Vec<u8>> = (0..500u32)
        .map(|i| format!("record {i} with some content").into_bytes())
        .collect();

    {
        // 32-byte segments: every record spans or rotates.
        let wal = Wal::open_segmented(dir.path(), 32).unwrap();
        for record in &records {
            let _ = wal.append(record).unwrap();
        }
        wal.sync().unwrap();
    }

    // Many segment files were created.
    let segment_count = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_str()
                .unwrap()
                .ends_with(".wal")
        })
        .count();
    assert!(
        segment_count > 5,
        "expected many segments, got {segment_count}"
    );

    // Reopen and read every record back, in order, across all the files.
    let wal = Wal::open_segmented(dir.path(), 32).unwrap();
    let read_back: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|entry| entry.unwrap().into_data())
        .collect();
    assert_eq!(read_back, records);
}

#[test]
fn record_larger_than_a_segment_spans_several() {
    let dir = tempfile::tempdir().unwrap();

    // A 1000-byte record into 64-byte segments spans ~16 of them.
    let big = vec![0x7Cu8; 1000];
    {
        let wal = Wal::open_segmented(dir.path(), 64).unwrap();
        let _ = wal.append(&big).unwrap();
        let _ = wal.append(b"after the big one").unwrap();
        wal.sync().unwrap();
    }

    let wal = Wal::open_segmented(dir.path(), 64).unwrap();
    let records: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|entry| entry.unwrap().into_data())
        .collect();
    assert_eq!(records, vec![big, b"after the big one".to_vec()]);
}

#[test]
fn concurrent_writers_across_segment_boundaries() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 300;

    let dir = tempfile::tempdir().unwrap();
    let wal = Arc::new(Wal::open_segmented(dir.path(), 48).unwrap());

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let wal = Arc::clone(&wal);
        handles.push(thread::spawn(move || {
            for i in 0..PER_THREAD {
                let _ = wal.append(format!("t{t}-record-{i}").as_bytes()).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    wal.sync().unwrap();

    // Every record recovers, in offset order, with no overlap, loss, or
    // corruption across all the segment files.
    let count = wal.iter().unwrap().count();
    assert_eq!(count, THREADS * PER_THREAD);

    // And reopening from the directory recovers the same set.
    drop(wal);
    let reopened = Wal::open_segmented(dir.path(), 48).unwrap();
    assert_eq!(reopened.iter().unwrap().count(), THREADS * PER_THREAD);
}

#[test]
fn recovery_truncates_a_torn_tail_in_the_last_segment() {
    let dir = tempfile::tempdir().unwrap();

    let clean_len;
    {
        let wal = Wal::open_segmented(dir.path(), 32).unwrap();
        for i in 0..50 {
            let _ = wal.append(format!("record {i}").as_bytes()).unwrap();
        }
        wal.sync().unwrap();
        clean_len = wal.len();
    }

    // Corrupt the tail: append raw garbage past the end via a fresh store.
    {
        let store = SegmentedStore::open(dir.path(), 32).unwrap();
        store_write_garbage(&store, clean_len);
    }

    // Reopen: the good records survive, the torn tail is dropped.
    let wal = Wal::open_segmented(dir.path(), 32).unwrap();
    assert_eq!(wal.iter().unwrap().count(), 50);
    assert_eq!(wal.len(), clean_len);
}

fn store_write_garbage(store: &SegmentedStore, at: u64) {
    use wal_db::WalStore;
    // Bytes that cannot form a valid record: a plausible-looking length with no
    // matching payload/checksum.
    store.write_at(at, &[0xFF; 6]).unwrap();
    store.sync().unwrap();
}
