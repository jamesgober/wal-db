//! Typed records with `pack-io`.
//!
//! Run it with:
//!
//! ```text
//! cargo run --example typed --features pack-io
//! ```
//!
//! With the `pack-io` feature, records can be any type that derives
//! `Serialize`/`Deserialize`: `append_typed` serialises a value into one record,
//! and `Record::decode` reads it back. The derives come from the re-exported
//! `wal_db::pack_io`, so no extra dependency is needed.

use wal_db::pack_io::{Deserialize, Serialize};
use wal_db::{MemStore, Wal};

#[derive(Serialize, Deserialize, Debug)]
struct Transfer {
    from: String,
    to: String,
    amount: u64,
}

fn main() -> Result<(), wal_db::WalError> {
    let wal = Wal::with_store(MemStore::new())?;

    let _ = wal.append_typed(&Transfer {
        from: "alice".into(),
        to: "bob".into(),
        amount: 50,
    })?;
    let _ = wal.append_typed(&Transfer {
        from: "bob".into(),
        to: "carol".into(),
        amount: 20,
    })?;

    println!("replaying transfers:");
    for entry in wal.iter()? {
        let transfer: Transfer = entry?.decode()?;
        println!(
            "  {} -> {}: {}",
            transfer.from, transfer.to, transfer.amount
        );
    }

    Ok(())
}
