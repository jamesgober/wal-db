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
  `serial-io` feature in 0.4; group-commit tuning in 0.3. The default feature set
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

[Unreleased]: https://github.com/jamesgober/wal-db/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/jamesgober/wal-db/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/jamesgober/wal-db/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jamesgober/wal-db/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/wal-db/releases/tag/v0.1.0
