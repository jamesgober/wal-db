//! Storage backends.
//!
//! A [`Wal`](crate::Wal) frames records and tracks sequence numbers; the bytes
//! themselves live behind the [`WalStore`] trait. That trait is the seam for
//! swapping where a log is kept: the default [`FileStore`] writes to a file,
//! [`MemStore`] keeps everything in memory for tests, and a custom
//! implementation could put the log on any byte-addressable, appendable medium.

use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use crate::error::{Result, WalError};

/// A byte-addressable, append-only store with an explicit durability barrier.
///
/// The log treats a store as a growing array of bytes. It appends framed
/// records to the end, reads from arbitrary offsets during recovery, and
/// occasionally truncates a torn tail. The one guarantee the log cannot provide
/// itself — that written bytes have reached stable storage — is delegated to
/// [`sync`](WalStore::sync).
///
/// # Implementing a backend
///
/// The contract an implementation must honour:
///
/// - [`append`](WalStore::append) places `bytes` immediately after the current
///   end of the store and is atomic with respect to [`len`](WalStore::len):
///   after it returns `Ok`, `len` has grown by `bytes.len()`.
/// - [`read_at`](WalStore::read_at) fills `buf` starting at `offset`, returning
///   the number of bytes read. It returns fewer than `buf.len()` only when the
///   store ends first — that short read is how recovery detects a torn tail.
/// - [`sync`](WalStore::sync) returns only once every prior `append` is durable.
/// - [`truncate`](WalStore::truncate) discards everything at or after `len`.
///
/// # Examples
///
/// The in-memory [`MemStore`] is the smallest complete implementation; see its
/// source for a reference. Using one with a log:
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
pub trait WalStore {
    /// Append `bytes` to the end of the store.
    fn append(&mut self, bytes: &[u8]) -> Result<()>;

    /// Read into `buf` starting at byte `offset`, returning the number of bytes
    /// read.
    ///
    /// A return value smaller than `buf.len()` means the store ended before
    /// `buf` could be filled.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Discard every byte at or after `len`, shrinking the store to exactly
    /// `len` bytes.
    fn truncate(&mut self, len: u64) -> Result<()>;

    /// Flush every preceding [`append`](WalStore::append) to stable storage.
    ///
    /// Returns only once the data will survive a power loss. This is the
    /// durability barrier the whole log rests on.
    fn sync(&mut self) -> Result<()>;

    /// The current size of the store in bytes — equivalently, the offset at
    /// which the next [`append`](WalStore::append) will land.
    fn len(&self) -> Result<u64>;

    /// Whether the store holds no bytes.
    ///
    /// The default defers to [`len`](WalStore::len); override it only if a
    /// backend can answer more cheaply.
    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

/// A file-backed [`WalStore`]: the default storage for [`Wal::open`](crate::Wal::open).
///
/// All reads and writes are positioned (`pread`/`pwrite` on Unix, `seek_read`/
/// `seek_write` on Windows), so a read during recovery never disturbs the
/// position the next append will write to. [`sync`](WalStore::sync) issues the
/// platform's true durability barrier: `fdatasync` on Linux, `FlushFileBuffers`
/// on Windows, and `fcntl(F_FULLFSYNC)` on macOS — the last because macOS's
/// `fsync` does not flush the drive's write cache.
#[derive(Debug)]
pub struct FileStore {
    file: File,
    write_offset: u64,
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
    /// missing parent directory or insufficient permissions) or its length
    /// cannot be read.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| WalError::io("opening the log file", e))?;
        let write_offset = file
            .metadata()
            .map_err(|e| WalError::io("reading log file metadata", e))?
            .len();
        Ok(FileStore {
            file,
            write_offset,
            path,
        })
    }

    /// The path this store was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl WalStore for FileStore {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        pwrite_all(&self.file, self.write_offset, bytes)
            .map_err(|e| WalError::io("appending a record", e))?;
        self.write_offset = self
            .write_offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| {
                WalError::io(
                    "advancing the write position",
                    io::Error::other("log size exceeds u64"),
                )
            })?;
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        pread_fill(&self.file, offset, buf).map_err(|e| WalError::io("reading from the log", e))
    }

    fn truncate(&mut self, len: u64) -> Result<()> {
        self.file
            .set_len(len)
            .map_err(|e| WalError::io("truncating the log", e))?;
        self.write_offset = len;
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        durable_sync(&self.file).map_err(|e| WalError::io("flushing to stable storage", e))
    }

    fn len(&self) -> Result<u64> {
        Ok(self.write_offset)
    }
}

/// An in-memory [`WalStore`] backed by a `Vec<u8>`.
///
/// Everything a [`FileStore`] does, without touching the filesystem.
/// [`sync`](WalStore::sync) is a no-op — memory has no separate durable tier —
/// so a `MemStore` is for tests, examples, and benchmarking the framing path in
/// isolation, not for durability.
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
#[derive(Debug, Default, Clone)]
pub struct MemStore {
    data: Vec<u8>,
}

