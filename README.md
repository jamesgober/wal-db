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
        <strong>wal-db</strong> is a <b>write-ahead log primitive</b> for Rust storage engines. It is the durability substrate underneath every database, transaction system, and distributed log in the portfolio - <code>lsm-db</code>, <code>txn-db</code>, <code>raft-io</code>, and Hive DB all build on it. The append path is <b>lock-free</b>, the durability guarantees are <b>explicit</b>, and recovery is <b>provable</b> from a torn write or partial flush.
    </p>
    <p>
        A WAL is the workhorse no database can avoid: every state change is appended to a durable log <em>before</em> it is acknowledged, and the log is the source of truth used to rebuild state after a crash. Most Rust databases ship their WAL privately inside the engine; <code>wal-db</code> publishes it as a clean, composable primitive so multiple storage engines (LSM, B+-tree, document store) can share a single, well-tested implementation.
    </p>
    <p>
        The common-case API is one line - <code>wal.append(&amp;record).await?</code> followed by <code>wal.sync().await?</code> - and that path is the fast path. Group commit, batching, segment rotation, and recovery iteration live in Tier 2.
    </p>
    <br>
    <hr>
    <p>
        <strong>MSRV is 1.85+</strong> (Rust 2024 edition). Lock-free append. Explicit fsync. Crash-safe recovery.
    </p>
    <blockquote>
        <strong>Status: pre-1.0, in active development.</strong> The on-disk format is being designed and frozen across the 0.x series; <code>1.0.0</code> will be the format freeze. See <a href="./CHANGELOG.md"><code>CHANGELOG.md</code></a> for detail.
    </blockquote>
</div>

<hr>
<br>

<h2>What it does</h2>

- **Append-only durable log** of arbitrary byte records
- **Lock-free append path** — multiple writers, one log, no global lock
- **Explicit durability barriers** — `append` is fast; `sync` is the durability point
- **Group commit** — many appends amortise a single fsync
- **Segment rotation** — bounded segment size; old segments archived or pruned
- **Crash-safe recovery** — torn-write detection via checksums; truncate to last good record on replay
- **Iterator-based replay** — walk the log forward from any position to rebuild state
- **Pluggable storage backend** — file-backed by default; injectable for in-memory testing and custom stores


<br>

## Features

- **Append-only** — single forward-only write path; no in-place updates
- **Lock-free** — concurrent appenders coordinate via atomics, not mutexes
- **Group commit** — N concurrent appends → 1 fsync, dramatically higher throughput
- **Segmented** — bounded segment files for rotation, archival, pruning
- **Torn-write detection** — per-record checksums; recovery stops at the last verifiable record
- **Replay iterator** — fast forward iteration for state recovery
- **Pluggable backend** — file, in-memory, or custom storage adapter
- **`serial-io` integration** — optional, for typed record framing

<br>

## Installation

```toml
[dependencies]
wal-db = "0.1"

# With serial-io framing:
wal-db = { version = "0.1", features = ["serial-io"] }
```

<br>

## Quick Start

```rust
use wal_db::Wal;

let wal = Wal::open("/var/lib/myapp/wal")?;

// The 80% case — append a record, then sync for durability.
let lsn = wal.append(b"some_record_bytes").await?;
wal.sync().await?;

// On startup, replay from the beginning to rebuild state.
for record in wal.iter()? {
    let (lsn, bytes) = record?;
    apply(lsn, &bytes)?;
}
```

<br>

## Group Commit

Many concurrent appends batched into one fsync:

```rust
use wal_db::Wal;

let wal = Wal::open("/var/lib/myapp/wal")?;

// N concurrent tasks call append + sync_when_ready;
// the WAL coalesces them into a single fsync.
let lsn = wal.append_and_sync(record).await?;
```

<br>

## Testing

```bash
cargo test --all-features
RUSTFLAGS="--cfg loom" cargo test --test loom_wal
cargo bench --bench wal_bench
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
- ✅ Linux (x86_64, aarch64) — uses `fdatasync` where supported
- ✅ macOS (x86_64, Apple Silicon) — uses `fcntl(F_FULLFSYNC)` for true durability
- ✅ Windows (x86_64) — uses `FlushFileBuffers`

Durability semantics are equivalent across platforms; the CI matrix verifies behavior on each.

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
