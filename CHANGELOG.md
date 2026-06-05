# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added

### Changed

### Fixed

### Security

---

## [0.8.0] - 2026-06-05

A documentation-accuracy, examples, and edge-coverage pass — the polish before
consumer integration. No public API change.

### Added

- `examples/checkpoint.rs` — replay from a checkpoint with `iter_from`, then
  truncate the log back to it with `truncate_after`.
- `tests/edge.rs` — boundary and edge coverage: a record exactly at the size
  limit is accepted and one byte over is rejected, a maximum-size record
  round-trips through recovery, a thousand empty records, alternating empty and
  full records, the on-disk image is byte-for-byte deterministic, and `len` /
  `is_empty` track the log exactly.

### Fixed

- Documentation accuracy: the install snippets in `README.md` and `docs/API.md`,
  and the `docs/API.md` status header (which still read "0.2 foundation"), now
  reflect the current version and the frozen API. The API reference's tier tables
  list the segment and recovery-policy surface that shipped after they were
  written.

### Notes

- An audit of the codebase confirmed the hot path and integrity guarantees are at
  the level the design allows: a ~4 ns lock-free LSN reservation, a syscall-bound
  (not lock-bound) file append, a conservative durable watermark that never
  over-reports (loom-verified), bounded-allocation recovery (fuzz-verified), a
  per-record CRC32C, and a byte-deterministic on-disk format. No issues were
  found that warranted a code change.

---

## [0.7.0] - 2026-06-05

Hardening, and the API freeze. Adversarial recovery inputs and injected I/O
failures join the fuzz harness and loom model checks, and the public surface is
now stable for the 1.x line. No public API change.

### Added

- `tests/hostile.rs` — named adversarial recovery cases: a garbage prefix, an
  implausible length (rejected before any allocation), all-zeros, a garbage tail
  after valid records, a corrupt middle record, and truncation mid-header and
  mid-payload. Each asserts recovery reads all-and-only the intact records and
  never trusts an unverified length or checksum.
- `tests/faults.rs` — injected I/O failures through the `WalStore` seam: a
  disk-full append surfaces the error and leaves the earlier records intact, a
  failed write fail-stops the log so a later sync reports the truncation rather
  than a false durability, and an fsync failure is always reported.

### Changed

- **API frozen.** The public surface will not change in a breaking way before
  2.0. `WalError` and `RecoveryPolicy` are `#[non_exhaustive]`, `WalConfig` is a
  builder, and `WalStore`'s one non-required method has a default, so the surface
  can still grow additively.

### Notes

- Cross-platform durability is re-verified by the cross-process durability test in
  the CI matrix on Linux, macOS, and Windows. The macOS "kernel reports success
  but the device did not flush" bug is avoided structurally by using
  `fcntl(F_FULLFSYNC)`.

---

## [0.6.0] - 2026-06-05

The optimization pass — measurement-driven, with an honest benchmark suite. No
public API change.

### Added

- `benches/compare.rs` — a head-to-head against a hand-rolled inline WAL
  (`Mutex<File>` + fsync per commit). wal-db's group commit is **~1.9× faster**
  for eight concurrent durable committers, from coalescing fsyncs and never taking
  a global lock on the write path.
- A `reservation/fetch_add` microbenchmark (the LSN-allocation primitive, ~4 ns)
  and a file-backed multi-writer append benchmark.
- `docs/BENCHMARKS.md` records the full 0.6 baseline, the comparison, and the
  measurement findings.

### Changed

- The recovery scan no longer allocates a buffer per record — it reuses one —
  cutting recovery-replay time by ~12%.

### Performance notes

- The LSN reservation is a single atomic at **~4 ns**.
- A file-backed append is **syscall-bound** (the `pwrite` the page-cache
  durability contract requires), not lock-bound. The commit-watermark mutex is
  negligible against it, so the append data plane stays lock-free and the
  watermark keeps its short, correct, loom-verified lock — the planned lock-free
  watermark rewrite was deliberately **not** done, because the measurement shows
  it would not move the number and would risk the integrity guarantees.
