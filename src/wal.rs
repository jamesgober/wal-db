//! The log itself: [`Wal`], its recovery iterator [`WalIter`], and the
//! [`Record`] iteration yields.

use std::{fmt, io, sync::atomic::Ordering};

#[cfg(not(loom))]
use std::{cell::RefCell, path::Path};

use crate::{
    commit::Commit,
    config::{RecoveryPolicy, WalConfig},
    error::{Result, WalError},
    lsn::Lsn,
    record::{self, HEADER_LEN},
    store::{FileStore, WalStore},
    sync::AtomicU64,
};

/// A cache-line-aligned wrapper, used to keep the heavily-written reservation
/// counter off the same cache line as the rest of the log's fields so appenders
/// hammering it do not invalidate readers' caches (false sharing).
#[repr(align(64))]
#[derive(Debug)]
struct CacheAligned<T>(T);

/// A durable, append-only log.
///
/// `Wal` is the entry point. The four calls that cover almost every use are
/// [`open`](Wal::open), [`append`](Wal::append), [`sync`](Wal::sync), and
/// [`iter`](Wal::iter). The type parameter `S` is the storage backend and
/// defaults to [`FileStore`], so the plain name `Wal` is the file-backed log;
/// custom backends are supplied through [`with_store`](Wal::with_store).
///
/// A `Wal` is [`Send`] and [`Sync`], and the append path is built for it: many
/// threads can call [`append`](Wal::append) at once with no global lock. Share
/// one behind an [`Arc`](std::sync::Arc) and write from every thread.
///
/// # Concurrency and durability
///
/// Appends are lock-free. Each one reserves its byte range with a single atomic
/// step — the range's start offset *is* the record's [`Lsn`] — frames the record
/// into a reused thread-local buffer, and writes it, all without blocking other
/// appenders. [`sync`](Wal::sync) is the durability barrier; when several
/// threads sync at once they coalesce into a single fsync (group commit), so the
/// cost of making data durable is amortised across everyone committing together.
///
/// `append` returns once the record is in the OS page cache; `sync` returns once
/// it is on stable storage. See the [crate docs](crate) for the full contract.
///
/// # Examples
///
/// ```
/// use wal_db::Wal;
///
/// # fn main() -> Result<(), wal_db::WalError> {
/// # let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
/// # let path = dir.path().join("log.wal");
/// let wal = Wal::open(&path)?;
/// let first = wal.append(b"first")?;
/// let second = wal.append(b"second")?;
/// wal.sync()?;
///
/// // LSNs are byte offsets: the first record starts at 0, the second after it.
/// assert_eq!(first.get(), 0);
/// assert!(second.get() > first.get());
///
/// let read_back: Vec<Vec<u8>> = wal
///     .iter()?
///     .map(|entry| entry.map(|record| record.into_data()))
///     .collect::<Result<_, _>>()?;
/// assert_eq!(read_back, vec![b"first".to_vec(), b"second".to_vec()]);
/// # Ok(())
/// # }
/// ```
pub struct Wal<S = FileStore> {
    /// Next byte offset to reserve. Hammered by every appender, so kept on its
    /// own cache line.
    tail: CacheAligned<AtomicU64>,
    store: S,
    max_record_size: u32,
    recovery_policy: RecoveryPolicy,
    commit: Commit,
}

#[cfg(not(loom))]
impl Wal<FileStore> {
    /// Open the log at `path`, creating it if it does not exist.
    ///
    /// On open the log scans its contents, stops at the first record that is
    /// incomplete or fails its checksum, and truncates that torn tail so the
    /// next append lands on a clean boundary. The common cause of a torn tail is
    /// a crash partway through an earlier append; that record was never
    /// acknowledged durable, so discarding it loses nothing the caller was
    /// promised.
    ///
    /// # Errors
    ///
    /// Returns [`WalError::Io`] if the file cannot be opened or scanned.
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::Wal;
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// # let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
    /// # let path = dir.path().join("log.wal");
    /// let wal = Wal::open(&path)?;
    /// wal.append(b"hello")?;
    /// wal.sync()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, WalConfig::new())
    }

    /// Open the log at `path` with an explicit [`WalConfig`].
    ///
    /// # Errors
    ///
    /// Returns [`WalError::Io`] if the file cannot be opened or scanned.
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::{Wal, WalConfig};
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// # let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
    /// # let path = dir.path().join("log.wal");
    /// let config = WalConfig::new().with_max_record_size(1024);
    /// let wal = Wal::open_with(&path, config)?;
    /// # let _ = wal;
    /// # Ok(())
    /// # }
    /// ```
    pub fn open_with(path: impl AsRef<Path>, config: WalConfig) -> Result<Self> {
        let store = FileStore::open(path)?;
        Self::with_store_and_config(store, config)
    }
}

