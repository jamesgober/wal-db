<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>wal-db</b><br>
    <sub><sup>BENCHMARKS</sup></sub>
</h1>

<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <a href="./API.md" title="API Reference"><b>API</b></a>
        <span>&nbsp;│&nbsp;</span>
        <span>BENCHMARKS</span>
    </sup>
</div>

<br>

> Recorded baseline numbers from the `criterion` suite. They are honest
> measurements on one machine, not marketing. The sync and commit figures are
> bounded by the development machine's fsync latency and improve markedly on
> faster storage; the append and recovery figures are CPU- and allocator-bound.
> A populated, honest comparison against other engines is the subject of the 0.6
> optimization milestone.

## Running

```bash
cargo bench --bench wal_bench   # append, commit, recovery, reservation
cargo bench --bench compare     # wal-db vs a hand-rolled inline WAL
```

Criterion writes full reports (with plots) to `target/criterion/`. Each run also
compares against the last, so a regression beyond the tracked threshold is
visible immediately.

## Baseline — 0.6.0

Measured on a Windows x86_64 development machine, release build, with 256-byte
records. Medians shown; see `target/criterion/` for the full distributions.

| Benchmark | Median | Throughput | What it measures |
|-----------|--------|------------|------------------|
| `reservation/fetch_add` | ~4.1 ns | — | the LSN-allocation primitive: the single atomic that reserves a record's byte range |
| `append/single` (memstore) | ~105 ns | — | the hot path: reserve, frame, and write one record into memory, no syscall |
| `append/multi` (8, memstore) | ~3.7 M/s | ~3.7 M appends/s | eight threads appending to one in-memory log (the store's own lock serialises the writes) |
| `append/multi` (8, filestore) | ~160 K/s | ~160 K appends/s | eight threads appending to a file — syscall-bound (one `pwrite` per append) |
| `commit/single` (filestore) | ~0.9 ms | ~0.75 K commits/s | one writer, append plus a durability barrier each time |
| `commit/group` (8, filestore) | ~3.5 K/s | ~3.5 K commits/s | eight threads each append-and-sync; fsyncs coalesced by group commit |
| `recovery/replay` (10k) | ~46 ms | ~215 K records/s | reopen a file-backed log (recovery scan) and replay every record |

### What the numbers say

- **The reservation is ~4 ns** — a single `AtomicU64::fetch_add`. This is the
  whole cost of allocating an LSN and a byte range; everything else on the append
  path is framing and the write itself.
- **A file-backed append is syscall-bound**, not lock-bound: ~6 µs/append under
  eight writers is the `pwrite` the page-cache durability contract requires. The
  commit-watermark mutex (tens of ns) is negligible against it — which is why the
  append data plane is left lock-free and the watermark stays under a short,
  correct, loom-verified lock rather than being rewritten lock-free for a number
  that would not move.
- **Group commit is the throughput lever.** One fsync amortises over every commit
  in flight; the multiplier grows with more writers and faster storage. See the
  head-to-head below.

## Head-to-head — `cargo bench --bench compare`

Eight threads each commit 16 records *durably* (every commit on stable storage),
identical workload for both:

| Contender | Throughput | Relative |
|-----------|------------|----------|
| **wal-db / group commit** | ~3.5 K commits/s | **1.9×** |
| naive `Mutex<File>` + fsync-per-commit | ~1.9 K commits/s | 1.0× |

The naive WAL is the shape an engine hand-rolls before it has group commit: a
global lock and one fsync on every commit. wal-db is ~1.9× faster on this
machine — from coalescing the fsyncs and never taking a global lock on the write
path — and the gap widens with more writers and faster storage, where the
per-commit fsync and lock contention scale worse. These figures are dominated by
the host's fsync latency, so the **ratio** is the signal, not the absolute rate.

> **Not compared:** full embedded databases (sled, redb). They are not WALs —
> a durability primitive against a complete B-tree / LSM engine is not
> apples-to-apples, and the comparison would mislead. The WAL's job is to be the
> fast substrate *under* such systems.

## Method

- Release profile (`opt-level = 3`, fat LTO, one codegen unit).
- `append/single` uses `iter_batched` with a fresh in-memory log per sample, so
  store setup is excluded from the timing.
- The multi-writer and comparison benchmarks use `iter_custom` with
  `std::thread::scope`, timing only the concurrent work; the log is built outside
  the timed region.
- The commit, recovery, and comparison benchmarks use real files in a temp
  directory, so the durability barrier is a real platform fsync.

<hr>
<br>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>JAMES GOBER.</strong></sup>
</div>