- O_DIRECT was evaluated and **not** implemented: it requires aligning every
  buffer, offset, and size — which a variable-size record stream violates — and
  does not reduce the per-append syscall or the fsync, so it offers no clear
  benefit for this workload.

---

## [0.5.0] - 2026-06-05

Feature complete. LSN seeking and compaction truncation round out what a storage
engine needs from a WAL, with a recorded benchmark suite. The feature set is now
frozen; remaining milestones are optimization and hardening.

### Added

- **`Wal::iter_from(lsn)`** — replay from any LSN. Because an LSN is a byte
  offset, the seek is O(1): iteration starts at the LSN instead of scanning from
  the beginning.
- **`Wal::truncate_after(lsn)`** — drop every record after the one at `lsn`,
  keeping the log up to and including it, for compaction. The truncation is made
  durable before returning, and works across single-file and segmented logs.
- A `recovery/replay` benchmark, and **`docs/BENCHMARKS.md`** recording baseline
  numbers for the append, commit, group-commit, and recovery paths.

### Changed

- CI now runs `clippy`, `test`, and `doc` on **both** the default and
  `--all-features` configurations (previously only `--all-features`).
- Feature freeze declared. No async wrapper is shipped — the core stays
  synchronous and runtime-agnostic by design; the docs show the one-line
  `spawn_blocking` pattern for async callers.

### Fixed

- `cargo doc` on the default feature set: the always-present `WalError::Encoding`
  variant linked to the `pack-io`-only `Wal::append_typed` / `Record::decode`,
  which broke the doc build without the feature. The links are now plain code
  spans.
- CI fuzz job: pinned the fuzz build to `x86_64-unknown-linux-gnu`. The prebuilt
  `cargo-fuzz` is a musl-static binary and otherwise defaulted the fuzz target to
  musl, where AddressSanitizer cannot link against the statically linked libc.

---

## [0.4.0] - 2026-06-05

Recovery hardening and optional typed records. A continuous fuzz harness proves
the recovery path never panics or over-allocates on arbitrary bytes; a
skip-bad-records policy enables forensic partial recovery; and the `pack-io`
feature lets records be typed values rather than raw bytes. Additive — the
default byte-record API is unchanged.

### Added

- **`pack-io` feature** — typed records. `Wal::append_typed` serialises any
  `pack_io::Serialize` value into a record, and `Record::decode` reads it back as
  a `pack_io::Deserialize` type. The derives are re-exported as `wal_db::pack_io`,
  so consumers need not add the dependency. Off by default; the byte-record API is
  unchanged when it is off.
- **`RecoveryPolicy`** — `StopAtFirstError` (default) or `SkipBadRecords`, set via
  `WalConfig::with_recovery_policy`. `SkipBadRecords` surfaces each damaged record
  as an error and then resumes at the next one, for forensic or partial recovery
  of mid-log corruption. `Wal::open` still truncates a torn tail regardless, to
  keep the append boundary clean.
- **`WalError::Encoding`** — a typed record failed to encode or decode (additive,
  via `#[non_exhaustive]`).
- **Recovery fuzz harness** (`fuzz/`, `cargo-fuzz`) — arbitrary bytes fed to the
  recovery path never panic, over-allocate, or read past the input. Run with
  `cargo +nightly fuzz run recover`.
- `examples/concurrent.rs` (multi-writer group commit) and `examples/typed.rs`
  (typed records).

### Changed

- CI now runs the `loom` job for real (it previously swallowed failures while no
  loom tests existed) and adds a continuous fuzz job.

---

## [0.3.1] - 2026-06-05

Segment rotation. The log can now be striped across bounded, fixed-size segment
files instead of one growing file, which keeps recovery time bounded and lets old
segments be archived or pruned. Additive and non-breaking: single-file logs are
unchanged, and records, LSNs, and the four-call API are identical.

### Added

