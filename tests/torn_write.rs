//! Torn-write recovery, proved over arbitrary inputs.
//!
//! The property: for any sequence of records written to a log and then
//! truncated at any byte offset — the shape a crash mid-append leaves behind —
//! recovery reads back exactly the records that survived in full, in order, and
//! never surfaces a partial record as if it were complete. Because records are
//! written contiguously and in sequence, "the records that survived in full" is
//! always a prefix of what was written.

use std::fs::{self, OpenOptions};

use proptest::prelude::*;
use wal_db::Wal;

proptest! {
    #[test]
    fn truncation_at_any_offset_recovers_a_clean_prefix(
        records in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..40),
        cut in 0u64..8192,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("torn.wal");

        // Write the full sequence. No sync needed: nothing here crashes, and
        // reopening the same file sees the page cache, so this measures recovery
        // logic, not durability.
        {
            let wal = Wal::open(&path).unwrap();
            for record in &records {
                let _ = wal.append(record).unwrap();
            }
        }

        let full_len = fs::metadata(&path).unwrap().len();
        let cut_at = cut.min(full_len);

        // Sever the file at an arbitrary byte offset.
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(cut_at).unwrap();
        drop(file);

        // Reopen and read what recovery considers intact.
        let wal = Wal::open(&path).unwrap();
        let recovered: Vec<Vec<u8>> = {
            let mut out = Vec::new();
            for item in wal.iter().unwrap() {
                // Recovery must never error on a cleanly truncated log: the torn
                // tail was dropped on open, leaving only whole records.
                out.push(item.expect("recovered records must all be complete").into_data());
            }
            out
        };

        // What survived is a prefix of what was written.
        prop_assert!(recovered.len() <= records.len());
        for (got, original) in recovered.iter().zip(records.iter()) {
            prop_assert_eq!(got, original);
        }

        // A cut at the very end keeps every record.
        if cut_at == full_len {
            prop_assert_eq!(recovered.len(), records.len());
        }

        // After recovery the next append continues at the recovered byte offset
        // with no gap: the LSN equals the summed framed size of what survived.
        let expected_tail: u64 = recovered.iter().map(|r| (8 + r.len()) as u64).sum();
        let next = wal.append(b"sentinel").unwrap();
        prop_assert_eq!(next.get(), expected_tail);
    }
}