#[cfg(not(loom))]
impl Wal<crate::segment::SegmentedStore> {
    /// Open a segmented log in directory `dir`, with segments of `segment_size`
    /// bytes, creating the directory if needed.
    ///
    /// The log is one continuous byte stream striped across fixed-size files, so
    /// it behaves exactly like a single-file log — records span segment
    /// boundaries freely — while keeping each file bounded for recovery and
    /// archival. Records larger than a segment simply occupy several.
    ///
    /// # Errors
    ///
    /// Returns [`WalError::Io`] if `segment_size` is zero or the directory cannot
    /// be opened or scanned.
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::Wal;
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// # let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
    /// let wal = Wal::open_segmented(dir.path(), 16 * 1024 * 1024)?; // 16 MiB segments
    /// wal.append(b"record")?;
    /// wal.sync()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn open_segmented(dir: impl AsRef<Path>, segment_size: u64) -> Result<Self> {
        Self::open_segmented_with(dir, segment_size, WalConfig::new())
    }

    /// Open a segmented log with an explicit [`WalConfig`].
    ///
    /// Like [`open_segmented`](Wal::open_segmented), but applies `config` (for
    /// example a tighter [`max_record_size`](WalConfig::max_record_size)).
    ///
    /// # Errors
    ///
    /// Returns [`WalError::Io`] if `segment_size` is zero or the directory cannot
    /// be opened or scanned.
    pub fn open_segmented_with(
        dir: impl AsRef<Path>,
        segment_size: u64,
        config: WalConfig,
    ) -> Result<Self> {
        let store = crate::segment::SegmentedStore::open(dir, segment_size)?;
        Self::with_store_and_config(store, config)
    }
}

