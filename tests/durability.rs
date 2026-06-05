//! Durability across a real process boundary.
//!
//! A round-trip within one process proves the file handle round-trips. This
//! proves something stronger and platform-specific: that once [`Wal::sync`]
//! returns, the records survive the *process* dying — that the durability
//! barrier really reached the operating system and not just a buffer inside the
//! crate. A child process writes and syncs, then exits hard (no destructors, no
//! graceful flush); the parent reopens the file and checks the records are
//! there. On macOS this is what exercises the `F_FULLFSYNC` path; on Linux,
//! `fdatasync`; on Windows, `FlushFileBuffers`.

use std::{
    env,
    process::{Command, exit},
};

use wal_db::Wal;

/// Environment variable that, when set, switches this test into the child
/// writer role and carries the log path to write.
const CHILD_ENV: &str = "WALDB_DURABILITY_CHILD_PATH";

/// The records the child writes and the parent expects back.
const RECORDS: &[&[u8]] = &[b"alpha", b"", b"gamma record", &[0u8; 200]];

#[test]
fn records_survive_a_process_restart() {
    if let Ok(path) = env::var(CHILD_ENV) {
        // Child role: append, sync, then terminate immediately. `exit` runs no
        // destructors, so only what `sync` made durable can survive.
        let wal = Wal::open(&path).expect("child opens the log");
        for record in RECORDS {
            let _ = wal.append(record).expect("child appends");
        }
        wal.sync().expect("child syncs to stable storage");
        exit(0);
    }

    // Parent role: spawn a fresh process to write the log, wait for it, then
    // reopen the file here and verify.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cross_process.wal");

    let exe = env::current_exe().unwrap();
    let status = Command::new(exe)
        .args([
            "records_survive_a_process_restart",
            "--exact",
            "--nocapture",
        ])
        .env(CHILD_ENV, &path)
        .status()
        .unwrap();
    assert!(status.success(), "child writer process exited with failure");

    let wal = Wal::open(&path).unwrap();
    let recovered: Vec<Vec<u8>> = wal
        .iter()
        .unwrap()
        .map(|entry| entry.unwrap().into_data())
        .collect();
    let expected: Vec<Vec<u8>> = RECORDS.iter().map(|record| record.to_vec()).collect();

    assert_eq!(recovered, expected);
}
