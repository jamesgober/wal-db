<h1 align="center">
    <img width="90px" height="auto" src="https://raw.githubusercontent.com/jamesgober/jamesgober/main/media/icons/hexagon-3.svg" alt="Triple Hexagon">
    <br><b>CHANGELOG</b>
</h1>
<p>
  All notable changes to <code>wal-db</code> will be documented in this file. The format is based on <a href="https://keepachangelog.com/en/1.1.0/">Keep a Changelog</a>,
  and this project adheres to <a href="https://semver.org/spec/v2.0.0.html/">Semantic Versioning</a>.
</p>

---

## [Unreleased]

### Added

### Changed

### Fixed

### Security

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

[Unreleased]: https://github.com/jamesgober/wal-db/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/jamesgober/wal-db/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/wal-db/releases/tag/v0.1.0
