//! The log itself: [`Wal`], its recovery iterator [`WalIter`], and the
//! [`Record`] iteration yields.

use std::{
    fmt, io,
    path::Path,
    sync::{Mutex, MutexGuard, PoisonError},
};

use crate::{
    config::WalConfig,
    error::{Result, WalError},
    lsn::Lsn,
    record::{self, HEADER_LEN},
    store::{FileStore, WalStore},
};

/// A durable, append-only log.
///
/// `Wal` is the entry point. The four calls that cover almost every use are
/// [`open`](Wal::open), [`append`](Wal::append), [`sync`](Wal::sync), and
/// [`iter`](Wal::iter). The type parameter `S` is the storage backend and
/// defaults to [`FileStore`], so the plain name `Wal` is the file-backed log;
/// custom backends are supplied through [`with_store`](Wal::with_store).
///
/// A `Wal` is [`Send`] and [`Sync`] whenever its store is [`Send`] (both
/// [`FileStore`] and [`MemStore`](crate::MemStore) are), so it can be shared
/// across threads behind an [`Arc`](std::sync::Arc).
///
/// # Durability
///
/// [`append`](Wal::append) returns once the record is in the operating system's
/// page cache; [`sync`](Wal::sync) returns once every prior record is on stable
/// storage. Keeping these separate is what makes a WAL fast in the common case
/// and correct at the durability boundary — see the [crate docs](crate) for the
/// full contract.
///
/// # Concurrency
///
/// In this `0.2` foundation, appends are serialised through an internal mutex.
/// The signature is already the one the lock-free multi-writer path will use in
/// `0.3`, so code written against it will not have to change.
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
/// let a = wal.append(b"first")?;
/// let b = wal.append(b"second")?;
/// wal.sync()?;
///
/// assert_eq!(a.get(), 0);
/// assert_eq!(b.get(), 1);
///
/// let read_back: Vec<Vec<u8>> = wal.iter()?.collect::<Result<Vec<_>, _>>()?
///     .into_iter()
///     .map(|record| record.into_data())
///     .collect();
/// assert_eq!(read_back, vec![b"first".to_vec(), b"second".to_vec()]);
/// # Ok(())
/// # }
/// ```
pub struct Wal<S = FileStore> {
    inner: Mutex<Inner<S>>,
}

/// The locked interior of a [`Wal`]. Holds the store, the next sequence number,
/// the configured limit, and a reusable framing buffer.
struct Inner<S> {
    store: S,
    next_lsn: u64,
    max_record_size: u32,
    scratch: Vec<u8>,
}

