# wal-db — API Reference

> Complete reference for every public item in `wal-db`, with examples.
> Format mirrors the portfolio standard.
>
> **Status: pre-1.0.** This document tracks the API surface as it lands across the 0.x series. Sections marked _(planned)_ describe the intended surface.

## Table of Contents

- [Overview](#overview)
- [Tier 1 — the lazy path](#tier-1--the-lazy-path)
  - [`Wal::open`](#walopen) _(planned: 0.2)_
  - [`Wal::append`](#walappend) _(planned: 0.2)_
  - [`Wal::sync`](#walsync) _(planned: 0.2)_
  - [`Wal::iter`](#waliter) _(planned: 0.2)_
- [Tier 2 — the configured path](#tier-2--the-configured-path)
  - [`WalConfig` builder](#walconfig-builder) _(planned: 0.3)_
  - [`Wal::append_and_sync`](#walappend_and_sync) _(planned: 0.3)_
- [Tier 3 — the power path](#tier-3--the-power-path)
  - [`WalStore` trait](#walstore-trait) _(planned: 0.2)_
- [On-disk format](#on-disk-format) _(spec lands at 0.3)_
- [Errors](#errors) _(planned: 0.2)_
- [Feature flags](#feature-flags)

---

## Overview

`wal-db` exposes a durable append-only log. The common case is a constructor plus `append` + `sync`; advanced use is a builder for segment size / sync policy / batching; the full surface is the `WalStore` trait for custom storage backends.

The append path never locks. Durability is explicit — `append` returns when the record is buffered; `sync` returns when the record is durable. Group commit coalesces concurrent syncs into a single fsync. Recovery is iterator-based and stops at the first torn-write.

```rust
use wal_db::Wal;

let wal = Wal::open("/var/lib/myapp/wal")?;
let lsn = wal.append(b"record").await?;
wal.sync().await?;
```

---

## Tier 1 — the lazy path

_Documented in full as the 0.2 foundation release lands. Intended signatures:_

- `Wal::open(path: impl AsRef<Path>) -> Result<Wal>` — open or create a WAL at the given path.
- `Wal::append(&self, record: &[u8]) -> Result<Lsn>` — append a record; returns its log sequence number. **Non-blocking on the hot path**, no fsync.
- `Wal::sync(&self) -> Result<()>` — make all previously-appended records durable. The durability barrier.
- `Wal::iter(&self) -> Result<WalIter>` — iterate records from the start (recovery / replay).
- `Wal::iter_from(&self, lsn: Lsn) -> Result<WalIter>` — iterate from a specific LSN.

---

## Tier 2 — the configured path

_Documented at 0.3 when group commit lands._

- `WalConfig` builder: segment size, sync policy (`SyncOnEvery` / `SyncOnInterval` / `SyncManual`), record checksum, recovery policy.
- `Wal::append_and_sync(&self, record: &[u8]) -> Result<Lsn>` — append + group-commit-aware sync in one call.

---

## Tier 3 — the power path

_The `WalStore` trait — the seam custom storage backends (in-memory, network, alternative file layouts) plug into. Documented as the trait surface stabilises at 0.2._

---

## On-disk format

_Normative byte-level spec ships at 0.3 when the format freezes. Until then, the on-disk format is considered unstable across 0.x._

---

## Errors

_Domain error type built on `error-forge`. Variants documented at 0.2._

---

## Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std`        | yes | Standard library. Off → `no_std` (in-memory backend only). |
| `batching`   | no  | Group commit batching (default-on once stabilised at 0.3). |
| `serial-io`  | no  | Typed record framing via `serial-io`. |

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
