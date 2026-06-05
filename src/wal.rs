//! The log itself: [`Wal`], its recovery iterator [`WalIter`], and the
//! [`Record`] iteration yields.

use std::{fmt, io, sync::atomic::Ordering};

#[cfg(not(loom))]
use std::{cell::RefCell, path::Path};

use crate::{
    commit::Commit,
    config::WalConfig,
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
        })
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
}

/// The iterator returned by [`Wal::iter`].
///
/// Walks the records fully written when it was created, yielding
/// `Result<`[`Record`]`>`. A corrupt record yields a single
/// [`WalError::Corruption`] and then the iterator ends.
pub struct WalIter<'a, S: WalStore = FileStore> {
    wal: &'a Wal<S>,
    offset: u64,
    end: u64,
    done: bool,
}

impl<S: WalStore> WalIter<'_, S> {
    /// Read and validate the record at the current offset.
    fn read_next(&mut self) -> Result<Option<Record>> {
        let mut header = [0u8; HEADER_LEN];
        if self.wal.store.read_at(self.offset, &mut header)? < HEADER_LEN {
            return Ok(None);
        }
        let parsed = record::parse_header(&header);
        if parsed.len > self.wal.max_record_size {
            return Err(WalError::corruption(
                self.offset,
                "record length exceeds the maximum",
            ));
        }

        let payload_start = self
            .offset
            .checked_add(HEADER_LEN as u64)
            .ok_or_else(|| WalError::corruption(self.offset, "record offset overflow"))?;
        let mut payload = vec![0u8; parsed.len as usize];
        if self.wal.store.read_at(payload_start, &mut payload)? < payload.len() {
            return Ok(None);
        }
        if !record::verify(&header, &payload, parsed.crc) {
            return Err(WalError::corruption(self.offset, "checksum mismatch"));
        }

        let lsn = Lsn::new(self.offset);
        self.offset = payload_start
            .checked_add(u64::from(parsed.len))
            .ok_or_else(|| WalError::corruption(self.offset, "record offset overflow"))?;
        Ok(Some(Record { lsn, data: payload }))
    }
}

impl<S: WalStore> Iterator for WalIter<'_, S> {
    type Item = Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.offset >= self.end {
            return None;
        }
        match self.read_next() {
            Ok(Some(record)) => Some(Ok(record)),
            Ok(None) => {
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
