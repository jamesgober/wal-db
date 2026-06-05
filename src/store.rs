//! Storage backends.
//!
//! A [`Wal`](crate::Wal) frames records, hands out sequence numbers, and
//! coordinates durability; the bytes themselves live behind the [`WalStore`]
//! trait. Every method takes `&self`, because the multi-writer append path
//! writes from several threads at once — the store must accept concurrent,
//! positioned writes without a lock of its own (a file does; an in-memory
//! [`MemStore`] uses a short internal lock). The default [`FileStore`] writes to
//! a file; a custom implementation could put the log on any byte-addressable,
//! appendable medium.

use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::error::{Result, WalError};

/// A byte-addressable, append-only store with an explicit durability barrier.
///
/// The log treats a store as a growing array of bytes. It writes framed records
/// at reserved offsets — possibly from several threads concurrently and out of
/// order — reads from arbitrary offsets during recovery, and occasionally
/// truncates a torn tail. The one guarantee the log cannot provide itself — that
/// written bytes have reached stable storage — is delegated to
/// [`sync`](WalStore::sync).
///
/// # Implementing a backend
///
/// The contract an implementation must honour:
///
/// - [`write_at`](WalStore::write_at) writes `bytes` at `offset`, growing the
///   store if `offset` is past the current end and zero-filling any gap (so a
///   later offset written before an earlier one leaves detectable zero bytes in
///   between, exactly as a sparse file does). Concurrent calls to disjoint
///   ranges must not corrupt each other.
/// - [`read_at`](WalStore::read_at) fills `buf` starting at `offset`, returning
///   the number of bytes read. It returns fewer than `buf.len()` only when the
///   store ends first — that short read is how recovery detects a torn tail.
/// - [`sync`](WalStore::sync) returns only once every prior write is durable.
/// - [`truncate`](WalStore::truncate) discards everything at or after `len`.
///
/// `Send + Sync` is required so the log can be shared across threads.
///
/// # Examples
///
/// ```
/// use wal_db::{MemStore, Wal};
///
/// # fn main() -> Result<(), wal_db::WalError> {
/// let wal = Wal::with_store(MemStore::new())?;
/// wal.append(b"record")?;
/// wal.sync()?;
/// # Ok(())
/// # }
/// ```
pub trait WalStore: Send + Sync {
    /// Write `bytes` at byte `offset`, growing the store and zero-filling any
    /// gap if `offset` is beyond the current end.
    fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<()>;

    /// Read into `buf` starting at byte `offset`, returning the number of bytes
    /// read.
    ///
    /// A return value smaller than `buf.len()` means the store ended before
    /// `buf` could be filled.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Discard every byte at or after `len`, shrinking the store to exactly
    /// `len` bytes.
    fn truncate(&self, len: u64) -> Result<()>;

    /// Flush every preceding [`write_at`](WalStore::write_at) to stable storage.
    ///
    /// Returns only once the data will survive a power loss. This is the
    /// durability barrier the whole log rests on.
    fn sync(&self) -> Result<()>;

    /// The current size of the store in bytes.
    fn len(&self) -> Result<u64>;

    /// Whether the store holds no bytes.
    ///
    /// The default defers to [`len`](WalStore::len); override it only if a
    /// backend can answer more cheaply.
    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// The lowest offset that still holds data.
    ///
    /// Normally `0`. A backend that can drop a prefix (see
    /// [`truncate_before`](WalStore::truncate_before)) reports the offset of its
    /// first surviving byte here, so recovery knows where to begin scanning. The
    /// default backends that cannot drop a prefix leave this at `0`.
    fn head(&self) -> Result<u64> {
        Ok(0)
    }

    /// Discard storage entirely below `offset`, if the backend can, returning the
    /// new [`head`](WalStore::head).
    ///
    /// Offsets are preserved: dropping a prefix never renumbers what remains, so
    /// a record keeps its byte position (its LSN) for life. A backend that cannot
    /// remove a prefix — a single file, where the surviving bytes would have to
    /// move — leaves the store unchanged and returns its current head. One that
    /// can (a segmented store, by deleting whole leading segment files) removes
    /// what it can at its own granularity and returns the resulting head, which
    /// may be below `offset`.
    fn truncate_before(&self, _offset: u64) -> Result<u64> {
        self.head()
    }
}

/// A file-backed [`WalStore`]: the default storage for [`Wal::open`](crate::Wal::open).
///
/// All reads and writes are positioned (`pread`/`pwrite` on Unix, `seek_read`/
/// `seek_write` on Windows), so concurrent appenders writing to disjoint offsets
/// never contend on a shared file cursor, and a recovery read never disturbs an
/// append. [`sync`](WalStore::sync) issues the platform's true durability
/// barrier: `fdatasync` on Linux, `FlushFileBuffers` on Windows, and
/// `fcntl(F_FULLFSYNC)` on macOS — the last because macOS's `fsync` does not
/// flush the drive's write cache.
#[derive(Debug)]
pub struct FileStore {
    file: File,
    path: PathBuf,
}

