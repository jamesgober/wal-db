//! LSN seeking and truncation on real, file-backed logs.

use wal_db::Wal;

#[test]
fn truncate_after_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compaction.wal");

    let keep;
    {
        let wal = Wal::open(&path).unwrap();
        let _ = wal.append(b"alpha").unwrap();
        keep = wal.append(b"bravo").unwrap();
        let _ = wal.append(b"charlie").unwrap();
        let _ = wal.append(b"delta").unwrap();
        wal.truncate_after(keep).unwrap();
    }

    // The truncation was made durable, so reopening sees only the kept records.
    let wal = Wal::open(&path).unwrap();
    let got: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|entry| entry.unwrap().into_data())
        .collect();
    assert_eq!(got, vec![b"alpha".to_vec(), b"bravo".to_vec()]);

    // And appends continue cleanly from the truncated end.
    let _ = wal.append(b"echo").unwrap();
    assert_eq!(wal.iter().unwrap().count(), 3);
}

#[test]
fn iter_from_on_file_backed_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seek.wal");

    let wal = Wal::open(&path).unwrap();
    let _ = wal.append(b"x").unwrap();
    let y = wal.append(b"y").unwrap();
    let _ = wal.append(b"z").unwrap();

    let got: Vec<Vec<u8>> = wal
        .iter_from(y)
        .unwrap()
        .map(|entry| entry.unwrap().into_data())
        .collect();
    assert_eq!(got, vec![b"y".to_vec(), b"z".to_vec()]);
}

#[test]
fn truncate_after_on_segmented_log_removes_segments() {
    let dir = tempfile::tempdir().unwrap();

    let keep;
    {
        // Small segments so 20 records span many files.
        let wal = Wal::open_segmented(dir.path(), 24).unwrap();
        let mut third = None;
        for i in 0..20u32 {
            let lsn = wal.append(format!("record-{i}").as_bytes()).unwrap();
            if i == 2 {
                third = Some(lsn);
            }
        }
        keep = third.unwrap();
        wal.truncate_after(keep).unwrap();
    }

    let wal = Wal::open_segmented(dir.path(), 24).unwrap();
    let got: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|entry| entry.unwrap().into_data())
        .collect();
    assert_eq!(
        got,
        vec![
            b"record-0".to_vec(),
            b"record-1".to_vec(),
            b"record-2".to_vec()
        ]
    );
}

fn wal_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_str()
                .unwrap()
                .ends_with(".wal")
        })
        .count()
}

#[test]
fn truncate_before_drops_leading_segments_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();

    // 24-byte segments + ~12-byte records => records span segment boundaries, so
    // the lowest surviving segment starts mid-record. The durable head marker is
    // what makes recovery resume at the right boundary anyway.
    let mut lsns = Vec::new();
    {
        let wal = Wal::open_segmented(dir.path(), 24).unwrap();
        for i in 0..30u32 {
            lsns.push(wal.append(format!("record-{i:03}").as_bytes()).unwrap());
        }
        wal.sync().unwrap();

        let before = wal_files(dir.path());
        let checkpoint = lsns[15];
        let head = wal.truncate_before(checkpoint).unwrap();
        assert!(head <= checkpoint);

        // Some leading segment files were reclaimed.
        assert!(wal_files(dir.path()) < before);

        // Iteration starts at the head, and everything from the checkpoint on is
        // still present; the early records are gone.
        let surviving: Vec<u64> = wal
            .iter()
            .unwrap()
            .map(|e| e.unwrap().lsn().get())
            .collect();
        assert_eq!(surviving.first().copied(), Some(head.get()));
        for lsn in &lsns[15..] {
            assert!(
                surviving.contains(&lsn.get()),
                "lsn {} should survive",
                lsn.get()
            );
        }
        assert!(!surviving.contains(&lsns[0].get()));
    }

    // Reopen: recovery resumes from the durable head, reading the surviving
    // records back correctly even though the lowest segment starts mid-record.
    let reopened = Wal::open_segmented(dir.path(), 24).unwrap();
    let after_reopen: Vec<Vec<u8>> = reopened
        .iter()
        .unwrap()
        .map(|e| e.unwrap().into_data())
        .collect();
    for i in 15..30u32 {
        let want = format!("record-{i:03}").into_bytes();
        assert!(
            after_reopen.contains(&want),
            "record-{i:03} should survive reopen"
        );
    }
    assert!(!after_reopen.contains(&b"record-000".to_vec()));
}

#[test]
fn truncate_before_on_a_single_file_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("single.wal");
    let wal = Wal::open(&path).unwrap();
    let _ = wal.append(b"a").unwrap();
    let b = wal.append(b"b").unwrap();
    let _ = wal.append(b"c").unwrap();

    // A single file cannot reclaim a prefix without moving the surviving bytes,
    // so it is left unchanged and the head stays at 0.
    let head = wal.truncate_before(b).unwrap();
    assert_eq!(head.get(), 0);
    assert_eq!(wal.iter().unwrap().count(), 3);
}

#[test]
fn appends_continue_after_truncate_before() {
    let dir = tempfile::tempdir().unwrap();
    let wal = Wal::open_segmented(dir.path(), 24).unwrap();

    let mut checkpoint = None;
    for i in 0..15u32 {
        let lsn = wal.append(format!("r{i}").as_bytes()).unwrap();
        if i == 10 {
            checkpoint = Some(lsn);
        }
    }
    wal.sync().unwrap();
    let checkpoint = checkpoint.unwrap();
    let _ = wal.truncate_before(checkpoint).unwrap();

    // Appends continue with monotonically increasing LSNs after compaction.
    let next = wal.append(b"after").unwrap();
    assert!(next > checkpoint);
    let last = wal
        .iter()
        .unwrap()
        .map(|e| e.unwrap().into_data())
        .last()
        .unwrap();
    assert_eq!(last, b"after");
}

proptest::proptest! {
    /// Across random record sets, segment sizes, and truncation points, every
    /// record from the checkpoint onward must read back intact after a reopen —
    /// even though records span segments and the lowest survivor starts mid-file.
    #[test]
    fn truncate_before_never_loses_records_from_the_checkpoint(
        n_records in 5usize..40,
        seg_size in 16u64..80,
        cut_num in 0usize..40,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cut = cut_num % n_records;

        let mut payloads = Vec::new();
        let mut lsns = Vec::new();
        {
            let wal = Wal::open_segmented(dir.path(), seg_size).unwrap();
            for i in 0..n_records {
                let payload = format!("r{i}:{}", "z".repeat(i % 9)).into_bytes();
                lsns.push(wal.append(&payload).unwrap());
                payloads.push(payload);
            }
            wal.sync().unwrap();
            let head = wal.truncate_before(lsns[cut]).unwrap();
            proptest::prop_assert_eq!(head.get(), lsns[cut].get());
        }

        // Reopen and confirm every record at or after the checkpoint survived
        // with its original bytes and LSN.
        let wal = Wal::open_segmented(dir.path(), seg_size).unwrap();
        let survivors: std::collections::HashMap<u64, Vec<u8>> = wal
            .iter()
            .unwrap()
            .map(|e| {
                let r = e.unwrap();
                (r.lsn().get(), r.into_data())
            })
            .collect();
        for i in cut..n_records {
            proptest::prop_assert_eq!(survivors.get(&lsns[i].get()), Some(&payloads[i]));
        }
    }
}
