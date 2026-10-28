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
//! - `recovery/strict_torn_tail` — opening a log whose last record is torn,
//!   leaving a tail of random bytes (a torn record of ciphertext, say), under
//!   each open policy. The strict policy scans the tail for intact records
//!   before cutting it; the default cuts it unread.
//!
//! Run with `cargo bench`. These establish the baselines later optimisation
//! milestones are measured against; a regression beyond the tracked threshold
//! is a blocker.

use std::hint::black_box;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use wal_db::{MemStore, OpenPolicy, Wal, WalConfig};

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

fn bench_reservation(c: &mut Criterion) {
    use std::sync::atomic::{AtomicU64, Ordering};

    // wal-db allocates an LSN and reserves a byte range with exactly this single
    // atomic step; everything else on the append path is framing and the write.
    let tail = AtomicU64::new(0);
    c.bench_function("reservation/fetch_add", |b| {
        b.iter(|| black_box(tail.fetch_add(black_box(264), Ordering::Relaxed)));
    });
}

fn bench_append_multi_filestore(c: &mut Criterion) {
    const THREADS: usize = 8;
    const OPS_PER_THREAD: usize = 2_000;
    let batch = (THREADS * OPS_PER_THREAD) as u64;

    let mut group = c.benchmark_group("append/multi/filestore");
    group.throughput(Throughput::Elements(batch));
    group.bench_function(format!("{THREADS}x{OPS_PER_THREAD}"), |b| {
        let dir = tempfile::tempdir().expect("temp dir");
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for i in 0..iters {
                let wal = Arc::new(Wal::open(dir.path().join(format!("a{i}.wal"))).expect("open"));
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

/// Deterministic random bytes (xorshift).
fn noise(len: usize) -> Vec<u8> {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.extend_from_slice(&seed.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn bench_strict_torn_tail(c: &mut Criterion) {
    let mut group = c.benchmark_group("recovery/strict_torn_tail");
    group.sample_size(10);
    for tail in [64 * 1024, 1024 * 1024, 16 * 1024 * 1024] {
        // 1000 intact records, then a torn record: a header that claims more
        // payload than follows it, and `tail` bytes of random payload.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("torn.wal");
        {
            let wal = Wal::open(&path).expect("open log");
            for _ in 0..1000 {
                let _ = wal.append(PAYLOAD).expect("append");
            }
            wal.sync().expect("sync");
        }
        let mut image = std::fs::read(&path).expect("read log");
        image.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        image.extend_from_slice(&((tail + 1) as u32).to_le_bytes());
        image.extend_from_slice(&noise(tail));

        group.throughput(Throughput::Bytes(tail as u64));
        for (name, policy) in [
            ("truncate_tail", OpenPolicy::TruncateTail),
            ("strict", OpenPolicy::Strict),
        ] {
            let config = WalConfig::new().with_open_policy(policy);
            group.bench_function(format!("{name}/{}KiB", tail / 1024), |b| {
                b.iter_batched(
                    || MemStore::from_bytes(image.clone()),
                    |store| {
                        let wal = Wal::with_store_and_config(store, config).expect("open");
                        black_box(wal.truncated_on_open());
                    },
                    BatchSize::LargeInput,
                );
            });
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_reservation,
    bench_append_single,
    bench_append_multi,
    bench_append_multi_filestore,
    bench_commit_single,
    bench_commit_group,
    bench_recovery,
    bench_strict_torn_tail
);
criterion_main!(benches);
