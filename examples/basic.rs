//! The four-call API end to end: open, append, sync, recover.
//!
//! Run it with:
//!
//! ```text
//! cargo run --example basic
//! ```
//!
//! It writes a handful of records to a temporary log, makes them durable, then
//! reopens the log and replays them — the same shape an application would use to
//! rebuild its state on startup.

use wal_db::Wal;

fn main() -> Result<(), wal_db::WalError> {
    let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
    let path = dir.path().join("basic.wal");

    // Write phase: append three records and make them durable.
    {
        let wal = Wal::open(&path)?;
        let a = wal.append(b"the quick brown fox")?;
        let b = wal.append(b"jumps over")?;
        let c = wal.append(b"the lazy dog")?;
        wal.sync()?;

        println!("appended records at lsns {a}, {b}, {c}");
    }

    // Recovery phase: reopen and replay every record in order.
    let wal = Wal::open(&path)?;
    println!("replaying {} bytes of log:", wal.len());
    for entry in wal.iter()? {
        let entry = entry?;
        let text = String::from_utf8_lossy(entry.data());
        println!("  lsn {:>2}: {text}", entry.lsn());
    }

    Ok(())
}