- **`SegmentedStore`** — a `WalStore` that maps the log's continuous byte space
  onto fixed-size segment files in a directory. A write or read crossing a
  boundary is split across files, so records span segments freely (the
  PostgreSQL scheme). Segments are created lazily, and `sync` flushes only the
  segments with unwritten changes.
- **`Wal::open_segmented`** and **`Wal::open_segmented_with`** — open a log over a
  directory of segment files.
- `docs/ON_DISK_FORMAT.md` now specifies the segment-file naming and directory
  layout, **frozen for the 1.x line**.

---

## [0.3.0] - 2026-06-05

The concurrency core: lock-free multi-writer append, group commit, and a record
format frozen for the 1.x line. Built for many writers under an `Arc`, with the
durability and torn-write recovery from 0.2 underneath. Segment rotation follows
in 0.3.1.

### Added

- **Lock-free multi-writer append.** Each `append` reserves its byte range with a
  single atomic step and writes its record without a global lock, concurrently
  with other writers. Steady-state framing is allocation-free (a reused
  thread-local buffer).
- **Group commit.** Concurrent `sync` calls coalesce into a single fsync. A short
  mutex+condvar coordinator tracks the contiguous-written watermark and elects one
  fsync leader; the expensive work runs outside the lock.
- `Wal::append_and_sync` — append plus a group-commit-aware sync in one call.
- **Fail-stop data integrity.** A failed record write poisons the log from that
  offset on: recovery stops at the gap, and syncs covering it return
  `WalError::Corruption` rather than silently dropping data.
- `MemStore::from_bytes` — preload an in-memory store with an existing log image.
- `docs/ON_DISK_FORMAT.md` — the normative record-format specification, with the
  exact CRC32C parameters and the recovery algorithm.
- `tests/loom_wal.rs` — model-checked concurrency: the lock-free reservation
  (no overlap, no reorder, no loss) and group commit (at most one fsync per
  syncer, every record durable), verified under `loom`.
- Throughput benchmarks for single- and multi-writer append and for group-commit
  commit rate.

### Changed

- **Breaking — LSNs are byte offsets.** `Lsn` is now a record's byte position in
  the log: monotonic and unique, but no longer consecutive. The first record is
  `0`; the next sits at its end. This is what makes the append path lock-free and
  reorder-free. Code that assumed dense `0, 1, 2, …` LSNs must adjust.
- **Breaking — `WalStore` trait.** Methods now take `&self` (the multi-writer path
  writes concurrently), `append(&mut self, bytes)` became
  `write_at(&self, offset, bytes)`, and the trait requires `Send + Sync`.
- **Breaking — `Wal::len` and `Wal::is_empty`** return `u64` and `bool` directly
  (a single atomic load) instead of `Result`.
- **Breaking — on-disk record format.** The header is now 8 bytes (CRC32C + length)
  instead of 16; the redundant stored LSN is gone, since a record's LSN is its
  offset. Logs written by 0.2 are not readable by 0.3. **This format is frozen for
  the 1.x line.**
- The single-writer append path is faster: the 0.2 internal mutex is gone.

### Notes

- Segment rotation moved to 0.3.1 (recorded in the roadmap). The byte-offset LSN
  design here is forward-compatible with it. The record format is frozen; the
  multi-file segment layout finalizes in 0.3.1.

---

## [0.2.0] - 2026-06-05

The foundation release: a working single-writer write-ahead log with
platform-correct durability and torn-write recovery. The four-call API is in
place and stable; the lock-free multi-writer path, group commit, and the frozen
on-disk format follow in 0.3.

### Added

- `Wal` — the log. The Tier-1 entry points `Wal::open`, `Wal::append`,
  `Wal::sync`, and `Wal::iter` cover the common case in four calls. `Wal` is
  generic over its backend (`Wal<S = FileStore>`), so the plain name is the
  file-backed log and custom backends plug in through `Wal::with_store`.
- `WalStore` — the backend trait: `append`, `read_at`, `truncate`, `sync`,
  `len`. The seam for swapping where a log is kept.
- `FileStore` — the default file backend, using positioned I/O so recovery reads
  never disturb the append position.
