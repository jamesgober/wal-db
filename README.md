<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br>
    <b>wal-db</b>
    <br>
    <sub><sup>WRITE-AHEAD LOG PRIMITIVE</sup></sub>
</h1>

<div align="center">
    <a href="https://crates.io/crates/wal-db"><img alt="Crates.io" src="https://img.shields.io/crates/v/wal-db"></a>
    <a href="https://crates.io/crates/wal-db" alt="Download wal-db"><img alt="Crates.io Downloads" src="https://img.shields.io/crates/d/wal-db?color=%230099ff"></a>
    <a href="https://docs.rs/wal-db" title="wal-db Documentation"><img alt="docs.rs" src="https://img.shields.io/docsrs/wal-db"></a>
    <a href="https://github.com/jamesgober/wal-db/actions"><img alt="GitHub CI" src="https://github.com/jamesgober/wal-db/actions/workflows/ci.yml/badge.svg"></a>
    <a href="https://github.com/rust-lang/rfcs/blob/master/text/2495-min-rust-version.md" title="MSRV"><img alt="MSRV" src="https://img.shields.io/badge/MSRV-1.85%2B-blue"></a>
</div>

<br>

<div align="left">
    <p>
        <strong>wal-db</strong> is a <b>write-ahead log primitive</b> for Rust storage engines. It is the durability substrate underneath every database, transaction system, and distributed log in the portfolio — <code>lsm-db</code>, <code>txn-db</code>, <code>raft-io</code>, and Hive DB all build on it. The append path is <b>lock-free</b>, durability is <b>explicit</b> and <b>platform-correct</b> on Linux, macOS, and Windows, recovery is <b>provable</b> from a torn write, and concurrent commits <b>coalesce into a single fsync</b>.
    </p>
    <p>
        A WAL is the workhorse no database can avoid: every state change is appended to a durable log <em>before</em> it is acknowledged, and the log is the source of truth used to rebuild state after a crash. Most Rust databases ship their WAL privately inside the engine; <code>wal-db</code> publishes it as a clean, composable primitive so multiple storage engines (LSM, B-tree, document store) can share a single, well-tested implementation.
    </p>
    <p>
        The common case is four calls — <code>open</code>, <code>append</code>, <code>sync</code>, <code>iter</code>. The core is synchronous; async is left to the consumer, where it belongs.
    </p>
    <br>
    <hr>
    <p>
        <strong>MSRV is 1.85+</strong> (Rust 2024 edition). Lock-free append. Group commit. Explicit fsync. Crash-safe recovery.
    </p>
    <blockquote>
        <strong>Status: pre-1.0, feature-frozen, API frozen.</strong> <code>0.7</code> is the hardening pass — adversarial recovery inputs and injected I/O failures alongside the fuzz harness and loom model checks — and freezes the public API for the 1.x line. The feature set was complete at <code>0.5</code>; <code>0.6</code> measured and optimized it (<a href="./docs/BENCHMARKS.md">benchmarks</a>). See <a href="./CHANGELOG.md"><code>CHANGELOG.md</code></a> for detail.
    </blockquote>
</div>

<hr>
<br>

<h2>What it does</h2>

- **Append-only durable log** of arbitrary byte records
- **Lock-free multi-writer append** — many threads append at once with no global lock
- **Group commit** — concurrent `sync` calls coalesce into one fsync, amortising the durability cost
- **Segment rotation** — optionally stripe the log across bounded segment files for bounded recovery and archival
- **Explicit durability barriers** — `append` is in-memory-fast; `sync` is the durability point
- **Platform-correct flush** — `fdatasync` on Linux, `FlushFileBuffers` on Windows, `fcntl(F_FULLFSYNC)` on macOS
- **Torn-write detection** — a CRC32C checksum per record; recovery stops at the first damaged record
- **Self-healing recovery** — a torn tail from a crash mid-append is truncated on open, leaving a clean boundary
- **Fuzz-hardened recovery** — arbitrary bytes never panic or over-allocate; a continuous `cargo-fuzz` harness proves it
- **Recovery policies** — stop at the first damaged record, or skip past it for forensic partial recovery
- **LSN seeking & truncation** — replay from any LSN (`iter_from`); drop everything after one (`truncate_after`) for compaction
- **Iterator-based replay** — walk the log forward to rebuild state
- **Typed records (optional)** — serialise any value via `pack-io` behind a feature; the byte-record API is unchanged when off
- **Pluggable storage backend** — file-backed by default; injectable for in-memory testing and custom stores

