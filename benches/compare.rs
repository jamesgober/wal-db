//! Honest head-to-head: durable commit throughput, wal-db versus the inline WAL
//! a storage engine hand-rolls before it has group commit.
//!
//! The workload is identical for every contender — eight threads each commit a
//! batch of 256-byte records *durably* (each commit reaches stable storage) to a
//! file-backed log. The numbers are dominated by fsync latency on the host, so
//! treat the *ratios* as the signal, not the absolute rates.
//!
//! Contenders:
//!
//! - **wal-db / group commit** — `Wal::append_and_sync` from each thread.
//!   Concurrent commits coalesce into one fsync.
//! - **naive / mutex + fsync** — a `Mutex<File>` with a length-prefixed write and
//!   a `sync_data` on every commit. This is the shape of an inline WAL before
//!   anyone adds batching: every commit takes the global lock and pays its own
//!   fsync.
//!
//! Run with `cargo bench --bench compare`.

use std::fs::{File, OpenOptions};
use std::hint::black_box;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use wal_db::Wal;

const PAYLOAD: &[u8] = &[0x5A; 256];
const THREADS: usize = 8;
const OPS_PER_THREAD: usize = 16;

/// A deliberately naive inline WAL: one global lock, one fsync per commit.
struct NaiveWal {
    inner: Mutex<NaiveInner>,
}

struct NaiveInner {
    file: File,
    offset: u64,
}

impl NaiveWal {
    fn create(path: &std::path::Path) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .expect("open naive wal");
        NaiveWal {
            inner: Mutex::new(NaiveInner { file, offset: 0 }),
        }
    }

    fn commit(&self, payload: &[u8]) {
        let mut guard = self.inner.lock().expect("naive wal lock");
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(payload);
        let offset = guard.offset;
        // Positioned write to mirror wal-db, then the per-commit durability barrier.
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            guard
                .file
                .write_all_at(&frame, offset)
                .expect("naive write");
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut at = offset;
            let mut buf = frame.as_slice();
            while !buf.is_empty() {
                let n = guard.file.seek_write(buf, at).expect("naive write");
                buf = &buf[n..];
                at += n as u64;
            }
        }
        guard.offset += frame.len() as u64;
        guard.file.sync_data().expect("naive fsync");
    }
}

fn run_concurrent(elapsed: &mut Duration, commit: impl Fn() + Sync) {
    let start = Instant::now();
    thread::scope(|scope| {
        for _ in 0..THREADS {
            let commit = &commit;
            let _ = scope.spawn(move || {
                for _ in 0..OPS_PER_THREAD {
                    commit();
                }
            });
        }
    });
    *elapsed += start.elapsed();
}

fn bench_compare(c: &mut Criterion) {
    let batch = (THREADS * OPS_PER_THREAD) as u64;
    let mut group = c.benchmark_group("durable_commit");
    group.throughput(Throughput::Elements(batch));

    group.bench_function("wal-db/group", |b| {
        let dir = tempfile::tempdir().expect("temp dir");
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for i in 0..iters {
                let wal = Arc::new(Wal::open(dir.path().join(format!("w{i}.wal"))).expect("open"));
                run_concurrent(&mut elapsed, || {
                    let _ = wal.append_and_sync(black_box(PAYLOAD)).expect("commit");
                });
            }
            elapsed
        });
    });

    group.bench_function("naive/mutex-fsync", |b| {
        let dir = tempfile::tempdir().expect("temp dir");
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for i in 0..iters {
                let wal = Arc::new(NaiveWal::create(&dir.path().join(format!("n{i}.wal"))));
                run_concurrent(&mut elapsed, || {
                    wal.commit(black_box(PAYLOAD));
                });
            }
            elapsed
        });
    });

    group.finish();
}

criterion_group!(benches, bench_compare);
criterion_main!(benches);
