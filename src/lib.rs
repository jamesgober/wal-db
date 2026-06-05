//! # wal-db
//!
//! A write-ahead log primitive for Rust storage engines.
//!
//! A write-ahead log (WAL) is the durability substrate every database leans on:
//! a state change is appended to a durable, append-only log *before* it is
//! acknowledged, and that log is the source of truth used to rebuild state after
//! a crash. `wal-db` publishes that primitive as a small, audited, benchmarked
//! crate so the storage engines in the portfolio — `lsm-db`, `txn-db`,
//! `raft-io`, Hive DB — share one well-tested implementation instead of each
//! re-deriving the durability contract and getting it subtly wrong.
//!
//! ## The four-call API
//!
//! The common case is four calls: open, append, sync, iterate.
//!
//! ```
//! use wal_db::Wal;
//!
//! # fn main() -> Result<(), wal_db::WalError> {
//! # let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
//! # let path = dir.path().join("app.wal");
//! // Open (or create) the log.
//! let wal = Wal::open(&path)?;
//!
//! // Append a record; `append` returns once the bytes are in the kernel
//! // page cache. It does not flush the disk. The returned LSN is the record's
//! // byte offset — the first record starts at 0.
//! let lsn = wal.append(b"the first record")?;
//! assert_eq!(lsn.get(), 0);
//!
//! // `sync` is the durability barrier: it returns once every record appended
//! // before it is on stable storage.
//! wal.sync()?;
//!
//! // On restart, replay the log to rebuild state.
//! for entry in wal.iter()? {
//!     let entry = entry?;
//!     assert_eq!(entry.data(), b"the first record");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Concurrency and group commit
//!
//! `Wal` is built for many writers. [`append`](Wal::append) is lock-free: each
//! call reserves its byte range with a single atomic step — that range's start
//! offset *is* the record's [`Lsn`] — then writes its record without blocking
//! the others. Share one `Wal` behind an [`Arc`](std::sync::Arc) and append from
//! every thread.
//!
//! Durability is where threads cooperate. When several call [`sync`](Wal::sync)
//! at once, they coalesce into a single fsync — **group commit** — so the cost
//! of making data durable is amortised across everyone committing together
//! rather than paid N times. [`append_and_sync`](Wal::append_and_sync) does an
//! append and a group-commit-aware sync in one call.
//!
//! ## The durability contract
//!
//! Two operations, two distinct guarantees. Confusing them is the single most
//! common way to lose data with a WAL, so they are kept explicit:
//!
//! - [`Wal::append`] returns when the record is in the operating system's page
//!   cache. A crash *after* `append` but *before* `sync` may lose the record.
//! - [`Wal::sync`] returns only when every previously appended record is on
//!   stable storage and will survive a power loss.
//!
//! The flush is platform-correct on each target, which is not the same call
//! everywhere:
//!
//! | Platform | Durability call |
//! |----------|-----------------|
//! | Linux    | `fdatasync` (via [`std::fs::File::sync_data`]) |
//! | Windows  | `FlushFileBuffers` (via [`std::fs::File::sync_data`]) |
//! | macOS    | `fcntl(F_FULLFSYNC)` — **not** plain `fsync`, which leaves data in the drive's write cache |
//!
//! ## Recovery
//!
//! Every record carries a CRC32C checksum over its own bytes. Recovery walks
//! the log forward and stops at the first record whose checksum fails or whose
//! bytes are incomplete — a torn write from a crash mid-append. Records up to
//! that point are returned; the torn tail is discarded. Recovery never reads a
//! partially written record as if it were complete, and a corrupt length prefix
//! can never trigger an unbounded allocation: lengths are validated against
//! [`WalConfig::max_record_size`] before a single byte of payload is read.
//!
//! ## Backends
//!
//! [`Wal::open`] uses the file-backed [`FileStore`]. Custom backends — in-memory
//! for tests, or an alternative storage layer — implement the [`WalStore`] trait
//! and plug in through [`Wal::with_store`]. An in-memory [`MemStore`] ships for
//! testing and examples.
//!
//! ## Status
//!
//! This is the `0.3` core: lock-free multi-writer append, group commit, and a
//! frozen record format, on top of the platform-correct durability and
//! torn-write recovery from `0.2`. Segment rotation follows in `0.3.1`. The
//! four-call API is stable and will not change shape.

#![deny(warnings)]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(unused_must_use)]
#![deny(unused_results)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::unreachable)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(clippy::missing_safety_doc)]

mod commit;
mod config;
mod error;
mod lsn;
mod record;
mod store;
mod sync;
mod wal;

pub use crate::config::WalConfig;
pub use crate::error::{Result, WalError};
pub use crate::lsn::Lsn;
pub use crate::store::{FileStore, MemStore, WalStore};
pub use crate::wal::{Record, Wal, WalIter};

/// The common imports for working with a log.
///
/// Glob-importing the prelude pulls in the four-call API and the types its
/// methods return, which is enough for the great majority of uses.
///
/// ```
/// use wal_db::prelude::*;
///
/// # fn main() -> Result<()> {
/// # let dir = tempfile::tempdir().map_err(WalError::from)?;
/// # let path = dir.path().join("p.wal");
/// let wal = Wal::open(&path)?;
/// let _lsn: Lsn = wal.append(b"record")?;
/// wal.sync()?;
/// # Ok(())
/// # }
/// ```
pub mod prelude {
    pub use crate::config::WalConfig;
    pub use crate::error::{Result, WalError};
    pub use crate::lsn::Lsn;
    pub use crate::store::WalStore;
    pub use crate::wal::{Record, Wal};
}