<br>

## The durability contract

Two operations, two distinct guarantees. Confusing them is the single most common way to lose data with a WAL, so `wal-db` keeps them explicit:

- **`append`** returns when the record is in the operating system's page cache. A crash after `append` but before `sync` may lose that record.
- **`sync`** returns only when every record appended before it is on stable storage and will survive a power loss.

That flush is not the same call on every platform, and getting it wrong is silent:

| Platform | Durability call |
|----------|-----------------|
| Linux    | `fdatasync` |
| Windows  | `FlushFileBuffers` |
| macOS    | `fcntl(F_FULLFSYNC)` — **not** plain `fsync`, which leaves data in the drive's write cache |

<br>

## Installation

```toml
[dependencies]
wal-db = "0.4"
```

<br>

## Quick Start

```rust
use wal_db::Wal;

# fn apply(_lsn: wal_db::Lsn, _bytes: &[u8]) -> Result<(), wal_db::WalError> { Ok(()) }
// Open (or create) the log.
let wal = Wal::open("/var/lib/myapp/app.wal")?;

// Append returns once the record is in the OS page cache. It does not flush.
let lsn = wal.append(b"a state change")?;

// Sync is the durability barrier: it returns once the record is on stable storage.
wal.sync()?;

// On restart, replay the log from the start to rebuild state.
for entry in wal.iter()? {
    let entry = entry?;
    apply(entry.lsn(), entry.data())?;
}
```

<br>

## Recovery

Every record carries a CRC32C checksum over its own bytes. On `open`, the log scans forward and stops at the first record that is incomplete or fails its checksum — a torn write left by a crash mid-append — and truncates that tail. The records before it are kept; the next append continues from a clean boundary with no gap in the sequence numbers. A corrupt length prefix can never trigger a wild allocation: lengths are validated against the configured maximum before a single payload byte is read.

```rust
use wal_db::Wal;

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
# let path = dir.path().join("app.wal");
// After a crash, reopening the log truncates any torn tail automatically.
let wal = Wal::open(&path)?;

// Iteration yields a Result per record; a damaged record surfaces once, then ends.
for entry in wal.iter()? {
    match entry {
        Ok(record) => { /* apply record.data() at record.lsn() */ }
        Err(e) => eprintln!("recovery stopped: {e}"),
    }
}
# Ok(())
# }
```

<br>

## Configuration

Tunables live on `WalConfig`, a builder passed to `Wal::open_with`:

```rust
use wal_db::{Wal, WalConfig};

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
# let path = dir.path().join("app.wal");
let config = WalConfig::new().with_max_record_size(1024 * 1024); // cap records at 1 MiB
let wal = Wal::open_with(&path, config)?;
# let _ = wal;
# Ok(())
# }
```

<br>

## Concurrency and group commit

`Wal` is built for many writers. `append` is lock-free: each call reserves its byte range with a single atomic step — that range's start offset *is* the record's LSN — then writes its record without blocking the others. Share one `Wal` behind an `Arc` and append from every thread.

Durability is where threads cooperate. When several call `sync` at once they coalesce into a single fsync — **group commit** — so the cost of making data durable is amortised across everyone committing together rather than paid N times. `append_and_sync` does an append and a group-commit-aware sync in one call:

```rust
use std::sync::Arc;
use std::thread;
use wal_db::{MemStore, Wal};

# fn main() -> Result<(), wal_db::WalError> {
let wal = Arc::new(Wal::with_store(MemStore::new())?);

let workers: Vec<_> = (0..4)
    .map(|t| {
        let wal = Arc::clone(&wal);
        thread::spawn(move || {
            for i in 0..100 {
                // Each thread appends and commits; the fsyncs coalesce.
                wal.append_and_sync(format!("worker {t} record {i}").as_bytes()).unwrap();
            }
        })
    })
    .collect();
for w in workers {
    w.join().unwrap();
}

assert_eq!(wal.iter()?.count(), 400);
# Ok(())
# }
```

