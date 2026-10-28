//! Strict open: a damaged record followed by intact data stops the open and
//! leaves the log untouched; a damaged or incomplete final record is still cut.
//!
//! Every case opens the same bytes under both policies, so the default
//! behaviour (cut at the first damaged record) is pinned next to the strict one.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use proptest::prelude::*;
use wal_db::{OpenPolicy, Wal, WalConfig, WalError};

fn strict() -> WalConfig {
    WalConfig::new().with_open_policy(OpenPolicy::Strict)
}

/// Write `records` to a new single-file log at `path`, durably, and return
/// each record's LSN.
fn write_log(path: &Path, records: &[&[u8]]) -> Vec<u64> {
    let wal = Wal::open(path).unwrap();
    records
        .iter()
        .map(|record| wal.append_and_sync(record).unwrap().get())
        .collect()
}

fn read_all<S: wal_db::WalStore>(wal: &Wal<S>) -> Vec<Vec<u8>> {
    wal.iter()
        .unwrap()
        .map(|entry| entry.unwrap().into_data())
        .collect()
}

fn flip(path: &Path, offset: u64, mask: u8) {
    let mut bytes = fs::read(path).unwrap();
    bytes[offset as usize] ^= mask;
    fs::write(path, bytes).unwrap();
}

/// Every segment file in `dir`, sorted by name, with its contents.
fn segment_files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<(PathBuf, Vec<u8>)> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "wal"))
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    files.sort();
    files
}

fn assert_corruption_at<T: std::fmt::Debug>(result: Result<T, WalError>, expected: u64) {
    match result {
        Err(WalError::Corruption { offset, .. }) => assert_eq!(offset, expected),
        other => panic!("expected Corruption at {expected}, got {other:?}"),
    }
}

#[test]
fn clean_log_opens_with_nothing_cut_under_both_policies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clean.wal");
    let _ = write_log(&path, &[b"one", b"two", b"three"]);

    for config in [WalConfig::new(), strict()] {
        let wal = Wal::open_with(&path, config).unwrap();
        assert_eq!(wal.truncated_on_open(), 0);
        assert_eq!(read_all(&wal).len(), 3);
    }
}

#[test]
fn incomplete_final_record_is_cut_under_both_policies() {
    for config in [WalConfig::new(), strict()] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("torn.wal");
        let lsns = write_log(&path, &[b"one", b"two", b"three", b"the torn one"]);
        // Cut the last record partway through its payload: a crash mid-append.
        let torn_at = lsns[3];
        let cut_to = torn_at + 11;
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(cut_to)
            .unwrap();

        let wal = Wal::open_with(&path, config).unwrap();
        assert_eq!(
            read_all(&wal),
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
        assert_eq!(wal.truncated_on_open(), cut_to - torn_at);
        assert_eq!(wal.len(), torn_at);
        drop(wal);
        assert_eq!(fs::metadata(&path).unwrap().len(), torn_at);
    }
}

#[test]
fn damaged_final_record_is_cut_under_both_policies() {
    for config in [WalConfig::new(), strict()] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last.wal");
        let lsns = write_log(&path, &[b"one", b"two", b"last record"]);
        let full = fs::metadata(&path).unwrap().len();
        flip(&path, lsns[2] + 9, 0x40);

        let wal = Wal::open_with(&path, config).unwrap();
        assert_eq!(read_all(&wal), vec![b"one".to_vec(), b"two".to_vec()]);
        assert_eq!(wal.truncated_on_open(), full - lsns[2]);
    }
}

#[test]
fn stray_bytes_after_the_last_record_are_cut_under_both_policies() {
    for config in [WalConfig::new(), strict()] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stray.wal");
        let _ = write_log(&path, &[b"one"]);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[0u8; 300]).unwrap();
        drop(file);

        let wal = Wal::open_with(&path, config).unwrap();
        assert_eq!(read_all(&wal), vec![b"one".to_vec()]);
        assert_eq!(wal.truncated_on_open(), 300);
    }
}

