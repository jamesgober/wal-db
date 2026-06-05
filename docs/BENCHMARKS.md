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
cargo bench --bench wal_bench
```

Criterion writes full reports (with plots) to `target/criterion/`. Each run also
compares against the last, so a regression beyond the tracked threshold is
visible immediately.

## Baseline — 0.5.0

Measured on a Windows x86_64 development machine, release build, with 256-byte
records. Medians shown; see `target/criterion/` for the full distributions.

| Benchmark | Median | Throughput | What it measures |
|-----------|--------|------------|------------------|
| `append/single` | ~120 ns | ~8 M/s | the lock-free hot path: framing one record into an in-memory store, no I/O |
| `append/multi` (8 writers) | ~3.9 ms / 16k | ~4.1 M appends/s | eight threads appending at once to one in-memory log |
| `commit/single` | ~1.3 ms | ~0.75 K commits/s | one writer, append plus a durability barrier each time (unbatched fsync) |
| `commit/group` (8 writers) | ~37 ms / 128 | ~3.5 K commits/s | eight threads each append-and-sync; fsyncs coalesced by group commit |
| `recovery/replay` (10k records) | ~48 ms | ~209 K records/s | reopen a file-backed log (recovery scan) and replay every record |

### Reading the numbers

- **`append/single` ≈ 120 ns** is the cost of the lock-free reservation plus
  framing a record; no syscalls. The multi-writer figure is lower per-op because
  the in-memory store serialises the actual writes behind its own lock — a
  `FileStore` parallelises the writes themselves.
- **Group commit ≈ 4.6× the single-writer commit rate** here (≈3.5 K vs ≈0.75 K
  commits/s). That multiplier is the whole point of group commit, and it grows
  with more concurrent writers and faster storage, where one fsync amortises over
  more commits.
- **Recovery replays ≈ 209 K records/s**, reading each record once to find the
  valid tail on open and once to hand it back during iteration.

## Method

- Release profile (`opt-level = 3`, fat LTO, one codegen unit).
- `append/single` uses `iter_batched` with a fresh in-memory log per sample, so
  store setup is excluded from the timing.
- The multi-writer benchmarks use `iter_custom` with `std::thread::scope`, timing
  only the concurrent work (the log is built outside the timed region).
- The commit and recovery benchmarks use a real `FileStore` in a temp directory,
  so the durability barrier is a real platform fsync.

<hr>
<br>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>JAMES GOBER.</strong></sup>
</div>
