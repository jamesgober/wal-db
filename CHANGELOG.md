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

[Unreleased]: https://github.com/jamesgober/wal-db/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jamesgober/wal-db/releases/tag/v0.1.0
