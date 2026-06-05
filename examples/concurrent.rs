//! Many writers, one log, group commit.
//!
//! Run it with:
//!
//! ```text
//! cargo run --example concurrent
//! ```
//!
//! Four threads append and commit to the same log at once. Appends are
//! lock-free, and the concurrent `append_and_sync` calls coalesce their fsyncs
//! through group commit — the shape of a real multi-writer workload.

use std::sync::Arc;
use std::thread;

use wal_db::Wal;

fn main() -> Result<(), wal_db::WalError> {
    let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
    let path = dir.path().join("concurrent.wal");

    let wal = Arc::new(Wal::open(&path)?);

    let workers: Vec<_> = (0..4)
        .map(|t| {
            let wal = Arc::clone(&wal);
            thread::spawn(move || -> Result<(), wal_db::WalError> {
                for i in 0..1_000 {
                    let _ = wal.append_and_sync(format!("worker {t} record {i}").as_bytes())?;
                }
                Ok(())
            })
        })
        .collect();

    for worker in workers {
        worker.join().expect("worker thread panicked")?;
    }

    println!(
        "4 workers durably committed {} records",
        wal.iter()?.count()
    );

    // Reopen to prove they survived.
    drop(wal);
    let reopened = Wal::open(&path)?;
    println!(
        "recovered {} records after reopen",
        reopened.iter()?.count()
    );

    Ok(())
}
