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
        <strong>wal-db</strong> is a <b>write-ahead log primitive</b> for Rust storage engines. It is the durability substrate underneath every database, transaction system, and distributed log in the portfolio — <code>lsm-db</code>, <code>txn-db</code>, <code>raft-io</code>, and Hive DB all build on it. The durability guarantees are <b>explicit</b>, recovery is <b>provable</b> from a torn write, and the flush is <b>platform-correct</b> on Linux, macOS, and Windows.
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
        <strong>MSRV is 1.85+</strong> (Rust 2024 edition). Explicit fsync. Crash-safe recovery. Cross-platform durability.
    </p>
    <blockquote>
        <strong>Status: pre-1.0, in active development.</strong> <code>0.2</code> is the foundation release — a correct single-writer log with platform-correct durability and torn-write recovery. The lock-free multi-writer append path and group commit land in <code>0.3</code>, at which point the on-disk format freezes. See <a href="./CHANGELOG.md"><code>CHANGELOG.md</code></a> for detail.
    </blockquote>
</div>

<hr>
<br>

<h2>What it does</h2>

- **Append-only durable log** of arbitrary byte records
- **Explicit durability barriers** — `append` is in-memory-fast; `sync` is the durability point
- **Platform-correct flush** — `fdatasync` on Linux, `FlushFileBuffers` on Windows, `fcntl(F_FULLFSYNC)` on macOS
- **Torn-write detection** — a CRC32C checksum per record; recovery stops at the first damaged record
- **Self-healing recovery** — a torn tail from a crash mid-append is truncated on open, leaving a clean boundary
- **Iterator-based replay** — walk the log forward to rebuild state
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
wal-db = "0.2"
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

## Performance

Baseline numbers from the criterion suite (`cargo bench`), single-writer, on the development machine. They are honest starting points, not marketing: the append path in `0.2` is serialised through a mutex, and the lock-free, sub-100ns hot path is the subject of the `0.3` and `0.6` milestones.

| Path | Cost | What it measures |
|------|------|------------------|
| `append` (in-memory store) | ~130 ns | framing a 256-byte record and writing it, no I/O |
| `append` + `sync` (file store) | ~1.1 ms | one record made fully durable, dominated by the disk flush |

Run them yourself:

```bash
cargo bench --bench wal_bench
```

<br>

## Examples

| Example | Run | Shows |
|---------|-----|-------|
| [`basic`](./examples/basic.rs) | `cargo run --example basic` | the four-call API: open, append, sync, replay |
| [`recovery`](./examples/recovery.rs) | `cargo run --example recovery` | a simulated torn write and self-healing recovery |

<br>

## Testing

```bash
cargo test --all-features        # unit, integration, doc tests
cargo test --test torn_write     # the torn-write recovery property test
cargo test --test durability     # durability across a real process restart
cargo bench --bench wal_bench    # append and sync baselines
```

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