#[test]
fn strict_refuses_mid_log_payload_damage_and_leaves_the_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rot.wal");
    let lsns = write_log(&path, &[b"one", b"two", b"three", b"four"]);
    flip(&path, lsns[1] + 8, 0x01); // first payload byte of "two"
    let before = fs::read(&path).unwrap();

    assert_corruption_at(Wal::open_with(&path, strict()), lsns[1]);
    assert_eq!(fs::read(&path).unwrap(), before);
    // Refusing is repeatable: a second strict open sees the same bytes.
    assert_corruption_at(Wal::open_with(&path, strict()), lsns[1]);
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn strict_refuses_mid_log_length_damage() {
    // An implausible length (above the maximum) and a plausible wrong one:
    // either way the next record's position is unknown, and the scan must
    // still find the intact records after it.
    for mask in [0x80u8, 0x01] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("len.wal");
        let lsns = write_log(&path, &[b"one", b"two", b"three", b"four"]);
        let config = strict().with_max_record_size(1024);
        flip(&path, lsns[1] + 4 + if mask == 0x80 { 3 } else { 0 }, mask);
        let before = fs::read(&path).unwrap();

        assert_corruption_at(Wal::open_with(&path, config), lsns[1]);
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

#[test]
fn strict_refuses_damage_in_the_checksum_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crc.wal");
    let lsns = write_log(&path, &[b"one", b"two", b"three"]);
    flip(&path, lsns[0], 0xFF);

    assert_corruption_at(Wal::open_with(&path, strict()), 0);
}

#[test]
fn default_policy_still_cuts_mid_log_damage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rot.wal");
    let lsns = write_log(&path, &[b"one", b"two", b"three", b"four"]);
    let full = fs::metadata(&path).unwrap().len();
    flip(&path, lsns[1] + 8, 0x01);

    // The 1.0 behaviour: cut at the damage, intact "three" and "four" included.
    let wal = Wal::open(&path).unwrap();
    assert_eq!(read_all(&wal), vec![b"one".to_vec()]);
    assert_eq!(wal.truncated_on_open(), full - lsns[1]);
    assert_eq!(wal.append(b"next").unwrap().get(), lsns[1]);
    drop(wal);
    assert_eq!(fs::metadata(&path).unwrap().len(), lsns[1] + 12);
}

#[test]
fn strict_refusal_can_be_overridden_by_reopening_with_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("override.wal");
    let lsns = write_log(&path, &[b"one", b"two", b"three"]);
    flip(&path, lsns[1] + 8, 0x10);

    assert!(Wal::open_with(&path, strict()).is_err());
    let wal = Wal::open_with(
        &path,
        WalConfig::new().with_open_policy(OpenPolicy::TruncateTail),
    )
    .unwrap();
    assert_eq!(read_all(&wal), vec![b"one".to_vec()]);
}

#[test]
fn strict_refusal_is_a_fatal_corruption_error() {
    use error_forge::ForgeError;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fatal.wal");
    let lsns = write_log(&path, &[b"one", b"two"]);
    flip(&path, lsns[0] + 8, 0x01);

    let err = Wal::open_with(&path, strict()).unwrap_err();
    assert_eq!(err.kind(), "Corruption");
    assert!(err.is_fatal());
    assert!(
        err.to_string()
            .contains("intact data follows a damaged record")
    );
}