impl Wal<FileStore> {
    /// Open the log at `path`, creating it if it does not exist.
    ///
    /// On open the log scans its contents, stopping at the first record that is
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
    /// Behaves like [`open`](Wal::open) but applies the supplied configuration,
    /// for example a tighter [`max_record_size`](WalConfig::max_record_size).
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
    /// This is the entry point for backends other than a file — an in-memory
    /// [`MemStore`](crate::MemStore) in tests, or any type implementing
    /// [`WalStore`].
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
        let mut inner = Inner {
            store,
            next_lsn: 0,
            max_record_size: config.max_record_size(),
            scratch: Vec::new(),
        };
        inner.recover()?;
        Ok(Wal {
            inner: Mutex::new(inner),
        })
    }

    /// Append `record` to the log and return the [`Lsn`] it was assigned.
    ///
    /// Returns once the bytes are in the operating system's page cache. It does
    /// **not** flush the disk — call [`sync`](Wal::sync) for that. A crash
    /// between `append` and `sync` may lose the record.
    ///
    /// # Errors
    ///
    /// - [`WalError::RecordTooLarge`] if `record` is larger than the configured
    ///   [`max_record_size`](WalConfig::max_record_size). The log is unchanged.
    /// - [`WalError::Io`] if the write fails. After an I/O error the log should
    ///   be reopened before further use, since the tail may be partially
    ///   written; recovery will discard it.
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
        self.lock().append(record)
    }

    /// Make every record appended before this call durable.
    ///
    /// Returns once the data is on stable storage, using the platform's true
    /// durability barrier. This is the only call that survives a power loss; it
    /// is also the expensive one, which is why it is separate from `append`.
    ///
    /// # Errors
    ///
    /// Returns [`WalError::Io`] if the flush fails. A failed sync means the
    /// records are not durable; it is a fatal condition the caller must handle,
    /// not retry blindly.
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
        self.lock().store.sync()
    }

    /// Iterate the log from the beginning, yielding each record in append order.
    ///
    /// The iterator captures the log's current length when it is created, so it
    /// walks the records present at that moment and is not affected by appends
    /// made afterwards. Each item is a [`Result`]: a damaged record (one that
    /// fails its checksum) yields a single [`WalError::Corruption`], after which
    /// the iterator stops. In a log opened normally the torn tail has already
    /// been truncated, so iteration simply runs to the end.
    ///
    /// # Errors
    ///
    /// Returns [`WalError::Io`] if the log's length cannot be read to start the
    /// scan. Per-record errors are delivered as iterator items.
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
        let end = self.lock().store.len()?;
        Ok(WalIter {
            wal: self,
            offset: 0,
            end,
            done: false,
        })
    }

    /// The size of the log in bytes, including record framing.
    ///
    /// # Errors
    ///
    /// Returns [`WalError::Io`] if the store cannot report its length.
    pub fn len(&self) -> Result<u64> {
        self.lock().store.len()
    }

    /// Whether the log holds no records.
    ///
    /// # Errors
    ///
    /// Returns [`WalError::Io`] if the store cannot report its length.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Acquire the interior lock.
    ///
    /// The critical sections in this module never panic — there is no `unwrap`,
    /// `expect`, or panicking index among them — so the mutex is never poisoned
    /// in practice. Should a poison flag somehow be set, the interior state is
    /// still consistent, so the guard is recovered rather than propagated as a
    /// spurious error.
    fn lock(&self) -> MutexGuard<'_, Inner<S>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<S: WalStore> Inner<S> {
    /// Frame and write one record, then advance the sequence number.
    fn append(&mut self, record: &[u8]) -> Result<Lsn> {
        if record.len() > self.max_record_size as usize {
            return Err(WalError::RecordTooLarge {
                len: record.len(),
                max: self.max_record_size,
            });
        }

        let lsn = self.next_lsn;
        record::encode(&mut self.scratch, lsn, record);
        self.store.append(&self.scratch)?;

        self.next_lsn = self.next_lsn.checked_add(1).ok_or_else(|| {
            WalError::io(
                "assigning the next sequence number",
                io::Error::other("LSN space exhausted"),
            )
        })?;
        Ok(Lsn::new(lsn))
    }

    /// Scan from the start, find the end of the last intact record, truncate any
    /// torn tail, and set the next sequence number to one past the last valid
    /// record.
    fn recover(&mut self) -> Result<()> {
        let total = self.store.len()?;
        let mut offset: u64 = 0;
        let mut next_lsn: u64 = 0;
        let mut header = [0u8; HEADER_LEN];

        while offset < total {
            if self.store.read_at(offset, &mut header)? < HEADER_LEN {
                break; // incomplete header: torn tail
            }
            let h = record::parse_header(&header);
            if h.len > self.max_record_size {
                break; // implausible length: treat the rest as a torn tail
            }

            let payload_start = match offset.checked_add(HEADER_LEN as u64) {
                Some(start) => start,
                None => break,
            };
            let mut payload = vec![0u8; h.len as usize];
            if self.store.read_at(payload_start, &mut payload)? < payload.len() {
                break; // incomplete payload: torn tail
            }
            if !record::verify(&header, &payload, h.crc) {
                break; // checksum mismatch: stop here
            }

            offset = match payload_start.checked_add(u64::from(h.len)) {
                Some(end) => end,
                None => break,
            };
            next_lsn = h.lsn.checked_add(1).unwrap_or(next_lsn);
        }

        if offset < total {
            self.store.truncate(offset)?;
        }
        self.next_lsn = next_lsn;
        Ok(())
    }
}

impl<S: WalStore> fmt::Debug for Wal<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately does not lock or print interior state — a `Debug` impl
        // should never be able to block.
        f.debug_struct("Wal").finish_non_exhaustive()
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
    /// The sequence number this record was assigned when it was appended.
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
/// Walks the records present when it was created, yielding `Result<`[`Record`]`>`.
/// A corrupt record yields a single [`WalError::Corruption`] and then ends.
pub struct WalIter<'a, S: WalStore = FileStore> {
    wal: &'a Wal<S>,
    offset: u64,
    end: u64,
    done: bool,
}

