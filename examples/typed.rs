//! Typed records with `pack-io` 2.
//!
//! Run it with:
//!
//! ```text
//! cargo run --example typed --features pack-io-2
//! ```
//!
//! With the `pack-io-2` feature, records can be any type that derives
//! `Serialize`/`Deserialize`: `append_pack` serialises a value into one record,
//! and `Record::unpack` reads it back with bounded allocation and nesting, so
//! a log from anywhere can be read safely. The derives come from the
//! re-exported `wal_db::pack_io2`, named in `#[pack_io(crate = ..)]`.

use wal_db::pack_io2::{Deserialize, Serialize};
use wal_db::{MemStore, Wal};

#[derive(Serialize, Deserialize, Debug)]
#[pack_io(crate = "wal_db::pack_io2")]
struct Transfer {
    from: String,
    to: String,
    amount: u64,
}

fn main() -> Result<(), wal_db::WalError> {
    let wal = Wal::with_store(MemStore::new())?;

    let _ = wal.append_pack(&Transfer {
        from: "alice".into(),
        to: "bob".into(),
        amount: 50,
    })?;
    let _ = wal.append_pack(&Transfer {
        from: "bob".into(),
        to: "carol".into(),
        amount: 20,
    })?;

    println!("replaying transfers:");
    for entry in wal.iter()? {
        let transfer: Transfer = entry?.unpack()?;
        println!(
            "  {} -> {}: {}",
            transfer.from, transfer.to, transfer.amount
        );
    }
    Ok(())
}