#[test]
fn large_record_after_damage_is_found() {
    // The intact record is far longer than the scan window, so it is checked
    // through prefix checksums rather than read whole.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.wal");
    let big: Vec<u8> = (0..300_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let lsns = write_log(&path, &[b"one", b"two", &big]);
    flip(&path, lsns[1] + 8, 0x01);

    assert_corruption_at(Wal::open_with(&path, strict()), lsns[1]);
}

#[test]
fn segmented_strict_refuses_mid_log_damage_and_leaves_segments_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let records: Vec<Vec<u8>> = (0..40)
        .map(|i| format!("record number {i}").into_bytes())
        .collect();
    let lsns: Vec<u64> = {
        let wal = Wal::open_segmented(dir.path(), 64).unwrap();
        records
            .iter()
            .map(|r| wal.append_and_sync(r).unwrap().get())
            .collect()
    };
    // Damage record 10's payload, in whichever segment holds it.
    let target = lsns[10] + 8;
    let segment = dir.path().join(format!("{:020}.wal", target / 64));
    flip(&segment, target % 64, 0x01);
    let before = segment_files(dir.path());

    assert_corruption_at(Wal::open_segmented_with(dir.path(), 64, strict()), lsns[10]);
    assert_eq!(segment_files(dir.path()), before);

    // The default cuts there, dropping the later segments.
    let wal = Wal::open_segmented(dir.path(), 64).unwrap();
    assert_eq!(read_all(&wal), records[..10].to_vec());
    assert!(wal.truncated_on_open() > 0);
}

#[test]
fn segmented_strict_refuses_a_missing_middle_segment() {
    let dir = tempfile::tempdir().unwrap();
    {
        let wal = Wal::open_segmented(dir.path(), 64).unwrap();
        for i in 0..40 {
            let _ = wal
                .append_and_sync(format!("record number {i}").as_bytes())
                .unwrap();
        }
    }
    let files = segment_files(dir.path());
    assert!(files.len() > 6);
    fs::remove_file(&files[3].0).unwrap();
    let before = segment_files(dir.path());

    let refused = Wal::open_segmented_with(dir.path(), 64, strict());
    assert!(matches!(refused, Err(WalError::Corruption { .. })));
    assert_eq!(segment_files(dir.path()), before);
}

#[test]
fn segmented_strict_refuses_many_missing_middle_segments() {
    let dir = tempfile::tempdir().unwrap();
    {
        let wal = Wal::open_segmented(dir.path(), 64).unwrap();
        for i in 0..24 {
            let _ = wal
                .append_and_sync(format!("record number {i}").as_bytes())
                .unwrap();
        }
    }
    let files = segment_files(dir.path());
    assert_eq!(files.len(), 9);
    for (path, _) in &files[1..8] {
        fs::remove_file(path).unwrap();
    }
    let before = segment_files(dir.path());

    let refused = Wal::open_segmented_with(dir.path(), 64, strict());
    assert!(matches!(refused, Err(WalError::Corruption { .. })));
    assert_eq!(segment_files(dir.path()), before);
}

#[test]
fn segmented_strict_refuses_a_torn_head_marker_after_prefix_compaction() {
    let dir = tempfile::tempdir().unwrap();
    {
        let wal = Wal::open_segmented(dir.path(), 64).unwrap();
        let mut checkpoint = None;
        for i in 0..24 {
            let lsn = wal
                .append_and_sync(format!("record number {i}").as_bytes())
                .unwrap();
            if i == 16 {
                checkpoint = Some(lsn);
            }
        }
        let _ = wal.truncate_before(checkpoint.unwrap()).unwrap();
    }
    // A torn rewrite of the head marker: the file is left empty, so the head
    // falls back to 0, below the segments that were dropped.
    fs::write(dir.path().join("head"), b"").unwrap();
    let before = segment_files(dir.path());
    assert!(!before.is_empty());

    let refused = Wal::open_segmented_with(dir.path(), 64, strict());
    assert!(matches!(
        refused,
        Err(WalError::Corruption { offset: 0, .. })
    ));
    assert_eq!(segment_files(dir.path()), before);
}

