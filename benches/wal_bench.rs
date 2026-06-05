//! Throughput benchmarks for the append and commit paths.
//!
//! - `append/single/memstore` — one writer framing a record into memory, the
//!   pure cost of the lock-free hot path with no I/O.
//! - `append/multi/memstore` — many writers appending at once, to show the
//!   lock-free reservation scales rather than serialising.
//! - `commit/single/filestore` — one writer, append plus a durability barrier
//!   each time: the unbatched fsync cost.
//! - `commit/group/filestore` — many writers each append-and-sync, so their
//!   fsyncs coalesce. This is the number that matters for a real workload, and
//!   where group commit earns its place.
//!
//! Run with `cargo bench`. These establish the baselines later optimisation
//! milestones are measured against; a regression beyond the tracked threshold
//! is a blocker.

use std::hint::black_box;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use wal_db::{MemStore, Wal};

/// A representative record payload.
const PAYLOAD: &[u8] = &[0x5A; 256];

fn bench_append_single(c: &mut Criterion) {
    c.bench_function("append/single/memstore", |b| {
        b.iter_batched(
            || Wal::with_store(MemStore::with_capacity(4096)).expect("fresh log"),
            |wal| {
                let _ = wal.append(black_box(PAYLOAD)).expect("append");
                wal
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_append_multi(c: &mut Criterion) {
    const THREADS: usize = 8;
    const OPS_PER_THREAD: usize = 2_000;
    let batch = (THREADS * OPS_PER_THREAD) as u64;

    let mut group = c.benchmark_group("append/multi/memstore");
    group.throughput(Throughput::Elements(batch));
    group.bench_function(format!("{THREADS}x{OPS_PER_THREAD}"), |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                let capacity = THREADS * OPS_PER_THREAD * (PAYLOAD.len() + 8);
                let wal =
                    Arc::new(Wal::with_store(MemStore::with_capacity(capacity)).expect("log"));
                let start = Instant::now();
                thread::scope(|scope| {
                    for _ in 0..THREADS {
                        let wal = Arc::clone(&wal);
                        let _ = scope.spawn(move || {
                            for _ in 0..OPS_PER_THREAD {
                                let _ = wal.append(black_box(PAYLOAD)).expect("append");
                            }
                        });
                    }
                });
                elapsed += start.elapsed();
            }
            elapsed
        });
    });
    group.finish();
}

fn bench_commit_single(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("temp dir");
    let wal = Wal::open(dir.path().join("commit_single.wal")).expect("open log");

    c.bench_function("commit/single/filestore", |b| {
        b.iter(|| {
            let _ = wal
                .append_and_sync(black_box(PAYLOAD))
                .expect("append_and_sync");
        });
    });
}

fn bench_commit_group(c: &mut Criterion) {
    const THREADS: usize = 8;
    const OPS_PER_THREAD: usize = 16;
    let batch = (THREADS * OPS_PER_THREAD) as u64;

    let mut group = c.benchmark_group("commit/group/filestore");
    group.throughput(Throughput::Elements(batch));
    group.bench_function(format!("{THREADS}x{OPS_PER_THREAD}"), |b| {
        b.iter_custom(|iters| {
            let dir = tempfile::tempdir().expect("temp dir");
            let mut elapsed = Duration::ZERO;
            for i in 0..iters {
                let wal = Arc::new(Wal::open(dir.path().join(format!("g{i}.wal"))).expect("open"));
                let start = Instant::now();
                thread::scope(|scope| {
                    for _ in 0..THREADS {
                        let wal = Arc::clone(&wal);
                        let _ = scope.spawn(move || {
                            for _ in 0..OPS_PER_THREAD {
                                let _ = wal.append_and_sync(black_box(PAYLOAD)).expect("commit");
                            }
                        });
                    }
                });
                elapsed += start.elapsed();
            }
            elapsed
        });
    });
    group.finish();
}

fn bench_recovery(c: &mut Criterion) {
    const RECORDS: u64 = 10_000;

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("recover.wal");
    {
        let wal = Wal::open(&path).expect("open log");
        for _ in 0..RECORDS {
            let _ = wal.append(black_box(PAYLOAD)).expect("append");
        }
        wal.sync().expect("sync");
    }

    let mut group = c.benchmark_group("recovery");
    group.throughput(Throughput::Elements(RECORDS));
    group.bench_function("replay/10k", |b| {
        b.iter(|| {
            // Reopen (the recovery scan) and replay every record — the full cost
            // of bringing a log back on startup.
            let wal = Wal::open(&path).expect("reopen log");
            let count = wal.iter().expect("iter").filter(Result::is_ok).count();
            black_box(count);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_append_single,
    bench_append_multi,
    bench_commit_single,
    bench_commit_group,
    bench_recovery
);
criterion_main!(benches);
