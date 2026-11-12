//! Typed records through `pack-io` 2 (feature `pack-io-2`): a round trip,
//! records written through `pack-io` 1.0 (feature `pack-io`) read back with
//! the same values, and the nesting limit with the option that raises it.

#![cfg(feature = "pack-io-2")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use wal_db::{MemStore, Wal};

mod v2 {
    use wal_db::pack_io2::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    #[pack_io(crate = "wal_db::pack_io2")]
    pub struct Entry {
        pub id: u64,
        pub label: String,
        pub tags: Vec<String>,
        pub score: Option<i32>,
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    #[pack_io(crate = "wal_db::pack_io2")]
    pub struct Node {
        pub depth: u32,
        pub child: Option<Box<Node>>,
    }
}

fn entry(id: u64) -> v2::Entry {
    v2::Entry {
        id,
        label: format!("entry {id}"),
        tags: vec!["a".into(), "b".into()],
        score: Some(-7),
    }
}

#[test]
fn test_append_pack_and_unpack_round_trip() {
    let wal = Wal::with_store(MemStore::new()).unwrap();
    let _ = wal.append_pack(&entry(1)).unwrap();
    let _ = wal.append_pack(&entry(2)).unwrap();
    let back: Vec<v2::Entry> = wal
        .iter()
        .unwrap()
        .map(|r| r.unwrap().unpack().unwrap())
        .collect();
    assert_eq!(back, vec![entry(1), entry(2)]);
}

fn chain(levels: u32) -> v2::Node {
    let mut node = v2::Node {
        depth: 0,
        child: None,
    };
    for depth in 1..levels {
        node = v2::Node {
            depth,
            child: Some(Box::new(node)),
        };
    }
    node
}

#[test]
fn test_nesting_past_the_default_limit_needs_unpack_with() {
    let wal = Wal::with_store(MemStore::new()).unwrap();
    let deep = chain(200);
    // The encoder has no depth limit; the decoder's default is 128.
    let _ = wal.append_pack(&deep).unwrap();
    let record = wal.iter().unwrap().next().unwrap().unwrap();
    assert!(record.unpack::<v2::Node>().is_err());
    let config = wal_db::pack_io2::Config::new().with_max_depth(1024);
    assert_eq!(record.unpack_with::<v2::Node>(config).unwrap(), deep);
}

#[cfg(feature = "pack-io")]
mod v1 {
    use wal_db::pack_io::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    pub struct Entry {
        pub id: u64,
        pub label: String,
        pub tags: Vec<String>,
        pub score: Option<i32>,
    }
}

/// A log written through `pack-io` 1.0 reads back through `pack-io` 2 with
/// the same values, and `pack-io` 2 writes the same bytes for them.
#[cfg(feature = "pack-io")]
#[test]
fn test_records_written_with_pack_io_1_unpack_the_same() {
    let old = Wal::with_store(MemStore::new()).unwrap();
    for id in [0, 1, 300, u64::MAX] {
        let _ = old
            .append_typed(&v1::Entry {
                id,
                label: format!("entry {id}"),
                tags: vec!["a".into(), "b".into()],
                score: Some(-7),
            })
            .unwrap();
    }
    let new = Wal::with_store(MemStore::new()).unwrap();
    for record in old.iter().unwrap() {
        let record = record.unwrap();
        let value: v2::Entry = record.unpack().unwrap();
        let _ = new.append_pack(&value).unwrap();
    }
    let old_bytes: Vec<Vec<u8>> = old
        .iter()
        .unwrap()
        .map(|r| r.unwrap().data().to_vec())
        .collect();
    let new_bytes: Vec<Vec<u8>> = new
        .iter()
        .unwrap()
        .map(|r| r.unwrap().data().to_vec())
        .collect();
    assert_eq!(old_bytes, new_bytes);
    let ids: Vec<u64> = new
        .iter()
        .unwrap()
        .map(|r| r.unwrap().unpack::<v2::Entry>().unwrap().id)
        .collect();
    assert_eq!(ids, vec![0, 1, 300, u64::MAX]);
}