#[test]
fn strict_refuses_a_record_longer_than_the_configured_maximum() {
    // Written with the default 64 MiB limit, reopened with 1 KiB. The long
    // record is intact; only the limit changed. Whether it is the last record
    // or not, strict open must not cut it as a torn tail.
    for long_last in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limit.wal");
        let long = vec![0x5Au8; 4096];
        let records: Vec<&[u8]> = if long_last {
            vec![b"one", b"two", &long]
        } else {
            vec![b"one", &long, b"three"]
        };
        let lsns = write_log(&path, &records);
        let victim = if long_last { lsns[2] } else { lsns[1] };
        let before = fs::read(&path).unwrap();

        let config = strict().with_max_record_size(1024);
        match Wal::open_with(&path, config) {
            Err(WalError::Corruption { offset, reason }) => {
                assert_eq!(offset, victim);
                assert!(reason.contains("max_record_size"), "{reason}");
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
        assert_eq!(fs::read(&path).unwrap(), before);

        // The right limit opens it in full.
        let wal = Wal::open_with(&path, strict()).unwrap();
        assert_eq!(read_all(&wal).len(), 3);
        assert_eq!(wal.truncated_on_open(), 0);
    }
}

#[test]
fn segmented_torn_tail_is_cut_under_strict() {
    let dir = tempfile::tempdir().unwrap();
    let lsns: Vec<u64> = {
        let wal = Wal::open_segmented(dir.path(), 64).unwrap();
        (0..20)
            .map(|i| {
                wal.append_and_sync(format!("record number {i}").as_bytes())
                    .unwrap()
                    .get()
            })
            .collect()
    };
    // Damage the last record.
    let target = lsns[19] + 9;
    let segment = dir.path().join(format!("{:020}.wal", target / 64));
    flip(&segment, target % 64, 0x01);

    let wal = Wal::open_segmented_with(dir.path(), 64, strict()).unwrap();
    assert_eq!(read_all(&wal).len(), 19);
    assert_eq!(wal.len(), lsns[19]);
}

proptest! {
    /// A log cut at any byte offset (what a crash mid-append leaves) opens
    /// identically under both policies.
    #[test]
    fn any_truncation_opens_the_same_under_both_policies(
        records in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..30),
        cut in 0u64..4096,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut.wal");
        let refs: Vec<&[u8]> = records.iter().map(Vec::as_slice).collect();
        let _ = write_log(&path, &refs);
        let full = fs::metadata(&path).unwrap().len();
        let cut_at = cut.min(full);
        OpenOptions::new().write(true).open(&path).unwrap().set_len(cut_at).unwrap();

        let copy = dir.path().join("copy.wal");
        fs::copy(&path, &copy).unwrap();

        let lenient = Wal::open(&path).unwrap();
        let strict_wal = Wal::open_with(&copy, strict()).unwrap();
        prop_assert_eq!(read_all(&lenient), read_all(&strict_wal));
        prop_assert_eq!(lenient.truncated_on_open(), strict_wal.truncated_on_open());
        prop_assert_eq!(lenient.len() + lenient.truncated_on_open(), cut_at);
    }

    /// Any single damaged byte in a record that is not the last one stops a
    /// strict open at that record, with the file unchanged, while the default
    /// open keeps exactly the records before it.
    #[test]
    fn any_damage_before_the_last_record_is_refused(
        records in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 2..20),
        pick in any::<prop::sample::Index>(),
        within in any::<prop::sample::Index>(),
        mask in 1u8..=255,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rot.wal");
        let refs: Vec<&[u8]> = records.iter().map(Vec::as_slice).collect();
        let lsns = write_log(&path, &refs);

        let victim = pick.index(records.len() - 1);
        let framed = 8 + records[victim].len();
        flip(&path, lsns[victim] + within.index(framed) as u64, mask);
        let before = fs::read(&path).unwrap();

        match Wal::open_with(&path, strict()) {
            Err(WalError::Corruption { offset, .. }) => prop_assert_eq!(offset, lsns[victim]),
            other => prop_assert!(false, "expected Corruption, got {:?}", other.map(|_| ())),
        }
        prop_assert_eq!(fs::read(&path).unwrap(), before);

        let lenient = Wal::open(&path).unwrap();
        prop_assert_eq!(read_all(&lenient), records[..victim].to_vec());
    }
}