> **LSNs are byte offsets.** The LSN returned by `append` is the record's position in the log — monotonic and unique, but not consecutive. The first record is `0`; the next sits at its end. This is what lets the append path reserve with a single atomic and never reorder. See [`docs/ON_DISK_FORMAT.md`](./docs/ON_DISK_FORMAT.md).

<br>

## Custom backends

`Wal::open` uses the file-backed `FileStore`. Any type implementing the `WalStore` trait can stand in — an in-memory store for tests, or an alternative storage layer. The crate ships `MemStore` for the in-memory case:

```rust
use wal_db::{MemStore, Wal};

# fn main() -> Result<(), wal_db::WalError> {
let wal = Wal::with_store(MemStore::new())?;
let lsn = wal.append(b"no filesystem involved")?;
assert_eq!(lsn.get(), 0);
# Ok(())
# }
```

<br>

## Segments

By default a log is a single file. For bounded recovery time and archival, stripe it across fixed-size segment files in a directory instead — `Wal::open_segmented`. The log stays one continuous byte stream; records span segment boundaries freely (the same scheme PostgreSQL uses), so nothing about the API or the records changes:

```rust
use wal_db::Wal;

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
// 16 MiB segments. Old, superseded segment files can be archived or pruned.
let wal = Wal::open_segmented(dir.path(), 16 * 1024 * 1024)?;
wal.append(b"striped across files")?;
wal.sync()?;
# Ok(())
# }
```

<br>

## Typed records

By default a record is bytes. With the `pack-io` feature, a record can be any type that derives `Serialize`/`Deserialize` — `append_typed` writes it, `Record::decode` reads it back. The derives come from the re-exported `wal_db::pack_io`, so no extra dependency is needed.

```toml
[dependencies]
wal-db = { version = "0.4", features = ["pack-io"] }
```

```rust
use wal_db::{MemStore, Wal};
use wal_db::pack_io::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Event { id: u64, name: String }

# fn main() -> Result<(), wal_db::WalError> {
let wal = Wal::with_store(MemStore::new())?;
wal.append_typed(&Event { id: 1, name: "start".into() })?;

let event: Event = wal.iter()?.next().unwrap()?.decode()?;
assert_eq!(event, Event { id: 1, name: "start".into() });
# Ok(())
# }
```

<br>

## Recovery policies

`Wal::open` always truncates a torn tail so the append boundary is clean. For corruption *inside* an already-recovered log — bit rot, say — a `WalConfig` recovery policy controls how iteration reacts:

```rust
use wal_db::{RecoveryPolicy, Wal, WalConfig};

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
# let path = dir.path().join("app.wal");
// Default: stop at the first damaged record. Or skip past it for partial recovery:
let config = WalConfig::new().with_recovery_policy(RecoveryPolicy::SkipBadRecords);
let wal = Wal::open_with(&path, config)?;

for entry in wal.iter()? {
    match entry {
        Ok(record) => { /* use it */ }
        Err(e) => eprintln!("skipped a damaged record: {e}"), // iteration continues
    }
}
# Ok(())
# }
```

<br>

## Seeking and compaction

An LSN is a byte offset, so replaying from a checkpoint is O(1) — `iter_from` starts at the LSN instead of scanning from the beginning. When a consumer has durably applied the log up to some point, `truncate_after` drops everything after that record, the durable building block of compaction:

```rust
use wal_db::Wal;

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
# let path = dir.path().join("app.wal");
let wal = Wal::open(&path)?;
let _ = wal.append(b"applied")?;
let checkpoint = wal.append(b"also applied")?;
let _ = wal.append(b"not yet applied")?;

// Replay only what came at or after the checkpoint.
for entry in wal.iter_from(checkpoint)? { let _ = entry?; }

// Or compact: keep up to the checkpoint, drop the rest (made durable).
wal.truncate_after(checkpoint)?;
# Ok(())
# }
```

<br>

## Async

The core is synchronous on purpose — a WAL's calls map to blocking syscalls (`write`, `fsync`), and a runtime is the consumer's choice, not the library's. From an async context, offload to a blocking pool:

```rust,ignore
let wal = wal.clone(); // Arc<Wal>
let lsn = tokio::task::spawn_blocking(move || wal.append_and_sync(b"record")).await??;
```