impl FileStore {
    /// Open the file at `path`, creating it if it does not exist.
    ///
    /// The store does not interpret the file's contents — it does not look for a
    /// torn tail or validate records. That is [`Wal::open`](crate::Wal::open)'s
    /// job, which scans on open and truncates any incomplete trailing record.
    ///
    /// # Errors
    ///
    /// Returns [`WalError::Io`] if the file cannot be opened (for example a
    /// missing parent directory or insufficient permissions).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| WalError::io("opening the log file", e))?;
        Ok(FileStore { file, path })
    }

    /// The path this store was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl WalStore for FileStore {
    fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        pwrite_all(&self.file, offset, bytes).map_err(|e| WalError::io("writing a record", e))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        pread_fill(&self.file, offset, buf).map_err(|e| WalError::io("reading from the log", e))
    }

    fn truncate(&self, len: u64) -> Result<()> {
        self.file
            .set_len(len)
            .map_err(|e| WalError::io("truncating the log", e))
    }

    fn sync(&self) -> Result<()> {
        durable_sync(&self.file).map_err(|e| WalError::io("flushing to stable storage", e))
    }

    fn len(&self) -> Result<u64> {
        Ok(self
            .file
            .metadata()
            .map_err(|e| WalError::io("reading log file metadata", e))?
            .len())
    }
}

/// An in-memory [`WalStore`] backed by a `Vec<u8>` behind a short lock.
///
/// Everything a [`FileStore`] does, without touching the filesystem, including
/// the sparse-file behaviour of zero-filling a gap when a higher offset is
/// written first. [`sync`](WalStore::sync) is a no-op — memory has no separate
/// durable tier — so a `MemStore` is for tests, examples, and benchmarking the
/// framing path in isolation, not for durability.
///
/// # Examples
///
/// ```
/// use wal_db::{MemStore, Wal};
///
/// # fn main() -> Result<(), wal_db::WalError> {
/// let wal = Wal::with_store(MemStore::new())?;
/// let lsn = wal.append(b"in memory")?;
/// assert_eq!(lsn.get(), 0);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Default)]
pub struct MemStore {
    data: Mutex<Vec<u8>>,
}

impl MemStore {
    /// Create an empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        MemStore {
            data: Mutex::new(Vec::new()),
        }
    }

    /// Create an empty store that has pre-allocated room for `capacity` bytes,
    /// to avoid reallocations during a known-size workload.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        MemStore {
            data: Mutex::new(Vec::with_capacity(capacity)),
        }
    }

    /// Create a store preloaded with `bytes` — for example a log image captured
    /// elsewhere, so [`Wal::with_store`](crate::Wal::with_store) can recover it.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        MemStore {
            data: Mutex::new(bytes),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A copy of the current bytes. Crate-internal, for tests that inspect or
    /// snapshot the on-disk image.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Vec<u8> {
        self.lock().clone()
    }
}

impl Clone for MemStore {
    fn clone(&self) -> Self {
        MemStore {
            data: Mutex::new(self.lock().clone()),
        }
    }
}

impl WalStore for MemStore {
    fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        let start = usize::try_from(offset).map_err(|_| {
            WalError::io(
                "writing to memory",
                io::Error::other("offset exceeds usize"),
            )
        })?;
        let end = start.checked_add(bytes.len()).ok_or_else(|| {
            WalError::io(
                "writing to memory",
                io::Error::other("write overflows usize"),
            )
        })?;

        let mut data = self.lock();
        if data.len() < end {
            data.resize(end, 0); // zero-fill any gap, like a sparse file
        }
        data[start..end].copy_from_slice(bytes);
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let data = self.lock();
        let start = match usize::try_from(offset) {
            Ok(start) if start < data.len() => start,
            _ => return Ok(0),
        };
        let available = &data[start..];
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        Ok(n)
    }

    fn truncate(&self, len: u64) -> Result<()> {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        self.lock().truncate(len);
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.lock().len() as u64)
    }
}

// ---------------------------------------------------------------------------
// Platform-correct durability.
// ---------------------------------------------------------------------------

