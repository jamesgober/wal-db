//! Boundary and edge cases: exact size limits, empty records, large records,
//! and byte-exact round-trips.

use wal_db::{MemStore, Wal, WalConfig, WalError};

#[test]
fn record_exactly_at_the_limit_is_accepted_one_over_is_rejected() {
    let config = WalConfig::new().with_max_record_size(64);
    let wal = Wal::with_store_and_config(MemStore::new(), config).unwrap();

    // Exactly the limit: accepted.
    let at_limit = vec![0xABu8; 64];
    assert!(wal.append(&at_limit).is_ok());

    // One byte over: rejected, log unchanged.
    let over = vec![0xABu8; 65];
    let err = wal.append(&over).unwrap_err();
    assert!(matches!(err, WalError::RecordTooLarge { len: 65, max: 64 }));

    // The accepted record is still the only one, intact.
    let got: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|e| e.unwrap().into_data())
        .collect();
    assert_eq!(got, vec![at_limit]);
}

#[test]
fn a_record_at_the_maximum_size_round_trips_through_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("max.wal");

    let big = vec![0x5Au8; 4096];
    {
        let config = WalConfig::new().with_max_record_size(4096);
        let wal = Wal::open_with(&path, config).unwrap();
        let _ = wal.append(&big).unwrap();
        wal.sync().unwrap();
    }

    let config = WalConfig::new().with_max_record_size(4096);
    let wal = Wal::open_with(&path, config).unwrap();
    let got: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|e| e.unwrap().into_data())
        .collect();
    assert_eq!(got, vec![big]);
}

#[test]
fn many_empty_records_round_trip() {
    let wal = Wal::with_store(MemStore::new()).unwrap();
    for _ in 0..1_000 {
        let _ = wal.append(b"").unwrap();
    }
    let records: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|e| e.unwrap().into_data())
        .collect();
    assert_eq!(records.len(), 1_000);
    assert!(records.iter().all(Vec::is_empty));
}

#[test]
fn alternating_empty_and_full_records_keep_their_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alt.wal");

    let expected: Vec<Vec<u8>> = (0..100u32)
        .map(|i| {
            if i % 2 == 0 {
                Vec::new()
            } else {
                format!("record {i}").into_bytes()
            }
        })
        .collect();

    {
        let wal = Wal::open(&path).unwrap();
        for record in &expected {
            let _ = wal.append(record).unwrap();
        }
        wal.sync().unwrap();
    }

    let wal = Wal::open(&path).unwrap();
    let got: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|e| e.unwrap().into_data())
        .collect();
    assert_eq!(got, expected);
}

#[test]
fn the_on_disk_image_is_byte_for_byte_stable() {
    // The same records written twice must produce identical bytes — the format is
    // deterministic (little-endian, fixed framing), which is what makes a log
    // portable across machines.
    fn image(records: &[&[u8]]) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stable.wal");
        let wal = Wal::open(&path).unwrap();
        for record in records {
            let _ = wal.append(record).unwrap();
        }
        wal.sync().unwrap();
        drop(wal);
        std::fs::read(&path).unwrap()
    }

    let records: &[&[u8]] = &[b"one", b"", b"three", &[0xFF; 200]];
    assert_eq!(image(records), image(records));
}

#[test]
fn len_and_is_empty_track_the_log() {
    let wal = Wal::with_store(MemStore::new()).unwrap();
    assert!(wal.is_empty());
    assert_eq!(wal.len(), 0);

    let _ = wal.append(b"abc").unwrap(); // 8 + 3
    assert!(!wal.is_empty());
    assert_eq!(wal.len(), 11);

    let _ = wal.append(b"").unwrap(); // 8 + 0
    assert_eq!(wal.len(), 19);
}