impl<S: WalStore> Wal<S> {
    /// Build a log over a custom [`WalStore`], using the default configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if scanning the existing contents of the store fails.
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::{MemStore, Wal};
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// let wal = Wal::with_store(MemStore::new())?;
    /// wal.append(b"record")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_store(store: S) -> Result<Self> {
        Self::with_store_and_config(store, WalConfig::new())
    }

    /// Build a log over a custom [`WalStore`] with an explicit [`WalConfig`].
    ///
    /// # Errors
    ///
    /// Returns an error if scanning the existing contents of the store fails.
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::{MemStore, Wal, WalConfig};
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// let config = WalConfig::new().with_max_record_size(64);
    /// let wal = Wal::with_store_and_config(MemStore::new(), config)?;
    /// # let _ = wal;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_store_and_config(store: S, config: WalConfig) -> Result<Self> {
        let recovered = recover(&store, config.max_record_size())?;
        Ok(Wal {
            tail: CacheAligned(AtomicU64::new(recovered)),
            store,
            max_record_size: config.max_record_size(),
            recovery_policy: config.recovery_policy(),
            commit: Commit::new(recovered),
        })
    }

    /// Append `record` to the log and return the [`Lsn`] it was assigned — the
    /// byte offset where the record begins.
    ///
    /// Lock-free: the byte range is reserved with one atomic step and the record
    /// is written without blocking other appenders. Returns once the bytes are
    /// in the operating system's page cache. It does **not** flush the disk —
    /// call [`sync`](Wal::sync) for that. A crash between `append` and `sync` may
    /// lose the record.
    ///
    /// # Errors
    ///
    /// - [`WalError::RecordTooLarge`] if `record` is larger than the configured
    ///   [`max_record_size`](WalConfig::max_record_size). The log is unchanged.
    /// - [`WalError::Io`] if the write fails. The reserved range becomes a
    ///   permanent gap: the log is durable only up to that point, recovery stops
    ///   there, and later syncs covering it report the truncation.
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::{MemStore, Wal};
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// let wal = Wal::with_store(MemStore::new())?;
    /// let lsn = wal.append(b"some bytes")?;
    /// assert_eq!(lsn.get(), 0);
    /// # Ok(())
    /// # }
    /// ```
    pub fn append(&self, record: &[u8]) -> Result<Lsn> {
        let payload_len = record.len();
        if payload_len > self.max_record_size as usize {
            return Err(WalError::RecordTooLarge {
                len: payload_len,
                max: self.max_record_size,
            });
        }
        let frame_len = record::framed_len(payload_len) as u64;

        // Reserve the byte range. The returned start offset is the LSN, and
        // because it comes from a single atomic it is unique and ordered.
        let start = self.tail.0.fetch_add(frame_len, Ordering::Relaxed);
        let end = match start.checked_add(frame_len) {
            Some(end) => end,
            None => {
                self.commit.mark_failed(start);
                return Err(WalError::io(
                    "reserving a record offset",
                    io::Error::other("log size exceeds u64"),
                ));
            }
        };

        match self.frame_and_write(start, record) {
            Ok(()) => {
                self.commit.mark_written(start, end);
                Ok(Lsn::new(start))
            }
            Err(error) => {
                self.commit.mark_failed(start);
                Err(error)
            }
        }
    }

    /// Make every record appended before this call durable.
    ///
    /// Returns once the data is on stable storage, using the platform's true
    /// durability barrier. Concurrent calls coalesce into a single fsync, so the
    /// flush cost is shared by everyone committing at the same time.
    ///
    /// # Errors
    ///
    /// Returns [`WalError::Io`] if the flush fails, or [`WalError::Corruption`]
    /// if an earlier append's write failed and left a gap that cannot be made
    /// durable. A failed sync means the records are not durable; treat it as
    /// fatal, not as something to retry blindly.
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::Wal;
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// # let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
    /// # let path = dir.path().join("log.wal");
    /// let wal = Wal::open(&path)?;
    /// wal.append(b"durable me")?;
    /// wal.sync()?; // now on stable storage
    /// # Ok(())
    /// # }
    /// ```
    pub fn sync(&self) -> Result<()> {
        let target = self.tail.0.load(Ordering::Acquire);
        if target == 0 {
            return Ok(());
        }
        self.commit.sync_to(&self.store, target)
    }

    /// Append `record` and make it durable in one call, returning its [`Lsn`].
    ///
    /// Equivalent to [`append`](Wal::append) followed by a [`sync`](Wal::sync)
    /// scoped to this record, but with the sync coalesced into the group commit
    /// of any other threads syncing at the same moment. Use it when every record
    /// must be durable before you proceed and you want the group-commit
    /// throughput without managing the two calls yourself.
    ///
    /// # Errors
    ///
    /// The union of [`append`](Wal::append)'s and [`sync`](Wal::sync)'s errors.
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::Wal;
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// # let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
    /// # let path = dir.path().join("log.wal");
    /// let wal = Wal::open(&path)?;
    /// let lsn = wal.append_and_sync(b"committed immediately")?;
    /// # let _ = lsn;
    /// # Ok(())
    /// # }
    /// ```
    pub fn append_and_sync(&self, record: &[u8]) -> Result<Lsn> {
        let lsn = self.append(record)?;
        let end = lsn.get() + record::framed_len(record.len()) as u64;
        self.commit.sync_to(&self.store, end)?;
        Ok(lsn)
    }

    /// Serialise `value` with `pack-io` and append it, returning its [`Lsn`].
    ///
    /// The typed counterpart to [`append`](Wal::append): the value is encoded to
    /// bytes and appended as one record, which [`Record::decode`] reads back.
    /// Available with the `pack-io` feature. Like `append`, it does not sync.
    ///
    /// # Errors
    ///
    /// - [`WalError::Encoding`] if the value fails to serialise.
    /// - Otherwise the errors of [`append`](Wal::append) ([`WalError::RecordTooLarge`],
    ///   [`WalError::Io`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::{MemStore, Wal};
    /// use wal_db::pack_io::{Deserialize, Serialize};
    ///
    /// #[derive(Serialize, Deserialize, PartialEq, Debug)]
    /// struct Entry {
    ///     key: String,
    ///     value: u64,
    /// }
    ///
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// let wal = Wal::with_store(MemStore::new())?;
    /// wal.append_typed(&Entry { key: "balance".into(), value: 100 })?;
    ///
    /// let entry: Entry = wal.iter()?.next().unwrap()?.decode()?;
    /// assert_eq!(entry.value, 100);
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "pack-io")]
    pub fn append_typed<T: pack_io::Serialize + ?Sized>(&self, value: &T) -> Result<Lsn> {
        let bytes = pack_io::encode(value).map_err(WalError::encoding)?;
        self.append(&bytes)
    }

    /// Iterate the log from the beginning, yielding each record in append order.
    ///
    /// The iterator walks the records that are fully written at the moment it is
    /// created — it does not see records still being written by other threads, or
    /// appended afterwards. Each item is a [`Result`]: a damaged record yields a
    /// single [`WalError::Corruption`] and then the iterator stops. In a log
    /// opened normally the torn tail has already been truncated, so iteration
    /// runs cleanly to the end.
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::{MemStore, Wal};
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// let wal = Wal::with_store(MemStore::new())?;
    /// wal.append(b"one")?;
    /// wal.append(b"two")?;
    ///
    /// let mut seen = Vec::new();
    /// for entry in wal.iter()? {
    ///     seen.push(entry?.into_data());
    /// }
    /// assert_eq!(seen, vec![b"one".to_vec(), b"two".to_vec()]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn iter(&self) -> Result<WalIter<'_, S>> {
        let end = self.commit.committed();
        Ok(WalIter {
            wal: self,
            offset: 0,
            end,
            done: false,
            policy: self.recovery_policy,
        })
    }

    /// Iterate from `from` (a record's [`Lsn`]) to the end, skipping the records
    /// before it.
    ///
    /// Because an LSN is a byte offset, seeking is O(1): iteration simply starts
    /// at `from` instead of 0. Pass an [`Lsn`] that a previous
    /// [`append`](Wal::append) or [`iter`](Wal::iter) produced — a real record
    /// boundary. An `Lsn` that does not land on a record boundary will be read as
    /// a malformed record and surface as [`WalError::Corruption`]; an `Lsn` past
    /// the end yields an empty iterator.
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::{MemStore, Wal};
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// let wal = Wal::with_store(MemStore::new())?;
    /// wal.append(b"one")?;
    /// let second = wal.append(b"two")?;
    /// wal.append(b"three")?;
    ///
    /// let from_second: Vec<Vec<u8>> = wal
    ///     .iter_from(second)?
    ///     .map(|entry| entry.map(|r| r.into_data()))
    ///     .collect::<Result<_, _>>()?;
    /// assert_eq!(from_second, vec![b"two".to_vec(), b"three".to_vec()]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn iter_from(&self, from: Lsn) -> Result<WalIter<'_, S>> {
        let end = self.commit.committed();
        Ok(WalIter {
            wal: self,
            offset: from.get().min(end),
            end,
            done: false,
            policy: self.recovery_policy,
        })
    }

    /// Drop every record after the one at `lsn`, keeping the log up to and
    /// including it. For compaction.
    ///
    /// The record at `lsn` becomes the new last record; the next append lands
    /// right after it. The truncation is made durable before returning. `lsn`
    /// must be a real record boundary from a previous [`append`](Wal::append) or
    /// [`iter`](Wal::iter), and the record there must be intact.
    ///
    /// # Exclusive access
    ///
    /// This mutates the log's end, so it must **not** run concurrently with
    /// [`append`](Wal::append), [`sync`](Wal::sync), or another `truncate_after`.
    /// The caller is responsible for quiescing writers first — the usual case for
    /// compaction, where the engine pauses the log, truncates, and resumes.
    ///
    /// # Errors
    ///
    /// - [`WalError::Corruption`] if `lsn` does not point at an intact record.
    /// - [`WalError::Io`] if the truncation or its sync fails.
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::{MemStore, Wal};
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// let wal = Wal::with_store(MemStore::new())?;
    /// wal.append(b"keep me")?;
    /// let last_kept = wal.append(b"and me")?;
    /// wal.append(b"drop me")?;
    ///
    /// wal.truncate_after(last_kept)?;
    ///
    /// let remaining: Vec<Vec<u8>> = wal
    ///     .iter()?
    ///     .map(|entry| entry.map(|r| r.into_data()))
    ///     .collect::<Result<_, _>>()?;
    /// assert_eq!(remaining, vec![b"keep me".to_vec(), b"and me".to_vec()]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn truncate_after(&self, lsn: Lsn) -> Result<()> {
        let start = lsn.get();

        // Confirm an intact record really lives at `lsn` before keeping it.
        let mut header = [0u8; HEADER_LEN];
        if self.store.read_at(start, &mut header)? < HEADER_LEN {
            return Err(WalError::corruption(start, "no record at this LSN"));
        }
        let parsed = record::parse_header(&header);
        if parsed.len > self.max_record_size {
            return Err(WalError::corruption(start, "no valid record at this LSN"));
        }
        let payload_start = start
            .checked_add(HEADER_LEN as u64)
            .ok_or_else(|| WalError::corruption(start, "record offset overflow"))?;
        let mut payload = vec![0u8; parsed.len as usize];
        if self.store.read_at(payload_start, &mut payload)? < payload.len() {
            return Err(WalError::corruption(start, "incomplete record at this LSN"));
        }
        if !record::verify(&header, &payload, parsed.crc) {
            return Err(WalError::corruption(start, "no valid record at this LSN"));
        }
        let new_end = payload_start
            .checked_add(u64::from(parsed.len))
            .ok_or_else(|| WalError::corruption(start, "record offset overflow"))?;

        self.store.truncate(new_end)?;
        self.store.sync()?;
        self.tail.0.store(new_end, Ordering::Release);
        self.commit.reset(new_end);
        Ok(())
    }

    /// The logical size of the log in bytes, including record framing.
    ///
    /// This is the offset at which the next append will land. It counts bytes
    /// that have been reserved, which under heavy concurrency may include a
    /// record another thread is still writing.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.tail.0.load(Ordering::Acquire)
    }

    /// Whether the log holds no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Frame `record` into a reused buffer and write it at `start`.
    fn frame_and_write(&self, start: u64, record: &[u8]) -> Result<()> {
        with_frame_buffer(|buf| {
            record::encode(buf, record);
            self.store.write_at(start, buf)
        })
    }

    /// Crate-internal access to the backing store, for tests that need to read,
    /// corrupt, or extend the on-disk image directly.
    #[cfg(test)]
    pub(crate) fn store(&self) -> &S {
        &self.store
    }
}