impl<S: WalStore> WalIter<'_, S> {
    /// Read and validate the record at the current offset.
    ///
    /// `Ok(Some(record))` advances past a good record; `Ok(None)` is a clean
    /// stop (an incomplete tail within the snapshot); `Err` is a damaged record.
    fn read_next(&mut self) -> Result<Option<Record>> {
        let inner = self.wal.lock();
        let max = inner.max_record_size;

        let mut header = [0u8; HEADER_LEN];
        if inner.store.read_at(self.offset, &mut header)? < HEADER_LEN {
            return Ok(None);
        }
        let h = record::parse_header(&header);
        if h.len > max {
            return Err(WalError::corruption(
                self.offset,
                "record length exceeds the maximum",
            ));
        }

        let payload_start = self
            .offset
            .checked_add(HEADER_LEN as u64)
            .ok_or_else(|| WalError::corruption(self.offset, "record offset overflow"))?;
        let mut payload = vec![0u8; h.len as usize];
        if inner.store.read_at(payload_start, &mut payload)? < payload.len() {
            return Ok(None);
        }
        if !record::verify(&header, &payload, h.crc) {
            return Err(WalError::corruption(self.offset, "checksum mismatch"));
        }

        self.offset = payload_start
            .checked_add(u64::from(h.len))
            .ok_or_else(|| WalError::corruption(self.offset, "record offset overflow"))?;
        Ok(Some(Record {
            lsn: Lsn::new(h.lsn),
            data: payload,
        }))
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
            Err(e) => {
                self.done = true;
                Some(Err(e))
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, unused_must_use)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    fn drain(wal: &Wal<MemStore>) -> Vec<Vec<u8>> {
        wal.iter()
            .unwrap()
            .map(|r| r.unwrap().into_data())
            .collect()
    }

    #[test]
    fn test_append_assigns_dense_increasing_lsns() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        assert_eq!(wal.append(b"a").unwrap().get(), 0);
        assert_eq!(wal.append(b"b").unwrap().get(), 1);
        assert_eq!(wal.append(b"c").unwrap().get(), 2);
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
        assert!(wal.is_empty().unwrap());
        assert_eq!(drain(&wal).len(), 0);
    }

    #[test]
    fn test_empty_record_roundtrips() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        let lsn = wal.append(b"").unwrap();
        assert_eq!(lsn.get(), 0);
        let records = drain(&wal);
        assert_eq!(records, vec![Vec::<u8>::new()]);
    }

    #[test]
    fn test_record_too_large_is_rejected_and_leaves_log_unchanged() {
        let config = WalConfig::new().with_max_record_size(4);
        let wal = Wal::with_store_and_config(MemStore::new(), config).unwrap();
        wal.append(b"ok").unwrap();

        let err = wal.append(b"too long").unwrap_err();
        assert!(matches!(err, WalError::RecordTooLarge { len: 8, max: 4 }));

        // The rejected append did not change the log or skip an LSN.
        assert_eq!(wal.append(b"next").unwrap().get(), 1);
        assert_eq!(drain(&wal), vec![b"ok".to_vec(), b"next".to_vec()]);
    }

    #[test]
    fn test_reopen_recovers_records_and_continues_lsns() {
        // Build a log in one store, then hand the bytes to a fresh Wal to
        // simulate reopening.
        let store = MemStore::new();
        let wal = Wal::with_store(store).unwrap();
        wal.append(b"first").unwrap();
        wal.append(b"second").unwrap();
        wal.sync().unwrap();
        let bytes = wal.lock().store.clone();

        let reopened = Wal::with_store(bytes).unwrap();
        assert_eq!(
            drain(&reopened),
            vec![b"first".to_vec(), b"second".to_vec()]
        );
        // Next LSN continues from where the recovered log left off.
        assert_eq!(reopened.append(b"third").unwrap().get(), 2);
    }

    #[test]
    fn test_recovery_truncates_torn_tail() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append(b"good record").unwrap();
        let clean_len = wal.len().unwrap();

        // Append raw garbage that cannot form a valid record: a torn tail.
        wal.lock().store.append(&[0xAB; 7]).unwrap();
        let torn = wal.lock().store.clone();
        assert!(torn.len().unwrap() > clean_len);

        let reopened = Wal::with_store(torn).unwrap();
        // The good record survives; the torn tail is gone.
        assert_eq!(drain(&reopened), vec![b"good record".to_vec()]);
        assert_eq!(reopened.len().unwrap(), clean_len);
        // Appends continue cleanly after the truncated tail, with no LSN gap.
        assert_eq!(reopened.append(b"after").unwrap().get(), 1);
    }

    #[test]
    fn test_iter_snapshot_excludes_later_appends() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append(b"a").unwrap();
        let iter = wal.iter().unwrap();
        wal.append(b"b").unwrap(); // appended after the iterator was created
        let collected: Vec<_> = iter.map(|r| r.unwrap().into_data()).collect();
        assert_eq!(collected, vec![b"a".to_vec()]);
    }

    #[test]
    fn test_corrupt_middle_record_surfaces_error_then_stops() {
        let wal = Wal::with_store(MemStore::new()).unwrap();
        wal.append(b"intact").unwrap();
        wal.append(b"victim").unwrap();
        // Flip a byte inside the second record's payload. The first record's
        // end is HEADER_LEN + 6; the second payload starts after another header.
        {
            let mut guard = wal.lock();
            let first_end = HEADER_LEN + 6;
            let second_payload = first_end + HEADER_LEN;
            guard.store.data_mut()[second_payload] ^= 0xFF;
        }

        let mut iter = wal.iter().unwrap();
        let first = iter.next().unwrap().unwrap();
        assert_eq!(first.data(), b"intact");
        let second = iter.next().unwrap();
        assert!(matches!(second, Err(WalError::Corruption { .. })));
        assert!(iter.next().is_none());
    }
}