/// Flush every buffered write for `file` to stable storage.
///
/// On macOS this is `fcntl(F_FULLFSYNC)`. The standard library's `sync_all` and
/// `sync_data` call `fsync(2)` there, which flushes the page cache to the device
/// but leaves the data in the device's own write cache, where a power loss can
/// still take it. `F_FULLFSYNC` is the documented way to force a full flush.
#[cfg(target_os = "macos")]
pub(crate) fn durable_sync(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let fd = file.as_raw_fd();
    // SAFETY: `fd` is a valid, open file descriptor for as long as `file` is
    // borrowed, so it cannot be closed from under us. `F_FULLFSYNC` takes no
    // argument pointer and neither reads nor writes any user buffer. `fcntl`
    // reports failure by returning -1 and setting `errno`, which is checked
    // immediately below.
    let ret = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
    if ret == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// On Linux `sync_data` is `fdatasync`; on Windows it is `FlushFileBuffers`.
/// Both are true durability barriers, so the standard library call is correct
/// on every platform except macOS.
#[cfg(not(target_os = "macos"))]
pub(crate) fn durable_sync(file: &File) -> io::Result<()> {
    file.sync_data()
}

// ---------------------------------------------------------------------------
// Positioned I/O. Reads and writes carry their own offset so they never move a
// shared file cursor, which is what lets disjoint concurrent writes proceed
// without a lock.
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub(crate) fn pwrite_all(file: &File, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;

    while !buf.is_empty() {
        match file.write_at(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the store accepted zero bytes mid-record",
                ));
            }
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn pwrite_all(file: &File, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;

    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the store accepted zero bytes mid-record",
                ));
            }
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn pread_fill(file: &File, mut offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;

    let mut total = 0;
    while total < buf.len() {
        match file.read_at(&mut buf[total..], offset) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                offset += n as u64;
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

#[cfg(windows)]
pub(crate) fn pread_fill(file: &File, mut offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;

    let mut total = 0;
    while total < buf.len() {
        match file.seek_read(&mut buf[total..], offset) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                offset += n as u64;
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_memstore_write_at_advances_len() {
        let store = MemStore::new();
        assert_eq!(store.len().unwrap(), 0);
        store.write_at(0, b"abc").unwrap();
        assert_eq!(store.len().unwrap(), 3);
        store.write_at(3, b"de").unwrap();
        assert_eq!(store.len().unwrap(), 5);
    }

    #[test]
    fn test_memstore_write_past_end_zero_fills_gap() {
        let store = MemStore::new();
        // Write at offset 4 while the store is empty: the gap [0,4) is zeros.
        store.write_at(4, b"XY").unwrap();
        assert_eq!(store.len().unwrap(), 6);
        let mut buf = [0xFFu8; 6];
        assert_eq!(store.read_at(0, &mut buf).unwrap(), 6);
        assert_eq!(&buf, &[0, 0, 0, 0, b'X', b'Y']);
    }

    #[test]
    fn test_memstore_read_past_end_is_short() {
        let store = MemStore::new();
        store.write_at(0, b"abc").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(store.read_at(1, &mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"bc");
    }

    #[test]
    fn test_memstore_truncate_shrinks() {
        let store = MemStore::new();
        store.write_at(0, b"0123456789").unwrap();
        store.truncate(4).unwrap();
        assert_eq!(store.len().unwrap(), 4);
    }

    #[test]
    fn test_filestore_roundtrip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.bin");

        {
            let store = FileStore::open(&path).unwrap();
            store.write_at(0, b"hello world").unwrap();
            store.sync().unwrap();
            assert_eq!(store.len().unwrap(), 11);
        }

        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.len().unwrap(), 11);
        let mut buf = [0u8; 5];
        assert_eq!(store.read_at(6, &mut buf).unwrap(), 5);
        assert_eq!(&buf, b"world");
    }

    #[test]
    fn test_filestore_concurrent_disjoint_writes() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrent.bin");
        let store = Arc::new(FileStore::open(&path).unwrap());

        let mut handles = Vec::new();
        for i in 0..8u64 {
            let store = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                let byte = b'A' + i as u8;
                store.write_at(i * 4, &[byte; 4]).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        store.sync().unwrap();

        let mut buf = [0u8; 32];
        assert_eq!(store.read_at(0, &mut buf).unwrap(), 32);
        for i in 0..8 {
            let expected = b'A' + i as u8;
            assert_eq!(&buf[i * 4..i * 4 + 4], &[expected; 4]);
        }
    }

    #[test]
    fn test_filestore_sync_durable_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("durable.bin");
        {
            let store = FileStore::open(&path).unwrap();
            store.write_at(0, b"persisted").unwrap();
            store.sync().unwrap();
        }
        let store = FileStore::open(&path).unwrap();
        let mut buf = [0u8; 9];
        assert_eq!(store.read_at(0, &mut buf).unwrap(), 9);
        assert_eq!(&buf, b"persisted");
    }
}