/// Frame a record using a reused thread-local buffer, so steady-state appends do
/// not allocate. Under loom a fresh buffer is used, since the model checker does
/// not need (and does not instrument) the thread-local.
#[cfg(not(loom))]
fn with_frame_buffer<R>(f: impl FnOnce(&mut Vec<u8>) -> R) -> R {
    thread_local! {
        static FRAME: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }
    FRAME.with(|cell| f(&mut cell.borrow_mut()))
}

#[cfg(loom)]
fn with_frame_buffer<R>(f: impl FnOnce(&mut Vec<u8>) -> R) -> R {
    let mut buf = Vec::new();
    f(&mut buf)
}

/// Scan a store from the start, returning the end offset of the last intact
/// record and truncating any torn tail beyond it.
fn recover<S: WalStore>(store: &S, max_record_size: u32) -> Result<u64> {
    let physical = store.len()?;
    let mut offset: u64 = 0;
    let mut header = [0u8; HEADER_LEN];

    while offset < physical {
        if store.read_at(offset, &mut header)? < HEADER_LEN {
            break; // incomplete header: torn tail
        }
        let parsed = record::parse_header(&header);
        if parsed.len > max_record_size {
            break; // implausible length: treat the rest as a torn tail
        }

        let payload_start = match offset.checked_add(HEADER_LEN as u64) {
            Some(start) => start,
            None => break,
        };
        let mut payload = vec![0u8; parsed.len as usize];
        if store.read_at(payload_start, &mut payload)? < payload.len() {
            break; // incomplete payload: torn tail
        }
        if !record::verify(&header, &payload, parsed.crc) {
            break; // checksum mismatch: stop here
        }

        offset = match payload_start.checked_add(u64::from(parsed.len)) {
            Some(end) => end,
            None => break,
        };
    }

    if offset < physical {
        store.truncate(offset)?;
    }
    Ok(offset)
}

