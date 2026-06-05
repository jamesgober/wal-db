//! Baseline benchmarks for the foundation append and sync paths.
//!
//! - `append/memstore` isolates the cost of framing a record and writing it to
//!   an in-memory store — the work the append path does before any I/O.
//! - `append_and_sync/filestore` measures one append followed by a durability
//!   barrier, the realistic per-commit cost when every record is synced.
//!
//! Run with `cargo bench`. These establish the numbers that later optimisation
//! milestones are measured against; a regression beyond the tracked threshold
//! is a blocker, not a footnote.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use wal_db::{MemStore, Wal};

/// A representative record payload.
const PAYLOAD: &[u8] = &[0x5A; 256];

fn bench_append_memstore(c: &mut Criterion) {
    c.bench_function("append/memstore", |b| {
        b.iter_batched(
            || Wal::with_store(MemStore::with_capacity(4096)).expect("fresh in-memory log"),
            |wal| {
                let _ = wal.append(black_box(PAYLOAD)).expect("append succeeds");
                wal
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_append_and_sync_filestore(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("temp dir");
    let wal = Wal::open(dir.path().join("bench.wal")).expect("open file-backed log");

    c.bench_function("append_and_sync/filestore", |b| {
        b.iter(|| {
            let _ = wal.append(black_box(PAYLOAD)).expect("append succeeds");
            wal.sync().expect("sync succeeds");
        });
    });
}

criterion_group!(
    benches,
    bench_append_memstore,
    bench_append_and_sync_filestore
);
criterion_main!(benches);