impl MemStore {
    /// Create an empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        MemStore { data: Vec::new() }
    }

    /// Create an empty store that has pre-allocated room for `capacity` bytes,
    /// to avoid reallocations during a known-size workload.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        MemStore {
            data: Vec::with_capacity(capacity),
        }
    }

    /// Crate-internal mutable access to the backing bytes, used by recovery and
    /// torn-write tests that need to corrupt the on-disk image directly.
    #[cfg(test)]
    pub(crate) fn data_mut(&mut self) -> &mut Vec<u8> {
        &mut self.data
    }
}

impl WalStore for MemStore {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.data.extend_from_slice(bytes);
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let start = match usize::try_from(offset) {
            Ok(start) if start < self.data.len() => start,
            _ => return Ok(0),
        };
        let available = &self.data[start..];
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        Ok(n)
    }

    fn truncate(&mut self, len: u64) -> Result<()> {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        self.data.truncate(len);
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.data.len() as u64)
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
fn durable_sync(file: &File) -> io::Result<()> {
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
fn durable_sync(file: &File) -> io::Result<()> {
    file.sync_data()
}

// ---------------------------------------------------------------------------
// Positioned I/O. Reads and writes carry their own offset so they never move a
// shared file cursor, which keeps recovery reads from racing the append tail.
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn pwrite_all(file: &File, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
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
fn pwrite_all(file: &File, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
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
fn pread_fill(file: &File, mut offset: u64, buf: &mut [u8]) -> io::Result<usize> {
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
fn pread_fill(file: &File, mut offset: u64, buf: &mut [u8]) -> io::Result<usize> {
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
    fn test_memstore_append_advances_len() {
        let mut store = MemStore::new();
        assert_eq!(store.len().unwrap(), 0);
        store.append(b"abc").unwrap();
        assert_eq!(store.len().unwrap(), 3);
        store.append(b"de").unwrap();
        assert_eq!(store.len().unwrap(), 5);
    }

    #[test]
    fn test_memstore_read_at_returns_requested_bytes() {
        let mut store = MemStore::new();
        store.append(b"0123456789").unwrap();
        let mut buf = [0u8; 4];
        let n = store.read_at(2, &mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf, b"2345");
    }

    #[test]
    fn test_memstore_read_past_end_is_short() {
        let mut store = MemStore::new();
        store.append(b"abc").unwrap();
        let mut buf = [0u8; 8];
        let n = store.read_at(1, &mut buf).unwrap();
        assert_eq!(n, 2); // only "bc" remained
        assert_eq!(&buf[..2], b"bc");
    }

    #[test]
    fn test_memstore_read_at_or_past_eof_is_zero() {
        let mut store = MemStore::new();
        store.append(b"abc").unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(store.read_at(3, &mut buf).unwrap(), 0);
        assert_eq!(store.read_at(99, &mut buf).unwrap(), 0);
    }

    #[test]
    fn test_memstore_truncate_shrinks() {
        let mut store = MemStore::new();
        store.append(b"0123456789").unwrap();
        store.truncate(4).unwrap();
        assert_eq!(store.len().unwrap(), 4);
        let mut buf = [0u8; 8];
        assert_eq!(store.read_at(0, &mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"0123");
    }

    #[test]
    fn test_filestore_roundtrip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.bin");

        {
            let mut store = FileStore::open(&path).unwrap();
            store.append(b"hello world").unwrap();
            store.sync().unwrap();
            assert_eq!(store.len().unwrap(), 11);
        }

        // Reopen and read it back at an offset.
        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.len().unwrap(), 11);
        let mut buf = [0u8; 5];
        let n = store.read_at(6, &mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b"world");
    }

    #[test]
    fn test_filestore_truncate_then_append_overwrites_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.bin");

        let mut store = FileStore::open(&path).unwrap();
        store.append(b"0123456789").unwrap();
        store.truncate(5).unwrap();
        assert_eq!(store.len().unwrap(), 5);
        store.append(b"XYZ").unwrap();
        store.sync().unwrap();

        let mut buf = [0u8; 8];
        let n = store.read_at(0, &mut buf).unwrap();
        assert_eq!(n, 8);
        assert_eq!(&buf, b"01234XYZ");
    }

    #[test]
    fn test_filestore_sync_is_durable_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("durable.bin");

        {
            let mut store = FileStore::open(&path).unwrap();
            store.append(b"persisted").unwrap();
            store.sync().unwrap();
        }

        let store = FileStore::open(&path).unwrap();
        let mut buf = [0u8; 9];
        assert_eq!(store.read_at(0, &mut buf).unwrap(), 9);
        assert_eq!(&buf, b"persisted");
    }
}
