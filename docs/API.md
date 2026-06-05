<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>wal-db</b><br>
    <sub><sup>API REFERENCE</sup></sub>
</h1>

<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <span>API</span>
        <span>&nbsp;│&nbsp;</span>
        <a href="../CHANGELOG.md" title="Changelog"><b>CHANGELOG</b></a>
    </sup>
</div>

<br>

> Complete reference for every public item in `wal-db`, with runnable examples.
>
> **Status: API frozen for the 1.x line.** Every item below is stable; the on-disk
> format is frozen (records in 0.3.0, segment layout in 0.3.1). See
> [`docs/ON_DISK_FORMAT.md`](./ON_DISK_FORMAT.md).

<a id="top"></a>

## Table of Contents

- [Overview](#overview)
- [Installation](#installation)
- [Tier 1 — the four-call API](#tier-1--the-four-call-api)
  - [`Wal::open`](#walopen)
  - [`Wal::append`](#walappend)
  - [`Wal::sync`](#walsync)
  - [`Wal::append_and_sync`](#walappend_and_sync)
  - [`Wal::append_typed`](#walappend_typed)
  - [`Wal::iter`](#waliter)
  - [`Wal::iter_from`](#waliter_from)
  - [`Wal::truncate_after`](#waltruncate_after)
  - [`Wal::truncate_before`](#waltruncate_before)
  - [`Wal::len` / `Wal::is_empty`](#wallen--walis_empty)
  - [`Lsn`](#lsn)
  - [`Record`](#record)
  - [`WalIter`](#waliter-type)
- [Tier 2 — configuration](#tier-2--configuration)
  - [`WalConfig`](#walconfig)
  - [`RecoveryPolicy`](#recoverypolicy)
  - [`Wal::open_with`](#walopen_with)
  - [`Wal::open_segmented`](#walopen_segmented)
- [Tier 3 — custom backends](#tier-3--custom-backends)
  - [`WalStore`](#walstore)
  - [`FileStore`](#filestore)
  - [`MemStore`](#memstore)
  - [`SegmentedStore`](#segmentedstore)
  - [`Wal::with_store` / `Wal::with_store_and_config`](#walwith_store--walwith_store_and_config)
- [Errors](#errors)
  - [`WalError`](#walerror)
  - [`Result`](#result)
- [The prelude](#the-prelude)
- [On-disk format](#on-disk-format)
- [Feature flags](#feature-flags)

---

## Overview

`wal-db` exposes a durable, append-only log. The common case is a constructor
plus `append` and `sync`, with `iter` for recovery. Advanced use adds a builder
for configuration and a trait for custom storage backends.

The API is layered:

| Tier | Surface | For |
|------|---------|-----|
| 1 | `Wal::open` / `append` / `sync` / `iter` (+ `append_and_sync`, `append_typed`, `iter_from`, `truncate_after`) | the common case — four calls, no generics to name |
| 2 | `WalConfig`, `RecoveryPolicy`, `Wal::open_with`, `Wal::open_segmented` | tuning record limits, recovery policy, segment rotation |
| 3 | `WalStore`, `FileStore`, `MemStore`, `SegmentedStore`, `Wal::with_store` | custom and segmented storage backends |

Durability is explicit: `append` returns when the record is buffered in the OS
page cache; `sync` returns when it is on stable storage. Recovery is
iterator-based and stops at the first torn or corrupt record.

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Installation

```toml
[dependencies]
wal-db = "0.9"

# Typed records via pack-io:
wal-db = { version = "0.9", features = ["pack-io"] }
```

The default feature set is empty; the crate is standard-library only.

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Tier 1 — the four-call API

Source: `src/wal.rs`

`Wal` is the log. It is generic over its storage backend, `Wal<S = FileStore>`,
so the plain name `Wal` is the file-backed log and nothing in Tier 1 requires
naming a type parameter.

`Wal` is `Send` and `Sync`, and the append path is lock-free: many threads can
share one `Wal` behind an `Arc` and call `append` at once with no global lock.
Each `append` reserves its byte range with a single atomic step — that range's
start offset is the record's [`Lsn`](#lsn) — so reservations never overlap or
reorder. Concurrent [`sync`](#walsync) calls coalesce into one fsync (group
commit).

### `Wal::open`

```rust
pub fn open(path: impl AsRef<Path>) -> Result<Wal<FileStore>>
```

Open the log at `path`, creating the file if it does not exist.

On open the log scans its contents, stops at the first record that is incomplete
or fails its checksum, and truncates that torn tail so the next append lands on a
clean boundary. The usual cause of a torn tail is a crash partway through an
earlier append; that record was never acknowledged durable, so discarding it
loses nothing the caller was promised.

**Parameters**

- `path` — the log file. Anything that is `AsRef<Path>`: a `&str`, `String`,
  `Path`, or `PathBuf`.

**Returns** a ready-to-use `Wal<FileStore>`, or [`WalError::Io`](#walerror) if
the file cannot be opened or scanned (for example a missing parent directory or
insufficient permissions).

**Examples**

Open a fresh log and use it:

```rust
use wal_db::Wal;

let wal = Wal::open("/var/lib/myapp/app.wal")?;
let _lsn = wal.append(b"first record")?;
wal.sync()?;
# Ok::<(), wal_db::WalError>(())
```

Reopen an existing log to recover it — any torn tail is truncated automatically:

```rust
use wal_db::Wal;

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
# let path = dir.path().join("app.wal");
# { let w = Wal::open(&path)?; w.append(b"x")?; w.sync()?; }
let wal = Wal::open(&path)?;
let recovered = wal.iter()?.count();
println!("recovered {recovered} records");
# Ok(())
# }
```

### `Wal::append`

```rust
pub fn append(&self, record: &[u8]) -> Result<Lsn>
```

Append `record` to the log and return the [`Lsn`](#lsn) it was assigned — the
byte offset where the record begins.

Lock-free: the byte range is reserved with one atomic step and the record is
written without blocking other appenders. Returns once the bytes are in the
operating system's page cache. It does **not** flush the disk — call
[`sync`](#walsync) for that. A crash between `append` and `sync` may lose the
record.

**Parameters**

- `record` — the payload bytes. May be empty. Must not exceed the configured
  [`max_record_size`](#walconfig) (64 MiB by default).

**Returns** the assigned `Lsn`, or:

- [`WalError::RecordTooLarge`](#walerror) if the record exceeds the limit. The
  log is unchanged.
- [`WalError::Io`](#walerror) if the write fails. The reserved range becomes a
  permanent gap: the log is durable only up to that point, recovery stops there,
  and later syncs covering it report the truncation.

**Examples**

Append and capture the LSN (a byte offset):

```rust
use wal_db::{MemStore, Wal};

# fn main() -> Result<(), wal_db::WalError> {
let wal = Wal::with_store(MemStore::new())?;
let lsn = wal.append(b"a state change")?;
assert_eq!(lsn.get(), 0);
// The next record sits at the first one's end (8-byte header + 14-byte payload).
let next = wal.append(b"another record")?;
assert_eq!(next.get(), 22);
# Ok(())
# }
```

Handle an oversized record:

```rust
use wal_db::{MemStore, Wal, WalConfig, WalError};

# fn main() -> Result<(), wal_db::WalError> {
let config = WalConfig::new().with_max_record_size(8);
let wal = Wal::with_store_and_config(MemStore::new(), config)?;

match wal.append(b"this is definitely longer than eight bytes") {
    Err(WalError::RecordTooLarge { len, max }) => {
        eprintln!("rejected {len}-byte record (limit {max})");
    }
    other => { other?; }
}
# Ok(())
# }
```

### `Wal::sync`

```rust
pub fn sync(&self) -> Result<()>
```

Make every record appended before this call durable. Returns once the data is on
stable storage, using the platform's true durability barrier — `fdatasync` on
Linux, `FlushFileBuffers` on Windows, `fcntl(F_FULLFSYNC)` on macOS.

This is the only call that survives a power loss, and the expensive one, which is
why it is separate from `append`. Concurrent `sync` calls coalesce into a single
fsync — **group commit** — so the flush cost is shared by everyone committing at
the same moment. Amortise it further by appending several records and syncing
once.

**Returns** `Ok(())`, or [`WalError::Io`](#walerror) if the flush fails, or
[`WalError::Corruption`](#walerror) if an earlier append's write failed and left a
gap that cannot be made durable. A failed sync means the records are **not**
durable; treat it as fatal, not as something to retry blindly.

**Examples**

Append a batch, then a single sync:

```rust
use wal_db::Wal;

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
# let path = dir.path().join("app.wal");
let wal = Wal::open(&path)?;
for i in 0..100u32 {
    wal.append(&i.to_le_bytes())?;
}
wal.sync()?; // one flush makes all 100 durable
# Ok(())
# }
```

### `Wal::append_and_sync`

```rust
pub fn append_and_sync(&self, record: &[u8]) -> Result<Lsn>
```

Append `record` and make it durable in one call, returning its [`Lsn`](#lsn).
Equivalent to [`append`](#walappend) followed by a [`sync`](#walsync) scoped to
this record, but with the sync coalesced into the group commit of any other
threads syncing at the same moment. Use it when every record must be durable
before you proceed and you want group-commit throughput without managing the two
calls yourself.

**Returns** the assigned `Lsn`, or the union of [`append`](#walappend)'s and
[`sync`](#walsync)'s errors.

**Examples**

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
            for i in 0..25 {
                wal.append_and_sync(format!("{t}:{i}").as_bytes()).unwrap();
            }
        })
    })
    .collect();
for w in workers {
    w.join().unwrap();
}
assert_eq!(wal.iter()?.count(), 100);
# Ok(())
# }
```

### `Wal::append_typed`

```rust
// Requires the `pack-io` feature.
pub fn append_typed<T: pack_io::Serialize + ?Sized>(&self, value: &T) -> Result<Lsn>
```

The typed counterpart to [`append`](#walappend): serialise `value` with
`pack-io` and append it as one record, which [`Record::decode`](#record) reads
back. Available with the `pack-io` feature; like `append`, it does not sync. The
`Serialize`/`Deserialize` derives are re-exported as `wal_db::pack_io`, so
consumers do not add the dependency themselves.

**Returns** the assigned `Lsn`, or [`WalError::Encoding`](#walerror) if the value
fails to serialise (otherwise [`append`](#walappend)'s errors).

```rust
use wal_db::{MemStore, Wal};
use wal_db::pack_io::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Entry { key: String, value: u64 }

# fn main() -> Result<(), wal_db::WalError> {
let wal = Wal::with_store(MemStore::new())?;
wal.append_typed(&Entry { key: "balance".into(), value: 100 })?;

let entry: Entry = wal.iter()?.next().unwrap()?.decode()?;
assert_eq!(entry.value, 100);
# Ok(())
# }
```

### `Wal::iter`

```rust
pub fn iter(&self) -> Result<WalIter<'_, S>>
```

Iterate the log from the beginning, yielding each record in append order.

The iterator captures the log's length when it is created, so it walks the
records present at that moment and is unaffected by appends made afterwards. Each
item is a `Result<`[`Record`](#record)`>`: a damaged record (one that fails its
checksum) yields a single [`WalError::Corruption`](#walerror), after which the
iterator stops. In a log opened normally the torn tail has already been
truncated, so iteration simply runs to the end.

**Returns** a [`WalIter`](#waliter-type), or [`WalError::Io`](#walerror) if the
log's length cannot be read to start the scan. Per-record errors arrive as
iterator items.

**Examples**

Replay to rebuild state:

```rust
use wal_db::Wal;

# fn apply(_lsn: wal_db::Lsn, _bytes: &[u8]) {}
# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
# let path = dir.path().join("app.wal");
# { let w = Wal::open(&path)?; w.append(b"x")?; w.sync()?; }
let wal = Wal::open(&path)?;
for entry in wal.iter()? {
    let entry = entry?;
    apply(entry.lsn(), entry.data());
}
# Ok(())
# }
```

Collect every payload:

```rust
use wal_db::{MemStore, Wal};

# fn main() -> Result<(), wal_db::WalError> {
let wal = Wal::with_store(MemStore::new())?;
wal.append(b"one")?;
wal.append(b"two")?;

let payloads: Vec<Vec<u8>> = wal
    .iter()?
    .map(|entry| entry.map(|record| record.into_data()))
    .collect::<Result<_, _>>()?;
assert_eq!(payloads, vec![b"one".to_vec(), b"two".to_vec()]);
# Ok(())
# }
```

Detect corruption explicitly:

```rust
use wal_db::{Wal, WalError};

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
# let path = dir.path().join("app.wal");
# { let w = Wal::open(&path)?; w.append(b"x")?; w.sync()?; }
let wal = Wal::open(&path)?;
for entry in wal.iter()? {
    match entry {
        Ok(record) => { /* use record */ }
        Err(WalError::Corruption { offset, reason }) => {
            eprintln!("log corrupt at byte {offset}: {reason}");
            break;
        }
        Err(e) => return Err(e),
    }
}
# Ok(())
# }
```

### `Wal::iter_from`

```rust
pub fn iter_from(&self, from: Lsn) -> Result<WalIter<'_, S>>
```

Iterate from `from` to the end, skipping the records before it. Because an
[`Lsn`](#lsn) is a byte offset, the seek is O(1) — iteration just starts at
`from` instead of 0. Pass an `Lsn` a previous [`append`](#walappend) or
[`iter`](#waliter) produced (a real record boundary); an `Lsn` that does not land
on one is read as a malformed record and surfaces as
[`WalError::Corruption`](#walerror), and an `Lsn` past the end yields an empty
iterator.

```rust
use wal_db::{MemStore, Wal};

# fn main() -> Result<(), wal_db::WalError> {
let wal = Wal::with_store(MemStore::new())?;
wal.append(b"one")?;
let second = wal.append(b"two")?;
wal.append(b"three")?;

let from_second: Vec<Vec<u8>> = wal
    .iter_from(second)?
    .map(|entry| entry.map(|r| r.into_data()))
    .collect::<Result<_, _>>()?;
assert_eq!(from_second, vec![b"two".to_vec(), b"three".to_vec()]);
# Ok(())
# }
```

### `Wal::truncate_after`

```rust
pub fn truncate_after(&self, lsn: Lsn) -> Result<()>
```

Drop every record after the one at `lsn`, keeping the log up to and including it
— the durable building block of compaction. The record at `lsn` becomes the new
last record, the next append lands right after it, and the truncation is made
durable before returning.

**Exclusive access.** This mutates the log's end, so it must **not** run
concurrently with [`append`](#walappend), [`sync`](#walsync), or another
`truncate_after`. The caller quiesces writers first — the usual case for
compaction, where the engine pauses, truncates, and resumes.

**Returns** `Ok(())`, or [`WalError::Corruption`](#walerror) if `lsn` does not
point at an intact record, or [`WalError::Io`](#walerror) if the truncation or its
sync fails.

```rust
use wal_db::{MemStore, Wal};

# fn main() -> Result<(), wal_db::WalError> {
let wal = Wal::with_store(MemStore::new())?;
wal.append(b"keep me")?;
let last_kept = wal.append(b"and me")?;
wal.append(b"drop me")?;

wal.truncate_after(last_kept)?;
assert_eq!(wal.iter()?.count(), 2);
# Ok(())
# }
```

### `Wal::truncate_before`

```rust
pub fn truncate_before(&self, lsn: Lsn) -> Result<Lsn>
```

Drop the records *before* the one at `lsn` — prefix compaction — and return the
new head [`Lsn`](#lsn), the lowest record still present. The complement of
[`truncate_after`](#waltruncate_after): once a consumer has durably applied
everything up to a checkpoint, the old records can be reclaimed. **Offsets are
preserved** — surviving records keep their LSNs, so [`iter`](#waliter) and
[`iter_from`](#waliter_from) keep working.

Removal is at the storage backend's granularity. A **segmented** log
([`Wal::open_segmented`](#walopen_segmented)) deletes whole leading segment files
and records the new head durably, so a crash recovers from the same boundary; the
returned head may be below `lsn` (at the start of the segment that holds it), and
the segment with the most recent records is never dropped. A **single-file** log
cannot reclaim a prefix without moving the surviving bytes (which would change
their LSNs), so it is left unchanged and the returned head is `Lsn(0)`.

Like [`truncate_after`](#waltruncate_after), this requires **exclusive access** —
no concurrent `append`, `sync`, `iter`, or other truncation — because it removes
files a reader could be holding open.

**Returns** the new head `Lsn`, or [`WalError::Io`](#walerror) if the removal
fails.

```rust
use wal_db::Wal;

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
let wal = Wal::open_segmented(dir.path(), 32)?; // small segments to span several files
for i in 0..10 {
    let _ = wal.append(format!("record {i}").as_bytes())?;
}
let checkpoint = wal.append(b"checkpoint")?;
wal.sync()?;

let head = wal.truncate_before(checkpoint)?;
assert!(head <= checkpoint);
assert!(wal.iter()?.next().unwrap()?.lsn() >= head);
# Ok(())
# }
```

### `Wal::len` / `Wal::is_empty`

```rust
pub fn len(&self) -> u64
pub fn is_empty(&self) -> bool
```

`len` is the logical size of the log in bytes, including per-record framing —
equivalently, the offset at which the next append will land. It is a single
atomic load, so it is infallible. `is_empty` reports whether the log holds no
records.

```rust
use wal_db::{MemStore, Wal};

# fn main() -> Result<(), wal_db::WalError> {
let wal = Wal::with_store(MemStore::new())?;
assert!(wal.is_empty());
wal.append(b"data")?;
assert!(!wal.is_empty());
assert!(wal.len() > 0);
# Ok(())
# }
```

### `Lsn`

Source: `src/lsn.rs`

```rust
pub struct Lsn(/* private */);

impl Lsn {
    pub const fn new(value: u64) -> Self;
    pub const fn get(self) -> u64;
}
```

A log sequence number: a record's **byte offset** in the log, assigned at append
time. LSNs are monotonic and unique but **not consecutive** — the first record is
`Lsn(0)`, and the next sits at its end, larger by the first record's framed size.
Defining the LSN as the offset is what lets the append path reserve with a single
atomic and never reorder. `Lsn` is `Copy`, totally ordered, and `Display`s as its
number. `u64::from(lsn)` and `Lsn::new` convert in each direction.

```rust
use wal_db::Lsn;

let first = Lsn::new(0);
let later = Lsn::new(64); // a later record, at some higher offset
assert!(first < later);
assert_eq!(later.get(), 64);
assert_eq!(u64::from(later), 64);
assert_eq!(first.to_string(), "0");
```

### `Record`

Source: `src/wal.rs`

```rust
pub struct Record { /* private */ }

impl Record {
    pub fn lsn(&self) -> Lsn;
    pub fn data(&self) -> &[u8];
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn into_data(self) -> Vec<u8>;
    pub fn decode<T: pack_io::Deserialize>(&self) -> Result<T>; // pack-io feature
}
```

One record read back during iteration: its [`Lsn`](#lsn) (its byte offset) and
its payload bytes. Yielded by [`Wal::iter`](#waliter). Borrow the payload with
`data`, or take ownership of it without copying via `into_data`. With the
`pack-io` feature, `decode` deserialises the payload into a typed value — the
mirror of [`Wal::append_typed`](#walappend_typed) — returning
[`WalError::Encoding`](#walerror) if the bytes do not match the requested type.

```rust
use wal_db::{MemStore, Wal};

# fn main() -> Result<(), wal_db::WalError> {
let wal = Wal::with_store(MemStore::new())?;
wal.append(b"payload")?;

let record = wal.iter()?.next().unwrap()?;
assert_eq!(record.lsn().get(), 0);
assert_eq!(record.data(), b"payload");
assert_eq!(record.len(), 7);
assert!(!record.is_empty());
let owned: Vec<u8> = record.into_data();
assert_eq!(owned, b"payload");
# Ok(())
# }
```

### `WalIter` (type) {#waliter-type}

Source: `src/wal.rs`

```rust
pub struct WalIter<'a, S: WalStore = FileStore> { /* private */ }

impl<'a, S: WalStore> Iterator for WalIter<'a, S> {
    type Item = Result<Record>;
}
```

The iterator returned by [`Wal::iter`](#waliter). A standard `Iterator`, so it
composes with `map`, `filter`, `collect`, `count`, and the rest. It borrows the
log for its lifetime. A corrupt record yields one `Err` and then the iterator
ends.

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Tier 2 — configuration

### `WalConfig`

Source: `src/config.rs`

```rust
pub struct WalConfig { /* private */ }

impl WalConfig {
    pub const fn new() -> Self;                              // also: Default
    pub const fn with_max_record_size(self, bytes: u32) -> Self;
    pub const fn max_record_size(self) -> u32;
    pub const fn with_recovery_policy(self, policy: RecoveryPolicy) -> Self;
    pub const fn recovery_policy(self) -> RecoveryPolicy;
}
```

A builder for log tunables. Construct with `new` (or `Default`), set parameters
with the `with_*` methods, and pass it to [`Wal::open_with`](#walopen_with) or
[`Wal::with_store_and_config`](#walwith_store--walwith_store_and_config). The
builder shape means new parameters do not break existing call sites.

**`max_record_size`** — the largest record the log will accept, in bytes
(default 64 MiB). [`append`](#walappend) rejects anything larger, and recovery
rejects any on-disk length prefix that claims to be larger *before* reading the
payload. That second use bounds the allocation a corrupt or hostile log can
request.

**`recovery_policy`** — how iteration reacts to a damaged record; see
[`RecoveryPolicy`](#recoverypolicy). Defaults to `StopAtFirstError`.

```rust
use wal_db::{RecoveryPolicy, WalConfig};

let config = WalConfig::new()
    .with_max_record_size(1024 * 1024)
    .with_recovery_policy(RecoveryPolicy::SkipBadRecords);
assert_eq!(config.max_record_size(), 1024 * 1024);
assert_eq!(config.recovery_policy(), RecoveryPolicy::SkipBadRecords);

let default = WalConfig::default();
assert_eq!(default.max_record_size(), 64 * 1024 * 1024);
assert_eq!(default.recovery_policy(), RecoveryPolicy::StopAtFirstError);
```

### `RecoveryPolicy`

Source: `src/config.rs`

```rust
#[non_exhaustive]
pub enum RecoveryPolicy {
    StopAtFirstError,
    SkipBadRecords,
}
```

How [`Wal::iter`](#waliter) reacts to a damaged record. This governs
*iteration*, not the torn-tail truncation [`Wal::open`](#walopen) always performs
to keep the append boundary clean — it matters when a record in the middle of an
already-recovered log is damaged (bit rot, say).

| Variant | Behaviour |
|---------|-----------|
| `StopAtFirstError` (default) | Yield the damaged record as a single [`WalError::Corruption`](#walerror), then end. Right for an append-only log, where a damaged record means everything after it is suspect. |
| `SkipBadRecords` | Yield the damage as a [`WalError::Corruption`](#walerror) — never silently — then resume at the next record. For forensic / partial recovery. Only works while the damaged record's length prefix is intact enough to locate the next one; an unreadable length still stops iteration. |

### `Wal::open_with`

```rust
pub fn open_with(path: impl AsRef<Path>, config: WalConfig) -> Result<Wal<FileStore>>
```

Like [`Wal::open`](#walopen), but applies an explicit [`WalConfig`](#walconfig).

```rust
use wal_db::{Wal, WalConfig};

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
# let path = dir.path().join("app.wal");
let config = WalConfig::new().with_max_record_size(4096);
let wal = Wal::open_with(&path, config)?;
# let _ = wal;
# Ok(())
# }
```

### `Wal::open_segmented`

```rust
pub fn open_segmented(dir: impl AsRef<Path>, segment_size: u64) -> Result<Wal<SegmentedStore>>
pub fn open_segmented_with(dir: impl AsRef<Path>, segment_size: u64, config: WalConfig) -> Result<Wal<SegmentedStore>>
```

Open a log striped across fixed-size segment files in `dir` (created if needed)
instead of a single file — see [`SegmentedStore`](#segmentedstore). The log is
still one continuous byte stream, so everything else behaves identically;
`open_segmented_with` additionally applies a [`WalConfig`](#walconfig).

**Returns** a `Wal<SegmentedStore>`, or [`WalError::Io`](#walerror) if
`segment_size` is zero or the directory cannot be opened or scanned.

```rust
use wal_db::Wal;

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
let wal = Wal::open_segmented(dir.path(), 16 * 1024 * 1024)?; // 16 MiB segments
wal.append(b"record")?;
wal.sync()?;
# Ok(())
# }
```

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Tier 3 — custom backends

### `WalStore`

Source: `src/store.rs`

```rust
pub trait WalStore: Send + Sync {
    fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<()>;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;
    fn truncate(&self, len: u64) -> Result<()>;
    fn sync(&self) -> Result<()>;
    fn len(&self) -> Result<u64>;
    fn is_empty(&self) -> Result<bool> { /* defaults to len() == 0 */ }
    fn head(&self) -> Result<u64> { /* defaults to 0 */ }
    fn truncate_before(&self, offset: u64) -> Result<u64> { /* defaults to a no-op */ }
}
```

The two `head` / `truncate_before` methods have defaults, so existing backends
need not change: `head` reports the lowest offset still present (0 unless a
prefix was dropped), and `truncate_before` reclaims storage below an offset if the
backend can — a single file cannot, a segmented store deletes leading segments.

A byte-addressable, append-only store with an explicit durability barrier. The
log frames records and hands out byte offsets; a `WalStore` just holds the bytes.
Every method takes `&self`, because the multi-writer append path writes from
several threads at once — the store must accept concurrent positioned writes.
Implement it to put a log somewhere other than a file.

**Contract**

- `write_at` writes `bytes` at `offset`, growing the store and zero-filling any
  gap if `offset` is past the current end (so a higher offset written first
  leaves detectable zero bytes, like a sparse file). Concurrent calls to disjoint
  ranges must not corrupt each other.
- `read_at` fills `buf` from `offset`, returning the number of bytes read. A
  short return (fewer than `buf.len()`) means the store ended first — this is how
  recovery detects a torn tail.
- `truncate` discards everything at or after `len`.
- `sync` returns only once every prior write is durable.

**Example** — a minimal in-memory backend (the shipped [`MemStore`](#memstore)
is this, with a lock for `&self` mutation):

```rust
use std::sync::Mutex;
use wal_db::{Result, WalStore};

#[derive(Default)]
struct VecStore { data: Mutex<Vec<u8>> }

impl WalStore for VecStore {
    fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        let (start, end) = (offset as usize, offset as usize + bytes.len());
        let mut data = self.data.lock().unwrap();
        if data.len() < end { data.resize(end, 0); }
        data[start..end].copy_from_slice(bytes);
        Ok(())
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let data = self.data.lock().unwrap();
        let start = offset as usize;
        if start >= data.len() { return Ok(0); }
        let n = (data.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n)
    }
    fn truncate(&self, len: u64) -> Result<()> {
        self.data.lock().unwrap().truncate(len as usize);
        Ok(())
    }
    fn sync(&self) -> Result<()> { Ok(()) }
    fn len(&self) -> Result<u64> { Ok(self.data.lock().unwrap().len() as u64) }
}
```

### `FileStore`

Source: `src/store.rs`

```rust
pub struct FileStore { /* private */ }

impl FileStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self>;
    pub fn path(&self) -> &Path;
}
```

The default file-backed `WalStore`, used by [`Wal::open`](#walopen). All reads
and writes are positioned (`pread`/`pwrite` on Unix, `seek_read`/`seek_write` on
Windows), so a recovery read never disturbs the append position. `sync` issues
the platform's true durability barrier. You rarely construct one directly —
`Wal::open` does it for you — but it is available for advanced composition.

```rust
use wal_db::{FileStore, Wal};

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
# let path = dir.path().join("app.wal");
let store = FileStore::open(&path)?;
assert_eq!(store.path(), path);
let wal = Wal::with_store(store)?;
# let _ = wal;
# Ok(())
# }
```

### `MemStore`

Source: `src/store.rs`

```rust
pub struct MemStore { /* private */ }

impl MemStore {
    pub fn new() -> Self;                          // also: Default
    pub fn with_capacity(capacity: usize) -> Self;
    pub fn from_bytes(bytes: Vec<u8>) -> Self;
}
```

An in-memory `WalStore` backed by a `Vec<u8>` behind a short lock. `sync` is a
no-op — memory has no durable tier — so it is for tests, examples, and
benchmarking the framing path in isolation, not for durability. `from_bytes`
preloads it with an existing log image so [`Wal::with_store`](#walwith_store--walwith_store_and_config)
can recover it; it is `Clone`, so an image can be snapshotted.

```rust
use wal_db::{MemStore, Wal};

# fn main() -> Result<(), wal_db::WalError> {
let wal = Wal::with_store(MemStore::with_capacity(4096))?;
wal.append(b"in memory")?;
assert_eq!(wal.iter()?.count(), 1);
# Ok(())
# }
```

### `SegmentedStore`

Source: `src/segment.rs`

```rust
pub struct SegmentedStore { /* private */ }

impl SegmentedStore {
    pub fn open(dir: impl AsRef<Path>, segment_size: u64) -> Result<Self>;
    pub fn dir(&self) -> &Path;
    pub fn segment_size(&self) -> u64;
}
```

A `WalStore` that stripes one continuous byte space across fixed-size segment
files in a directory (`00000000000000000000.wal`, `…01.wal`, …). A write or read
that crosses a boundary is split across the two files, so records span segments
freely — the same scheme PostgreSQL's WAL uses. Bounded files keep recovery time
bounded and let old, fully superseded segments be archived or pruned. Segments
are created lazily as the log grows, and `sync` flushes only the segments with
unwritten changes.

Because the address space stays contiguous, a `Wal` over a `SegmentedStore`
behaves identically to one over a single file. Use it through
[`Wal::open_segmented`](#walopen_segmented), or construct it directly for
composition:

```rust
use wal_db::{SegmentedStore, Wal};

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
let store = SegmentedStore::open(dir.path(), 1024 * 1024)?; // 1 MiB segments
assert_eq!(store.segment_size(), 1024 * 1024);
let wal = Wal::with_store(store)?;
let big = vec![0u8; 4096]; // larger than nothing here, but a record may span segments
wal.append(&big)?;
wal.sync()?;
# Ok(())
# }
```

The byte-level segment layout is specified in
[`docs/ON_DISK_FORMAT.md`](./ON_DISK_FORMAT.md).

### `Wal::with_store` / `Wal::with_store_and_config`

```rust
pub fn with_store(store: S) -> Result<Wal<S>>
pub fn with_store_and_config(store: S, config: WalConfig) -> Result<Wal<S>>
```

Build a log over any `S: WalStore`, with the default or an explicit
[`WalConfig`](#walconfig). Like `open`, these scan the store's existing contents
and truncate a torn tail, so a backend that persists (or a snapshot reloaded into
a `MemStore`) recovers correctly.

```rust
use wal_db::{MemStore, Wal, WalConfig};

# fn main() -> Result<(), wal_db::WalError> {
let a = Wal::with_store(MemStore::new())?;
let b = Wal::with_store_and_config(
    MemStore::new(),
    WalConfig::new().with_max_record_size(256),
)?;
# let _ = (a, b);
# Ok(())
# }
```

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Errors

### `WalError`

Source: `src/error.rs`

```rust
#[non_exhaustive]
pub enum WalError {
    Io { context: &'static str, source: io::Error },
    RecordTooLarge { len: usize, max: u32 },
    Corruption { offset: u64, reason: &'static str },
    Encoding { detail: String },
}
```

The crate error type. It implements `error_forge::ForgeError` (so it carries the
portfolio's stable `kind` / `is_fatal` metadata) and `std::error::Error`, and it
preserves the underlying `io::Error` through `Error::source` for code that needs
the OS error kind. It is `#[non_exhaustive]`: a `match` over it needs a wildcard
arm.

| Variant | Meaning | What to do |
|---------|---------|------------|
| `Io` | An underlying I/O operation failed; `context` names the operation, `source` is the original `io::Error`. | Inspect `source` for the kind (disk full, permission denied). After an append error, reopen the log. |
| `RecordTooLarge` | The record exceeds [`max_record_size`](#walconfig). The log is unchanged. | Split the payload or raise the limit. |
| `Corruption` | Recovery reached a record that is incomplete or fails its checksum, at byte `offset`. | Everything after `offset` is untrustworthy; stop and investigate. `is_fatal()` returns `true`. |
| `Encoding` | A typed record (the `pack-io` feature) failed to encode or decode; `detail` is the codec error's message. | Check the value or the type being decoded into. |

```rust
use wal_db::WalError;
use std::error::Error;

# fn main() -> Result<(), wal_db::WalError> {
# let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
# let missing = dir.path().join("nope").join("deep").join("app.wal");
// Inspect the source io::Error behind a WalError::Io.
if let Err(err) = wal_db::Wal::open(&missing) {
    if let Some(source) = err.source() {
        eprintln!("underlying cause: {source}");
    }
}
# Ok(())
# }
```

### `Result`

```rust
pub type Result<T, E = WalError> = std::result::Result<T, E>;
```

The crate result alias, so signatures read `Result<Lsn>`, `Result<()>`, and so
on.

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## The prelude

```rust
use wal_db::prelude::*;
```

Re-exports the four-call API and the types its methods return: `Wal`, `Lsn`,
`Record`, `WalConfig`, `RecoveryPolicy`, `WalStore`, `WalError`, and `Result`.
Enough for the great majority of uses.

```rust
use wal_db::prelude::*;

# fn main() -> Result<()> {
# let dir = tempfile::tempdir().map_err(WalError::from)?;
# let path = dir.path().join("app.wal");
let wal = Wal::open(&path)?;
let _lsn: Lsn = wal.append(b"record")?;
wal.sync()?;
# Ok(())
# }
```

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## On-disk format

Each record is a fixed 8-byte header followed by its payload:

```text
+-----------+-----------+----------------------+
| crc32c    | length    | payload              |
| 4 bytes   | 4 bytes   | `length` bytes       |
+-----------+-----------+----------------------+
```

All integers are little-endian, fixed regardless of host byte order. The CRC32C
(Castagnoli) checksum covers the length and the payload — everything after the
checksum field. There is no stored LSN: a record's LSN is its byte offset, which
recovery already knows as it scans. A torn write leaves either too few bytes to
form a record or a payload that no longer matches the checksum; recovery detects
both and stops.

> **Frozen for 1.x.** The full normative specification — the exact CRC
> parameters, the recovery algorithm, and the segment-file layout — is in
> [`docs/ON_DISK_FORMAT.md`](./ON_DISK_FORMAT.md). The record format froze in
> 0.3.0 and the segment layout in 0.3.1.

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

## Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `pack-io` | no | Typed records via [`pack-io`](https://crates.io/crates/pack-io): adds [`Wal::append_typed`](#walappend_typed) and [`Record::decode`](#record), and re-exports `wal_db::pack_io` for the derives. |

The default surface is empty and standard-library only. Feature flags are
additive: enabling `pack-io` only adds API, it never changes the byte-record
behaviour.

<hr>
<br>
<a href="#top">&uarr; <b>TOP</b></a>
<br>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>JAMES GOBER.</strong></sup>
</div>
