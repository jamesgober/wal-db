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
