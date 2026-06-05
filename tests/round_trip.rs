//! Single-writer round-trip: append, sync, close, reopen, read back.
//!
//! This is the core durability promise at the integration level — that a log
//! written and synced in one session reads back intact in the next.

use wal_db::Wal;

/// The framed size of a record: an 8-byte header plus the payload.
fn framed(payload_len: usize) -> u64 {
    (8 + payload_len) as u64
}

#[test]
fn append_sync_reopen_reads_back_all_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("round_trip.wal");

    let records: Vec<Vec<u8>> = (0..1_000u32)
        .map(|i| format!("record number {i}").into_bytes())
        .collect();

    let mut expected_offset = 0u64;
    {
        let wal = Wal::open(&path).unwrap();
        for record in &records {
            // The LSN is the record's byte offset; offsets advance by framed size.
            let lsn = wal.append(record).unwrap();
            assert_eq!(lsn.get(), expected_offset);
            expected_offset += framed(record.len());
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
    assert_eq!(wal.len(), expected_offset);
    // Appends resume at the recovered end with no gap.
    assert_eq!(wal.append(b"one more").unwrap().get(), expected_offset);
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
    assert!(wal.is_empty());
    assert_eq!(wal.iter().unwrap().count(), 0);
}

#[test]
fn records_survive_multiple_reopen_cycles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cycles.wal");

    for batch in 0..5 {
        let wal = Wal::open(&path).unwrap();
        for _ in 0..10 {
            let _ = wal.append(format!("batch {batch}").as_bytes()).unwrap();
        }
        wal.sync().unwrap();
    }

    let wal = Wal::open(&path).unwrap();
    assert_eq!(wal.iter().unwrap().count(), 50);
}