impl<S: WalStore> fmt::Debug for Wal<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wal")
            .field("len", &self.tail.0.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// One record read back during iteration: its [`Lsn`] and its payload bytes.
///
/// Yielded by [`Wal::iter`]. The payload is owned (a fresh `Vec` per record);
/// take it without copying via [`into_data`](Record::into_data), or borrow it
/// via [`data`](Record::data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    lsn: Lsn,
    data: Vec<u8>,
}

impl Record {
    /// The sequence number this record was assigned — its byte offset in the log.
    pub fn lsn(&self) -> Lsn {
        self.lsn
    }

    /// The record's payload bytes.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The payload length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the record's payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Consume the record and take ownership of its payload without copying.
    #[must_use]
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    /// Decode the record's payload into a typed value via `pack-io`.
    ///
    /// The mirror of [`Wal::append_typed`]. Available with the `pack-io` feature.
    ///
    /// # Errors
    ///
    /// Returns [`WalError::Encoding`] if the bytes do not deserialise into `T` —
    /// for example reading a record written as a different type.
    ///
    /// # Examples
    ///
    /// ```
    /// use wal_db::{MemStore, Wal};
    /// use wal_db::pack_io::{Deserialize, Serialize};
    ///
    /// #[derive(Serialize, Deserialize, PartialEq, Debug)]
    /// struct Event {
    ///     id: u64,
    ///     name: String,
    /// }
    ///
    /// # fn main() -> Result<(), wal_db::WalError> {
    /// let wal = Wal::with_store(MemStore::new())?;
    /// wal.append_typed(&Event { id: 7, name: "boot".into() })?;
    ///
    /// let record = wal.iter()?.next().unwrap()?;
    /// let event: Event = record.decode()?;
    /// assert_eq!(event, Event { id: 7, name: "boot".into() });
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "pack-io")]
    pub fn decode<T: pack_io::Deserialize>(&self) -> Result<T> {
        pack_io::decode(&self.data).map_err(WalError::encoding)
    }
}

/// The outcome of reading one record-sized chunk at the iterator's cursor.
enum Step {
    /// A valid record, plus the offset just past it.
    Record(Record, u64),
    /// A damaged record. `skip_to` is the offset of the next record if its
    /// extent is known (length and payload present, only the checksum failed),
    /// or `None` if the damage makes the next record's position unknowable.
    Damaged(WalError, Option<u64>),
    /// A clean end: a short read, meaning the log stops here (a torn tail).
    End,
}

/// The iterator returned by [`Wal::iter`].
///
/// Walks the records fully written when it was created, yielding
/// `Result<`[`Record`]`>`. Behaviour at a damaged record follows the configured
/// [`RecoveryPolicy`]: by default the iterator yields the damage once and stops;
/// under [`RecoveryPolicy::SkipBadRecords`] it yields the damage and continues
/// past it when the next record's position is still recoverable.
pub struct WalIter<'a, S: WalStore = FileStore> {
    wal: &'a Wal<S>,
    offset: u64,
    end: u64,
    done: bool,
    policy: RecoveryPolicy,
}

