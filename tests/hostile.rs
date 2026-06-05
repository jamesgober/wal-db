//! Adversarial recovery inputs.
//!
//! The fuzz harness proves recovery never panics or over-allocates on *arbitrary*
//! bytes; these are the specific hostile shapes, as named regression tests, with
//! the exact guarantee each one checks. Recovery must read all-and-only the
//! intact records and never trust a length or a checksum it has not verified.

use std::fs;

use wal_db::{Wal, WalConfig};

/// Write `records` to a fresh file and return its path (and the dir, kept alive).
fn build_log(records: &[&[u8]]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hostile.wal");
    let wal = Wal::open(&path).unwrap();
    for record in records {
        let _ = wal.append(record).unwrap();
    }
    wal.sync().unwrap();
    (dir, path)
}

fn record_count(path: &std::path::Path) -> usize {
    Wal::open(path).unwrap().iter().unwrap().count()
}

#[test]
fn garbage_prefix_recovers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("garbage.wal");
    // A file that begins with bytes that cannot form a valid record.
    fs::write(&path, b"this is not a wal-db record at all, just text").unwrap();

    let wal = Wal::open(&path).unwrap();
    assert_eq!(wal.iter().unwrap().count(), 0);
    // The torn (here, entirely invalid) content is truncated away on open.
    assert_eq!(wal.len(), 0);
}

#[test]
fn implausible_length_is_rejected_without_allocating() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("biglen.wal");
    // An 8-byte "header": a checksum, then a length of 0xFFFF_FFFF (~4 GiB). With
    // a small max record size, recovery must reject it on the length check —
    // before reading or allocating a single payload byte.
    fs::write(&path, [0u8, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]).unwrap();

    let config = WalConfig::new().with_max_record_size(1024);
    let wal = Wal::open_with(&path, config).unwrap(); // must not try to allocate 4 GiB
    assert_eq!(wal.iter().unwrap().count(), 0);
    assert_eq!(wal.len(), 0);
}

#[test]
fn all_zeros_recovers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zeros.wal");
    // A block of zeros parses as a zero-length record with checksum 0, which does
    // not match the checksum of an empty record — so recovery stops at it.
    fs::write(&path, vec![0u8; 4096]).unwrap();

    let wal = Wal::open(&path).unwrap();
    assert_eq!(wal.iter().unwrap().count(), 0);
}

#[test]
fn valid_records_then_garbage_tail_keeps_the_valid_ones() {
    let (_dir, path) = build_log(&[b"first", b"second", b"third"]);
    let clean_len = fs::metadata(&path).unwrap().len();

    // Append raw garbage past the last good record.
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(b"\xDE\xAD\xBE\xEF garbage tail");
    fs::write(&path, &bytes).unwrap();

    let wal = Wal::open(&path).unwrap();
    let got: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|e| e.unwrap().into_data())
        .collect();
    assert_eq!(
        got,
        vec![b"first".to_vec(), b"second".to_vec(), b"third".to_vec()]
    );
    // The garbage tail was truncated back to the last good record.
    assert_eq!(wal.len(), clean_len);
}

#[test]
fn corrupt_middle_record_truncates_from_there() {
    let (_dir, path) = build_log(&[b"alpha", b"bravo", b"charlie"]);

    // Flip a byte inside the second record's payload. Record 1 is 8 + 5 = 13
    // bytes, so record 2's payload begins at 13 + 8 = 21.
    let mut bytes = fs::read(&path).unwrap();
    bytes[21] ^= 0xFF;
    fs::write(&path, &bytes).unwrap();

    // Default recovery stops at the first bad record, so on open the log is
    // truncated to just the first record.
    let wal = Wal::open(&path).unwrap();
    let got: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|e| e.unwrap().into_data())
        .collect();
    assert_eq!(got, vec![b"alpha".to_vec()]);
    assert_eq!(wal.len(), 13);
}

#[test]
fn truncated_mid_payload_drops_the_partial_record() {
    let (_dir, path) = build_log(&[b"complete", b"incomplete"]);

    // Cut the file inside the second record's payload: record 1 is 8 + 8 = 16,
    // record 2's header ends at 16 + 8 = 24, so 28 is four bytes into its payload.
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(28).unwrap();
    drop(file);

    assert_eq!(record_count(&path), 1);
}

#[test]
fn a_torn_head_marker_falls_back_to_full_recovery() {
    let dir = tempfile::tempdir().unwrap();
    {
        let wal = Wal::open_segmented(dir.path(), 32).unwrap();
        for i in 0..10u32 {
            let _ = wal.append(format!("rec{i}").as_bytes()).unwrap();
        }
        wal.sync().unwrap();
    }

    // A short (torn) head marker — the kind a crash mid-write could leave — has no
    // valid checksum, so it is ignored: recovery falls back to the head of 0 and
    // reads every record. Nothing is silently skipped.
    fs::write(dir.path().join("head"), 3u64.to_le_bytes()).unwrap(); // 8 bytes, no crc
    let wal = Wal::open_segmented(dir.path(), 32).unwrap();
    assert_eq!(wal.iter().unwrap().count(), 10);

    // A full-length marker with a corrupt checksum is rejected the same way, even
    // though its offset (here, mid-record) looks plausible.
    let mut corrupt = [0u8; 12];
    corrupt[..8].copy_from_slice(&3u64.to_le_bytes()); // plausible offset, bogus crc bytes
    fs::write(dir.path().join("head"), corrupt).unwrap();
    let wal = Wal::open_segmented(dir.path(), 32).unwrap();
    assert_eq!(wal.iter().unwrap().count(), 10);
}

#[test]
fn truncated_mid_header_drops_the_partial_record() {
    let (_dir, path) = build_log(&[b"whole"]);

    // Append four bytes — half a header — after the good record.
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(&[0xAB; 4]);
    fs::write(&path, &bytes).unwrap();

    let wal = Wal::open(&path).unwrap();
    assert_eq!(wal.iter().unwrap().count(), 1);
    assert_eq!(wal.len(), 13); // 8 + 5, the one good record
}
