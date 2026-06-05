//! Fuzz the recovery path: arbitrary bytes fed to a log must never panic,
//! over-allocate, or read past the input.
//!
//! The store is loaded with the fuzzer's raw bytes and a log is opened over it
//! (which scans and truncates), then iterated to the end. The record-size cap is
//! deliberately small so a crafted length prefix is rejected before any payload
//! allocation — the recovery scan validates every length against it before
//! reading. Both recovery policies are exercised, including the skip path that
//! advances using untrusted lengths.

#![no_main]

use libfuzzer_sys::fuzz_target;
use wal_db::{MemStore, RecoveryPolicy, Wal, WalConfig};

fn drive(data: &[u8], policy: RecoveryPolicy) {
    let config = WalConfig::new().with_max_record_size(1 << 16).with_recovery_policy(policy);
    if let Ok(wal) = Wal::with_store_and_config(MemStore::from_bytes(data.to_vec()), config) {
        if let Ok(iter) = wal.iter() {
            for entry in iter {
                let _ = entry.map(|record| record.into_data());
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    drive(data, RecoveryPolicy::StopAtFirstError);
    drive(data, RecoveryPolicy::SkipBadRecords);
});