- `MemStore` — an in-memory backend for tests, examples, and benchmarking the
  framing path in isolation.
- `WalConfig` — a builder for tunables, starting with `max_record_size`.
- `Lsn` — a dense, monotonic log sequence number, returned by `append`.
- `Record` — a recovered record (its `Lsn` and payload), yielded by iteration.
- `WalIter` — the recovery iterator. It snapshots the log length on creation,
  yields each record in order, and surfaces a corrupt record once before
  stopping.
- `WalError` — the domain error type, implementing `error_forge::ForgeError`
  and preserving the underlying `io::Error` through `Error::source`.
- `prelude` — the common imports for the four-call API.
- Per-record CRC32C (Castagnoli) checksums for torn-write detection, using the
  hardware CRC instruction on x86-64 and aarch64.
- Platform-correct durability: `fdatasync` on Linux, `FlushFileBuffers` on
  Windows, and `fcntl(F_FULLFSYNC)` on macOS — the last because macOS's `fsync`
  does not flush the drive's write cache.
- Recovery that truncates a torn tail on open: the log scans on `open`, stops at
  the first incomplete or failed-checksum record, and trims it so the next
  append lands on a clean boundary with no sequence-number gap.
- Bounded-allocation recovery: an on-disk length prefix is validated against
  `max_record_size` before any payload bytes are read, so a corrupt or hostile
  log cannot force a wild allocation.
- `examples/basic.rs` and `examples/recovery.rs` — runnable demonstrations of
  the four-call API and torn-write recovery.
- `benches/wal_bench.rs` — criterion baselines for the append and append-and-sync
  paths.
- Integration tests for the single-writer round-trip, cross-process durability,
  and a property test proving recovery returns all-and-only complete records
  after truncation at any byte offset.

### Changed

- The crate is now standard-library only. The scaffold's `no_std` posture was
  removed: a file-backed, fsync-driven log is inherently a `std` component, and
  the mandated `error-forge` dependency is itself `std`.
- Dropped the scaffold's placeholder `pack-io` optional dependency and the
  `std` / `batching` feature flags. Typed record framing returns as an additive
  `pack-io` feature in 0.4; group-commit tuning in 0.3. The default feature set
  is empty.

### Notes

- **On-disk format is unstable across 0.x.** It is documented and frozen for the
  1.x line in 0.3, alongside `docs/ON_DISK_FORMAT.md`. Logs written by 0.2 are
  not guaranteed readable by later 0.x releases.

---

## [0.1.0] - 2026-05-29

Initial scaffold and repository bootstrap. No WAL logic yet — this release establishes the structure, tooling, and quality gates the implementation will be built on.

### Added

- `Cargo.toml` with full crate metadata, Rust 2024 edition, MSRV 1.85, dual `Apache-2.0 OR MIT` license, `docs.rs` configuration, perf-tuned release profile.
- Feature flags: `std` (default), `batching`, `pack-io` (optional record framing).
- Dev-dependencies for the test stack: `criterion`, `proptest`, `tempfile`, and `loom` under `cfg(loom)`.
- `README.md` — overview, the "why a shared WAL primitive" positioning, Tier-1 quick start, group-commit example, cross-platform durability notes.
- `docs/API.md` reference skeleton.
- `REPS.md` compliance baseline at the repository root.
- `.github/workflows/ci.yml` — Linux/macOS/Windows CI matrix on stable and MSRV.
- `deny.toml` — cargo-deny license / advisory / source policy.
- `.gitattributes` normalising line endings and excluding development paths from archives.
- `.dev/` AI-editor briefing (`PROMPT.md`, `ROADMAP.md`) — gitignored.

[Unreleased]: https://github.com/jamesgober/wal-db/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/jamesgober/wal-db/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/jamesgober/wal-db/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/jamesgober/wal-db/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/jamesgober/wal-db/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/jamesgober/wal-db/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/jamesgober/wal-db/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/jamesgober/wal-db/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jamesgober/wal-db/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/wal-db/releases/tag/v0.1.0