<br>

## Performance

Numbers from the criterion suite on the development machine, 256-byte records. They are honest measurements, not marketing — the commit figures are bounded by this machine's fsync latency and scale with faster storage and more writers. Full detail and method in [`docs/BENCHMARKS.md`](./docs/BENCHMARKS.md).

| Benchmark | Result | What it measures |
|-----------|--------|------------------|
| LSN reservation | ~4 ns | the single atomic that allocates an LSN and reserves a byte range |
| `append/single` | ~105 ns | the lock-free hot path: reserve, frame, write one record to memory, no syscall |
| `append/multi` (8, file) | ~160 K/s | file-backed multi-writer append — syscall-bound (one `pwrite` each) |
| `commit/group` (8 writers) | **~1.9× a hand-rolled inline WAL** | concurrent append-and-sync; group commit coalesces the fsyncs |
| `recovery/replay` (10k) | ~215 K records/s | reopen and replay a file-backed log |

A file-backed append is syscall-bound, not lock-bound — the `pwrite` the durability contract requires dominates the negligible commit-watermark lock — so the throughput lever is **group commit**, which beats the inline WAL an engine hand-rolls before it has batching. Run them yourself:

```bash
cargo bench --bench wal_bench   # append, commit, recovery, reservation
cargo bench --bench compare     # wal-db vs a hand-rolled inline WAL
```

<br>

## Examples

| Example | Run | Shows |
|---------|-----|-------|
| [`basic`](./examples/basic.rs) | `cargo run --example basic` | the four-call API: open, append, sync, replay |
| [`recovery`](./examples/recovery.rs) | `cargo run --example recovery` | a simulated torn write and self-healing recovery |
| [`concurrent`](./examples/concurrent.rs) | `cargo run --example concurrent` | many writers, one log, group commit |
| [`typed`](./examples/typed.rs) | `cargo run --example typed --features pack-io` | typed records via `pack-io` |

<br>

## Testing

```bash
cargo test --all-features                       # unit, integration, doc tests
cargo test --test torn_write                    # torn-write recovery property test
cargo test --test durability                    # durability across a real process restart
cargo test --test segmented                     # segment rotation and spanning records
RUSTFLAGS="--cfg loom" cargo test --test loom_wal  # model-checked concurrency
cargo +nightly fuzz run recover                 # fuzz the recovery path
cargo bench --bench wal_bench                    # append and commit throughput
```

The `loom` run model-checks the lock-free append and the group-commit handshake: it explores every meaningful thread interleaving and asserts no overlapping records, no reorder, and at most one fsync per syncer. The `fuzz` run feeds arbitrary bytes to the recovery path and proves it never panics or over-allocates.

<hr>
<br>

## Where It Fits

`wal-db` is the durability substrate. It is consumed by:
- [`lsm-db`](https://github.com/jamesgober/lsm-db) — memtable durability
- [`txn-db`](https://github.com/jamesgober/txn-db) — transaction log
- [`raft-io`](https://github.com/jamesgober/raft-io) — Raft log persistence
- Hive DB — primary write-ahead log

It stays foreign-compatible: usable standalone in any project that needs a durable append-only log.

<br>

## Cross-Platform Support

**Tier 1 Support:**
- Linux (x86_64, aarch64) — `fdatasync`
- macOS (x86_64, Apple Silicon) — `fcntl(F_FULLFSYNC)` for true durability
- Windows (x86_64) — `FlushFileBuffers`

Durability semantics are equivalent across platforms; the CI matrix runs the full suite — including the cross-process durability test — on each.

<br>

## Contributing

Before opening a PR, `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` must be clean. Any change touching the durability path requires a torn-write recovery test and a benchmark.

<br>

<div id="license">
    <h2>License</h2>
    <p>Licensed under either of</p>
    <ul>
        <li><b>Apache License, Version 2.0</b> — see <a href="./LICENSE-APACHE">LICENSE-APACHE</a></li>
        <li><b>MIT License</b> — see <a href="./LICENSE-MIT">LICENSE-MIT</a></li>
    </ul>
    <p>at your option.</p>
</div>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>JAMES GOBER.</strong></sup>
</div>
