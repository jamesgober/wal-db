//! Single-writer round-trip: append, sync, close, reopen, read back.
//!
//! This is the core durability promise at the integration level — that a log
//! written and synced in one session reads back intact in the next.

use wal_db::Wal;

#[test]
fn append_sync_reopen_reads_back_all_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("round_trip.wal");

    let records: Vec<Vec<u8>> = (0..1_000u32)
        .map(|i| format!("record number {i}").into_bytes())
        .collect();

    {
        let wal = Wal::open(&path).unwrap();
        for (i, record) in records.iter().enumerate() {
            assert_eq!(wal.append(record).unwrap().get(), i as u64);
        }
        wal.sync().unwrap();
    } // dropping the Wal closes the file handle

    let wal = Wal::open(&path).unwrap();
    let read_back: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|entry| entry.unwrap().into_data())
        .collect();

    assert_eq!(read_back, records);
    // Appends resume at the next sequence number with no gap.
    assert_eq!(wal.append(b"one more").unwrap().get(), records.len() as u64);
}

#[test]
fn mixed_sizes_including_empty_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.wal");

    let records: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"x".to_vec(),
        vec![0u8; 4096],
        b"a medium record".to_vec(),
        vec![0xFF; 1],
        Vec::new(),
    ];

    {
        let wal = Wal::open(&path).unwrap();
        for record in &records {
            let _ = wal.append(record).unwrap();
        }
        wal.sync().unwrap();
    }

    let wal = Wal::open(&path).unwrap();
    let read_back: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|entry| entry.unwrap().into_data())
        .collect();
    assert_eq!(read_back, records);
}

#[test]
fn reopening_a_fresh_path_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.wal");

    let wal = Wal::open(&path).unwrap();
    assert!(wal.is_empty().unwrap());
    assert_eq!(wal.iter().unwrap().count(), 0);
}

#[test]
fn lsns_survive_multiple_reopen_cycles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cycles.wal");

    let mut expected_next = 0u64;
    for batch in 0..5 {
        let wal = Wal::open(&path).unwrap();
        for _ in 0..10 {
            let payload = format!("batch {batch}").into_bytes();
            assert_eq!(wal.append(&payload).unwrap().get(), expected_next);
            expected_next += 1;
        }
        wal.sync().unwrap();
    }

    let wal = Wal::open(&path).unwrap();
    assert_eq!(wal.iter().unwrap().count(), 50);
}
