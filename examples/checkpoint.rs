//! Replay from a checkpoint, and truncate back to one.
//!
//! Run it with:
//!
//! ```text
//! cargo run --example checkpoint
//! ```
//!
//! An engine applies WAL records to its state and remembers the last LSN it
//! applied (its checkpoint). Two operations follow from that: replaying only the
//! records after the checkpoint on restart (`iter_from`), and — when records
//! after a point must be discarded, as a Raft log does on a conflict, or to roll
//! back speculative writes — truncating the log back to it (`truncate_after`).

use wal_db::Wal;

fn main() -> Result<(), wal_db::WalError> {
    let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
    let path = dir.path().join("checkpoint.wal");

    let wal = Wal::open(&path)?;
    let _ = wal.append(b"set x = 1")?;
    let checkpoint = wal.append(b"set y = 2")?; // the engine has durably applied through here
    let _ = wal.append(b"set z = 3")?;
    let _ = wal.append(b"set w = 4")?;
    wal.sync()?;

    // Replay only what came after the checkpoint — the work still to apply.
    println!("records after the checkpoint (lsn {checkpoint}):");
    for entry in wal.iter_from(checkpoint)? {
        let entry = entry?;
        // The checkpoint record itself is included; skip it to get strictly later.
        if entry.lsn() > checkpoint {
            println!(
                "  lsn {}: {}",
                entry.lsn(),
                String::from_utf8_lossy(entry.data())
            );
        }
    }

    // Now discard everything after the checkpoint and prove it is durable.
    wal.truncate_after(checkpoint)?;
    drop(wal);

    let reopened = Wal::open(&path)?;
    println!("after truncating back to the checkpoint, the log holds:");
    for entry in reopened.iter()? {
        let entry = entry?;
        println!(
            "  lsn {}: {}",
            entry.lsn(),
            String::from_utf8_lossy(entry.data())
        );
    }

    Ok(())
}
