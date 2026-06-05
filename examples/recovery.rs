//! What recovery does with a torn write.
//!
//! Run it with:
//!
//! ```text
//! cargo run --example recovery
//! ```
//!
//! It writes some good records, then scribbles a half-written record onto the
//! end of the file by hand — exactly what a crash partway through an append
//! leaves behind. Reopening the log shows the good records survive untouched
//! while the torn tail is discarded, and that the next append continues from a
//! clean boundary with no gap in the sequence numbers.

use std::fs::OpenOptions;
use std::io::Write;

use wal_db::Wal;

fn main() -> Result<(), wal_db::WalError> {
    let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
    let path = dir.path().join("recovery.wal");

    // Write three good records and sync them.
    {
        let wal = Wal::open(&path)?;
        let _ = wal.append(b"account opened")?;
        let _ = wal.append(b"deposit 100")?;
        let _ = wal.append(b"withdraw 30")?;
        wal.sync()?;
    }

    // Simulate a crash mid-append: append raw bytes that do not form a complete,
    // checksummed record. This is the torn tail.
    {
        let mut raw = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(wal_db::WalError::from)?;
        raw.write_all(b"\x09\x00\x00\x00half a record")
            .map_err(wal_db::WalError::from)?;
        raw.flush().map_err(wal_db::WalError::from)?;
    }

    // Reopen: recovery scans, keeps every intact record, and truncates the tail.
    let wal = Wal::open(&path)?;

    println!("records that survived the crash:");
    for entry in wal.iter()? {
        let entry = entry?;
        println!(
            "  lsn {}: {}",
            entry.lsn(),
            String::from_utf8_lossy(entry.data())
        );
    }

    // The log is clean again — the next append lands right after the last good
    // record, with no sequence-number gap.
    let next = wal.append(b"interest accrued")?;
    println!("appended after recovery at lsn {next}");

    Ok(())
}