impl<S: WalStore> WalIter<'_, S> {
    /// Read and classify the record at the current offset, without advancing.
    fn step(&self) -> Result<Step> {
        let mut header = [0u8; HEADER_LEN];
        if self.wal.store.read_at(self.offset, &mut header)? < HEADER_LEN {
            return Ok(Step::End);
        }
        let parsed = record::parse_header(&header);
        if parsed.len > self.wal.max_record_size {
            // The length is implausible, so the next record's position is
            // unknowable — there is nothing to skip to.
            return Ok(Step::Damaged(
                WalError::corruption(self.offset, "record length exceeds the maximum"),
                None,
            ));
        }

        let payload_start = self
            .offset
            .checked_add(HEADER_LEN as u64)
            .ok_or_else(|| WalError::corruption(self.offset, "record offset overflow"))?;
        let mut payload = vec![0u8; parsed.len as usize];
        if self.wal.store.read_at(payload_start, &mut payload)? < payload.len() {
            return Ok(Step::End);
        }
        let next = payload_start
            .checked_add(u64::from(parsed.len))
            .ok_or_else(|| WalError::corruption(self.offset, "record offset overflow"))?;

        if !record::verify(&header, &payload, parsed.crc) {
            // The length and payload are present, so we know where the next
            // record starts even though this one is corrupt.
            return Ok(Step::Damaged(
                WalError::corruption(self.offset, "checksum mismatch"),
                Some(next),
            ));
        }

        Ok(Step::Record(
            Record {
                lsn: Lsn::new(self.offset),
                data: payload,
            },
            next,
        ))
    }
}

impl<S: WalStore> Iterator for WalIter<'_, S> {
    type Item = Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.offset >= self.end {
            return None;
        }
        match self.step() {
            Ok(Step::Record(record, next)) => {
                self.offset = next;
                Some(Ok(record))
            }
            // Skip-bad-records, and the next record is locatable: surface the
            // damage but continue from past it on the next call.
            Ok(Step::Damaged(error, Some(next)))
                if self.policy == RecoveryPolicy::SkipBadRecords =>
            {
                self.offset = next;
                Some(Err(error))
            }
            // Stop-at-first-error, or damage that makes the next position
            // unknowable: surface the damage and end.
            Ok(Step::Damaged(error, _)) => {
                self.done = true;
                Some(Err(error))
            }
            Ok(Step::End) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}

impl<S: WalStore> fmt::Debug for WalIter<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalIter")
            .field("offset", &self.offset)
            .field("end", &self.end)
            .field("done", &self.done)
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    unused_must_use,
    unused_results
)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;
    use crate::store::MemStore;

    fn drain(wal: &Wal<MemStore>) -> Vec<Vec<u8>> {
        wal.iter()
            .unwrap()
            .map(|r| r.unwrap().into_data())
            .collect()
    }

    fn corrupt_byte(store: &MemStore, offset: u64) {
        let mut byte = [0u8; 1];
        store.read_at(offset, &mut byte).unwrap();
        byte[0] ^= 0xFF;
        store.write_at(offset, &byte).unwrap();
    }

    #[test]
    fn test_stop_at_first_error_stops_at_corruption() {
        let wal = Wal::with_store(MemStore::new()).unwrap(); // default policy
        wal.append(b"first").unwrap();
        let second = wal.append(b"second").unwrap();
        wal.append(b"third").unwrap();
        corrupt_byte(wal.store(), second.get() + HEADER_LEN as u64);

        let items: Vec<_> = wal.iter().unwrap().collect();
        assert_eq!(items.len(), 2); // first ok, second damaged, then stop
        assert_eq!(items[0].as_ref().unwrap().data(), b"first");
        assert!(matches!(items[1], Err(WalError::Corruption { .. })));
    }

    #[test]
    fn test_skip_bad_records_continues_past_corruption() {
        let config = WalConfig::new().with_recovery_policy(RecoveryPolicy::SkipBadRecords);
        let wal = Wal::with_store_and_config(MemStore::new(), config).unwrap();
        wal.append(b"first").unwrap();
        let second = wal.append(b"second").unwrap();
        wal.append(b"third").unwrap();
        // Corrupt the payload only; the length prefix stays intact, so the
        // record is skippable.
        corrupt_byte(wal.store(), second.get() + HEADER_LEN as u64);

        let items: Vec<_> = wal.iter().unwrap().collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].as_ref().unwrap().data(), b"first");
        assert!(matches!(items[1], Err(WalError::Corruption { .. })));
        assert_eq!(items[2].as_ref().unwrap().data(), b"third");
    }

    #[test]
    fn test_skip_bad_records_still_stops_on_unreadable_length() {
        let config = WalConfig::new()
            .with_max_record_size(16)
            .with_recovery_policy(RecoveryPolicy::SkipBadRecords);
        let wal = Wal::with_store_and_config(MemStore::new(), config).unwrap();
        wal.append(b"ok").unwrap();
        let second = wal.append(b"victim").unwrap();
        // Corrupt the length field to an implausible value: the next record's
        // position becomes unknowable, so even skip-mode must stop.
        corrupt_byte(wal.store(), second.get() + 4); // LEN_OFFSET within the header

        let items: Vec<_> = wal.iter().unwrap().collect();
        assert_eq!(items.len(), 2); // first ok, then a damaged stop
        assert_eq!(items[0].as_ref().unwrap().data(), b"ok");
        assert!(matches!(items[1], Err(WalError::Corruption { .. })));
    }

    #[cfg(feature = "pack-io")]
    #[test]
    fn test_typed_record_roundtrip() {
        use pack_io::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Entry {
            id: u64,
            label: String,
        }

        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append_typed(&Entry {
            id: 9,
            label: "nine".into(),
        })
        .unwrap();
        wal.append_typed(&Entry {
            id: 10,
            label: "ten".into(),
        })
        .unwrap();

        let decoded: Vec<Entry> = wal
            .iter()
            .unwrap()
            .map(|r| r.unwrap().decode().unwrap())
            .collect();
        assert_eq!(
            decoded[0],
            Entry {
                id: 9,
                label: "nine".into()
            }
        );
        assert_eq!(
            decoded[1],
            Entry {
                id: 10,
                label: "ten".into()
            }
        );
    }

    #[cfg(feature = "pack-io")]
    #[test]
    fn test_typed_decode_wrong_type_errors() {
        use pack_io::{Deserialize, Serialize};

        #[derive(Serialize)]
        struct Big {
            a: u64,
            b: u64,
            c: u64,
        }
        #[derive(Deserialize)]
        struct Small {
            _a: u8,
        }

        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append_typed(&Big { a: 1, b: 2, c: 3 }).unwrap();
        let record = wal.iter().unwrap().next().unwrap().unwrap();
        // Decoding 24 bytes as a 1-byte type leaves trailing bytes -> error.
        let result: Result<Small> = record.decode();
        assert!(matches!(result, Err(WalError::Encoding { .. })));
    }

    #[test]
    fn test_append_assigns_byte_offset_lsns() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        let a = wal.append(b"abc").unwrap(); // 8 header + 3 = 11 bytes
        let b = wal.append(b"de").unwrap();
        assert_eq!(a.get(), 0);
        assert_eq!(b.get(), 11);
    }

    #[test]
    fn test_iter_reads_back_all_records_in_order() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append(b"one").unwrap();
        wal.append(b"two").unwrap();
        wal.append(b"three").unwrap();
        assert_eq!(
            drain(&wal),
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
    }

    #[test]
    fn test_empty_log_iterates_to_nothing() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        assert!(wal.is_empty());
        assert_eq!(drain(&wal).len(), 0);
    }

    #[test]
    fn test_empty_record_roundtrips() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append(b"").unwrap();
        assert_eq!(drain(&wal), vec![Vec::<u8>::new()]);
    }

    #[test]
    fn test_record_too_large_is_rejected() {
        let config = WalConfig::new().with_max_record_size(4);
        let wal = Wal::with_store_and_config(MemStore::new(), config).unwrap();
        wal.append(b"ok").unwrap();
        let err = wal.append(b"too long").unwrap_err();
        assert!(matches!(err, WalError::RecordTooLarge { len: 8, max: 4 }));
        // The rejected append did not advance the log.
        assert_eq!(drain(&wal), vec![b"ok".to_vec()]);
    }

    #[test]
    fn test_reopen_recovers_records() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append(b"first").unwrap();
        wal.append(b"second").unwrap();
        wal.sync().unwrap();
        let image = wal.store().snapshot();

        let reopened = Wal::with_store(MemStore::from_bytes(image)).unwrap();
        assert_eq!(
            drain(&reopened),
            vec![b"first".to_vec(), b"second".to_vec()]
        );
        // The next append continues at the recovered end: two records of
        // (8 + 5) and (8 + 6) bytes leave the tail at 27.
        assert_eq!(reopened.append(b"third").unwrap().get(), 27);
    }

    #[test]
    fn test_recovery_truncates_torn_tail() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append(b"good record").unwrap();
        let clean_len = wal.len();
        // Append raw garbage directly to the store: a torn tail.
        wal.store().write_at(clean_len, &[0xAB; 5]).unwrap();

        let reopened = Wal::with_store(MemStore::from_bytes(wal.store().snapshot())).unwrap();
        assert_eq!(drain(&reopened), vec![b"good record".to_vec()]);
        assert_eq!(reopened.len(), clean_len);
    }

    #[test]
    fn test_corrupt_record_surfaces_error_then_stops() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append(b"intact").unwrap();
        let second = wal.append(b"victim").unwrap();
        // Flip a byte inside the second record's payload (offset + header).
        let payload_offset = second.get() + HEADER_LEN as u64;
        let mut byte = [0u8; 1];
        wal.store().read_at(payload_offset, &mut byte).unwrap();
        byte[0] ^= 0xFF;
        wal.store().write_at(payload_offset, &byte).unwrap();

        let mut iter = wal.iter().unwrap();
        assert_eq!(iter.next().unwrap().unwrap().data(), b"intact");
        assert!(matches!(
            iter.next().unwrap(),
            Err(WalError::Corruption { .. })
        ));
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_append_and_sync_is_durable() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append_and_sync(b"committed").unwrap();
        assert_eq!(drain(&wal), vec![b"committed".to_vec()]);
    }

    #[test]
    fn test_iter_from_seeks_to_lsn() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append(b"a").unwrap();
        let b = wal.append(b"b").unwrap();
        wal.append(b"c").unwrap();

        let got: Vec<Vec<u8>> = wal
            .iter_from(b)
            .unwrap()
            .map(|r| r.unwrap().into_data())
            .collect();
        assert_eq!(got, vec![b"b".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn test_iter_from_past_end_is_empty() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append(b"a").unwrap();
        assert_eq!(wal.iter_from(Lsn::new(9_999)).unwrap().count(), 0);
    }

    #[test]
    fn test_truncate_after_drops_later_records() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append(b"first").unwrap(); // [0, 13)
        let keep = wal.append(b"second").unwrap(); // [13, 27)
        wal.append(b"third").unwrap();
        wal.append(b"fourth").unwrap();

        wal.truncate_after(keep).unwrap();
        assert_eq!(drain(&wal), vec![b"first".to_vec(), b"second".to_vec()]);
        assert_eq!(wal.len(), 27);

        // Appends resume immediately after the kept record.
        assert_eq!(wal.append(b"new").unwrap().get(), 27);
        assert_eq!(
            drain(&wal),
            vec![b"first".to_vec(), b"second".to_vec(), b"new".to_vec()]
        );
    }

    #[test]
    fn test_truncate_after_keeping_last_record_is_a_no_op() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append(b"first").unwrap();
        let last = wal.append(b"second").unwrap();
        let before = wal.len();

        wal.truncate_after(last).unwrap();
        assert_eq!(wal.len(), before);
        assert_eq!(drain(&wal), vec![b"first".to_vec(), b"second".to_vec()]);
    }

    #[test]
    fn test_truncate_after_invalid_lsn_errors() {
        let config = WalConfig::new().with_max_record_size(64);
        let wal = Wal::with_store_and_config(MemStore::new(), config).unwrap();
        wal.append(b"only record").unwrap();
        // An LSN that does not land on a record boundary is rejected.
        let err = wal.truncate_after(Lsn::new(3)).unwrap_err();
        assert!(matches!(err, WalError::Corruption { .. }));
    }

    #[test]
    fn test_concurrent_appends_no_overlap_all_recovered() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 200;

        let wal = Arc::new(Wal::with_store(MemStore::new()).unwrap());
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let wal = Arc::clone(&wal);
            handles.push(thread::spawn(move || {
                let mut lsns = Vec::with_capacity(PER_THREAD);
                for i in 0..PER_THREAD {
                    let payload = format!("t{t}-r{i}").into_bytes();
                    lsns.push(wal.append(&payload).unwrap().get());
                }
                lsns
            }));
        }
        let mut all_lsns = Vec::new();
        for h in handles {
            all_lsns.extend(h.join().unwrap());
        }
        wal.sync().unwrap();

        // Every LSN is distinct (no two records shared a byte range).
        let mut sorted = all_lsns.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), THREADS * PER_THREAD);

        // Recovery reads back exactly the records that were appended, in offset
        // order, with no gaps or corruption.
        let records = drain(&wal);
        assert_eq!(records.len(), THREADS * PER_THREAD);

        // Reopening from the raw image recovers the same set.
        let reopened = Wal::with_store(MemStore::from_bytes(wal.store().snapshot())).unwrap();
        assert_eq!(reopened.iter().unwrap().count(), THREADS * PER_THREAD);
    }

    #[test]
    fn test_concurrent_append_and_sync_all_durable() {
        const THREADS: usize = 8;

        let wal = Arc::new(Wal::with_store(MemStore::new()).unwrap());
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let wal = Arc::clone(&wal);
            handles.push(thread::spawn(move || {
                for i in 0..50 {
                    wal.append_and_sync(format!("{t}:{i}").as_bytes()).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(drain(&wal).len(), THREADS * 50);
    }
}
