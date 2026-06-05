//! Segmented file storage.
//!
//! A [`SegmentedStore`] presents one continuous byte address space — the same
//! flat space the log already uses, since an [`Lsn`](crate::Lsn) is a byte offset
//! — striped across fixed-size segment files on disk. A write or read that
//! crosses a segment boundary is split across the two files; a record may span
//! boundaries freely, exactly as PostgreSQL's WAL records span its 16 MiB
//! segments. Bounded segments keep recovery time bounded and let old, fully
//! superseded segments be archived or pruned.
//!
//! Because the address space stays contiguous, the log's append, recovery, and
//! iteration logic do not change at all: a [`Wal`](crate::Wal) over a
//! `SegmentedStore` behaves identically to one over a single file, only with the
//! bytes spread across `00000000000000000000.wal`, `00000000000000000001.wal`,
//! and so on inside a directory.

use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    error::{Result, WalError},
    store::{WalStore, durable_sync, pread_fill, pwrite_all},
};

/// The fixed width of a segment file name, in zero-padded decimal digits — wide
/// enough for any `u64` index and lexically sortable.
const NAME_DIGITS: usize = 20;
/// The segment file extension.
const NAME_EXT: &str = "wal";
/// Name of the file that records the log's head (its lowest surviving offset)
/// after a prefix has been dropped. Absent until the first `truncate_before`.
const HEAD_FILE: &str = "head";
/// Size of the head marker: an 8-byte offset plus a 4-byte CRC32C of it.
const HEAD_FILE_LEN: usize = 12;

/// A [`WalStore`] that stripes one flat byte space across fixed-size segment
/// files in a directory.
///
/// Open one with [`SegmentedStore::open`] and hand it to
/// [`Wal::with_store`](crate::Wal::with_store), or use the
/// [`Wal::open_segmented`](crate::Wal::open_segmented) convenience constructor.
/// Segments are created lazily as the log grows, and [`sync`](WalStore::sync)
/// flushes only the segments with unwritten changes, not the whole history.
///
/// # Examples
///
/// ```
/// use wal_db::{SegmentedStore, Wal};
///
/// # fn main() -> Result<(), wal_db::WalError> {
/// # let dir = tempfile::tempdir().map_err(wal_db::WalError::from)?;
/// // 1 MiB segments. Records larger than a segment simply span several.
/// let store = SegmentedStore::open(dir.path(), 1024 * 1024)?;
/// let wal = Wal::with_store(store)?;
/// wal.append(b"spans nothing yet")?;
/// wal.sync()?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct SegmentedStore {
    dir: PathBuf,
    segment_size: u64,
    segments: RwLock<HashMap<u64, Arc<File>>>,
    /// Highest byte offset ever written, used to decide which segments still
    /// need flushing.
    max_written: AtomicU64,
    /// Index of the lowest segment that may still have unflushed writes. Every
    /// segment below it is full and durable, so `sync` skips it.
    synced_from: AtomicU64,
    /// Lowest offset still present — the start of the lowest segment that has not
    /// been dropped by `truncate_before`. Recovery scans from here.
    head: AtomicU64,
}

impl SegmentedStore {
    /// Open the segmented log in `dir`, creating the directory if needed, with
    /// segments of `segment_size` bytes.
    ///
    /// Existing segment files are picked up so the log can be recovered. The
    /// store does not validate record contents — that is
    /// [`Wal::open`](crate::Wal::open)'s recovery scan, which runs unchanged over
    /// the flat space.
    ///
    /// # Errors
    ///
    /// Returns [`WalError::Io`] if `segment_size` is zero, or if the directory
    /// cannot be created or read.
    pub fn open(dir: impl AsRef<Path>, segment_size: u64) -> Result<Self> {
        if segment_size == 0 {
            return Err(WalError::io(
                "opening the segmented log",
                io::Error::other("segment size must be non-zero"),
            ));
        }
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(|e| WalError::io("creating the log directory", e))?;

        // Find the highest existing segment to compute the current logical length.
        let mut highest: Option<(u64, u64)> = None; // (index, file length)
        for entry in fs::read_dir(&dir).map_err(|e| WalError::io("reading the log directory", e))? {
            let entry = entry.map_err(|e| WalError::io("reading the log directory", e))?;
            if let Some(index) = parse_segment_name(&entry.file_name()) {
                let len = entry
                    .metadata()
                    .map_err(|e| WalError::io("reading segment metadata", e))?
                    .len();
                if highest.is_none_or(|(h, _)| index > h) {
                    highest = Some((index, len));
                }
            }
        }

        let total_len = match highest {
            Some((index, len)) => index.saturating_mul(segment_size).saturating_add(len),
            None => 0,
        };
        let active = total_len / segment_size;
        // The head is the exact record boundary recorded by the last prefix
        // truncation, or 0 for a log that has never had one. It is durable, so
        // recovery resumes from the same place after a crash.
        let head = read_head_file(&dir)?.unwrap_or(0).min(total_len);

        Ok(SegmentedStore {
            dir,
            segment_size,
            segments: RwLock::new(HashMap::new()),
            max_written: AtomicU64::new(total_len),
            // Everything already on disk is treated as durable on open.
            synced_from: AtomicU64::new(active),
            head: AtomicU64::new(head),
        })
    }

    /// The directory holding the segment files.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The configured segment size in bytes.
    #[must_use]
    pub fn segment_size(&self) -> u64 {
        self.segment_size
    }

    fn read_map(&self) -> RwLockReadGuard<'_, HashMap<u64, Arc<File>>> {
        self.segments.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_map(&self) -> RwLockWriteGuard<'_, HashMap<u64, Arc<File>>> {
        self.segments
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Get the handle for segment `index`, creating the file if it does not
    /// exist yet.
    fn segment_for_write(&self, index: u64) -> Result<Arc<File>> {
        if let Some(file) = self.read_map().get(&index) {
            return Ok(Arc::clone(file));
        }
        let mut map = self.write_map();
        if let Some(file) = map.get(&index) {
            return Ok(Arc::clone(file));
        }
        let path = self.dir.join(segment_name(index));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| WalError::io("creating a log segment", e))?;
        let file = Arc::new(file);
        let _ = map.insert(index, Arc::clone(&file));
        Ok(file)
    }

    /// Get the handle for segment `index` only if it already exists, without
    /// creating it. `None` means the log ends before this segment.
    fn segment_for_read(&self, index: u64) -> Result<Option<Arc<File>>> {
        if let Some(file) = self.read_map().get(&index) {
            return Ok(Some(Arc::clone(file)));
        }
        let path = self.dir.join(segment_name(index));
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => {
                let file = Arc::new(file);
                let mut map = self.write_map();
                if let Some(existing) = map.get(&index) {
                    return Ok(Some(Arc::clone(existing)));
                }
                let _ = map.insert(index, Arc::clone(&file));
                Ok(Some(file))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(WalError::io("opening a log segment", error)),
        }
    }

    /// Look up an already-open segment without touching the filesystem.
    fn open_segment(&self, index: u64) -> Option<Arc<File>> {
        self.read_map().get(&index).map(Arc::clone)
    }

    /// Durably record `head` so a later open resumes from the same boundary.
    ///
    /// The marker is checksummed (`[head: u64][crc32c(head): u32]`) so a torn
    /// write of the marker itself is detected on read rather than trusted — a
    /// corrupt marker must never make recovery skip live records.
    fn write_head_file(&self, head: u64) -> Result<()> {
        let mut buf = [0u8; HEAD_FILE_LEN];
        buf[..8].copy_from_slice(&head.to_le_bytes());
        let crc = crc32c::crc32c(&buf[..8]);
        buf[8..].copy_from_slice(&crc.to_le_bytes());

        let path = self.dir.join(HEAD_FILE);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| WalError::io("writing the head marker", e))?;
        pwrite_all(&file, 0, &buf).map_err(|e| WalError::io("writing the head marker", e))?;
        durable_sync(&file).map_err(|e| WalError::io("flushing the head marker", e))?;
        Ok(())
    }
}

/// Read the durably recorded head, or `None` if no prefix has been dropped.
///
/// A marker that is absent, too short, or whose checksum does not match is taken
/// as `None`, so the head falls back to 0 and recovery reads the whole log. That
/// is always safe: a dropped prefix that the marker can no longer vouch for is
/// simply re-read, never skipped.
fn read_head_file(dir: &Path) -> Result<Option<u64>> {
    match fs::read(dir.join(HEAD_FILE)) {
        Ok(bytes) if bytes.len() >= HEAD_FILE_LEN => {
            let head = u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]);
            let stored = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
            if crc32c::crc32c(&bytes[..8]) == stored {
                Ok(Some(head))
            } else {
                Ok(None) // torn or corrupt marker: fall back to full recovery
            }
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(WalError::io("reading the head marker", error)),
    }
}

impl WalStore for SegmentedStore {
    fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        let mut pos = offset;
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let index = pos / self.segment_size;
            let local = pos % self.segment_size;
            let room = (self.segment_size - local) as usize;
            let take = remaining.len().min(room);

            let file = self.segment_for_write(index)?;
            pwrite_all(&file, local, &remaining[..take])
                .map_err(|e| WalError::io("writing a record", e))?;

            pos += take as u64;
            remaining = &remaining[take..];
        }
        let end = offset.saturating_add(bytes.len() as u64);
        let _ = self.max_written.fetch_max(end, Ordering::Relaxed);
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut pos = offset;
        let mut filled = 0;
        while filled < buf.len() {
            let index = pos / self.segment_size;
            let local = pos % self.segment_size;
            let room = (self.segment_size - local) as usize;
            let want = (buf.len() - filled).min(room);

            let Some(file) = self.segment_for_read(index)? else {
                break; // no such segment: the log ends here
            };
            let got = pread_fill(&file, local, &mut buf[filled..filled + want])
                .map_err(|e| WalError::io("reading from the log", e))?;
            filled += got;
            pos += got as u64;
            if got < want {
                break; // short read within a segment: the log ends here
            }
        }
        Ok(filled)
    }

    fn truncate(&self, len: u64) -> Result<()> {
        let last_index = len / self.segment_size;
        let last_local = len % self.segment_size;

        // Walk the directory so segments not currently open are handled too.
        let entries =
            fs::read_dir(&self.dir).map_err(|e| WalError::io("reading the log directory", e))?;
        for entry in entries {
            let entry = entry.map_err(|e| WalError::io("reading the log directory", e))?;
            let Some(index) = parse_segment_name(&entry.file_name()) else {
                continue;
            };
            match index.cmp(&last_index) {
                std::cmp::Ordering::Greater => {
                    // Entirely past the new end: drop the segment.
                    fs::remove_file(entry.path())
                        .map_err(|e| WalError::io("removing a truncated segment", e))?;
                    let _ = self.write_map().remove(&index);
                }
                std::cmp::Ordering::Equal => {
                    // Straddles the new end: shrink it to the kept bytes.
                    let file = self.segment_for_write(index)?;
                    file.set_len(last_local)
                        .map_err(|e| WalError::io("truncating a log segment", e))?;
                }
                std::cmp::Ordering::Less => {}
            }
        }

        self.max_written.store(len, Ordering::Relaxed);
        self.synced_from.store(last_index, Ordering::Relaxed);
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        let written = self.max_written.load(Ordering::Acquire);
        if written == 0 {
            return Ok(());
        }
        // The segment holding the last written byte.
        let active = (written - 1) / self.segment_size;
        let from = self.synced_from.load(Ordering::Acquire);

        for index in from..=active {
            if let Some(file) = self.open_segment(index) {
                durable_sync(&file).map_err(|e| WalError::io("flushing to stable storage", e))?;
            }
        }
        // Every segment below `active` is now full and durable; the active one
        // may still grow, so it stays in the window for the next sync.
        self.synced_from.store(active, Ordering::Release);
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.max_written.load(Ordering::Acquire))
    }

    fn head(&self) -> Result<u64> {
        Ok(self.head.load(Ordering::Acquire))
    }

    fn truncate_before(&self, offset: u64) -> Result<u64> {
        let written = self.max_written.load(Ordering::Acquire);
        // The new head is the caller's record boundary, clamped so it never moves
        // backwards and never past the end. It is a *record* boundary, not a
        // segment boundary: because records span segments, the lowest surviving
        // segment may start mid-record, so the head — and where recovery begins —
        // must be the exact offset the caller gave.
        let prev = self.head.load(Ordering::Acquire);
        let new_head = offset.clamp(prev, written);

        // Persist the head durably *before* deleting anything: a crash mid-delete
        // then recovers from the right boundary, and the leftover segments below it
        // are harmless dead space.
        self.write_head_file(new_head)?;

        // Delete every segment entirely below the one that holds the new head; that
        // segment, and the one holding the most recent records, are always kept.
        let last_segment = written.saturating_sub(1) / self.segment_size;
        let keep_from = (new_head / self.segment_size).min(last_segment);
        let entries =
            fs::read_dir(&self.dir).map_err(|e| WalError::io("reading the log directory", e))?;
        for entry in entries {
            let entry = entry.map_err(|e| WalError::io("reading the log directory", e))?;
            let Some(index) = parse_segment_name(&entry.file_name()) else {
                continue;
            };
            if index < keep_from {
                // Drop our handle before unlinking — Windows refuses to remove a
                // file that still has an open handle.
                let _ = self.write_map().remove(&index);
                fs::remove_file(entry.path())
                    .map_err(|e| WalError::io("removing a dropped segment", e))?;
            }
        }

        self.head.store(new_head, Ordering::Release);
        Ok(new_head)
    }
}

/// The file name for segment `index`: zero-padded decimal, then `.wal`.
fn segment_name(index: u64) -> String {
    format!("{index:0NAME_DIGITS$}.{NAME_EXT}")
}

/// Parse a segment index out of a file name, or `None` if it is not a segment
/// file. Only names of exactly the expected shape are accepted, so unrelated
/// files in the directory are ignored.
fn parse_segment_name(name: &OsStr) -> Option<u64> {
    let name = name.to_str()?;
    let stem = name.strip_suffix(&format!(".{NAME_EXT}"))?;
    if stem.len() != NAME_DIGITS || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_name_roundtrips() {
        assert_eq!(segment_name(0), "00000000000000000000.wal");
        assert_eq!(segment_name(42), "00000000000000000042.wal");
        assert_eq!(
            parse_segment_name(OsStr::new("00000000000000000042.wal")),
            Some(42)
        );
        assert_eq!(parse_segment_name(OsStr::new("README.md")), None);
        assert_eq!(parse_segment_name(OsStr::new("42.wal")), None);
        assert_eq!(
            parse_segment_name(OsStr::new("0000000000000000004x.wal")),
            None
        );
    }

    #[test]
    fn test_write_read_within_one_segment() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentedStore::open(dir.path(), 64).unwrap();
        store.write_at(0, b"hello").unwrap();
        store.sync().unwrap();

        let mut buf = [0u8; 5];
        assert_eq!(store.read_at(0, &mut buf).unwrap(), 5);
        assert_eq!(&buf, b"hello");
        assert_eq!(store.len().unwrap(), 5);
    }

    #[test]
    fn test_write_spans_segment_boundary() {
        let dir = tempfile::tempdir().unwrap();
        // 8-byte segments force a 12-byte write to span two segments.
        let store = SegmentedStore::open(dir.path(), 8).unwrap();
        store.write_at(0, b"ABCDEFGHIJKL").unwrap(); // 12 bytes -> segments 0 and 1
        store.sync().unwrap();

        // Two segment files exist.
        assert!(dir.path().join("00000000000000000000.wal").exists());
        assert!(dir.path().join("00000000000000000001.wal").exists());

        let mut buf = [0u8; 12];
        assert_eq!(store.read_at(0, &mut buf).unwrap(), 12);
        assert_eq!(&buf, b"ABCDEFGHIJKL");
    }

    #[test]
    fn test_read_at_arbitrary_offset_across_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentedStore::open(dir.path(), 4).unwrap();
        store.write_at(0, b"0123456789").unwrap();
        let mut buf = [0u8; 5];
        let n = store.read_at(3, &mut buf).unwrap(); // spans segments 0,1,2
        assert_eq!(n, 5);
        assert_eq!(&buf, b"34567");
    }

    #[test]
    fn test_reopen_reports_correct_length() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = SegmentedStore::open(dir.path(), 8).unwrap();
            store.write_at(0, b"ABCDEFGHIJKLM").unwrap(); // 13 bytes -> segs 0(full),1(partial)
            store.sync().unwrap();
            assert_eq!(store.len().unwrap(), 13);
        }
        let store = SegmentedStore::open(dir.path(), 8).unwrap();
        assert_eq!(store.len().unwrap(), 13);
        let mut buf = [0u8; 13];
        assert_eq!(store.read_at(0, &mut buf).unwrap(), 13);
        assert_eq!(&buf, b"ABCDEFGHIJKLM");
    }

    #[test]
    fn test_truncate_removes_later_segments() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentedStore::open(dir.path(), 8).unwrap();
        store.write_at(0, &[0xAB; 30]).unwrap(); // segments 0..=3
        store.sync().unwrap();
        assert!(dir.path().join("00000000000000000003.wal").exists());

        store.truncate(10).unwrap(); // keep segment 0 (full) + 2 bytes of segment 1
        assert_eq!(store.len().unwrap(), 10);
        assert!(dir.path().join("00000000000000000001.wal").exists());
        assert!(!dir.path().join("00000000000000000002.wal").exists());
        assert!(!dir.path().join("00000000000000000003.wal").exists());

        let mut buf = [0u8; 16];
        assert_eq!(store.read_at(0, &mut buf).unwrap(), 10);
    }

    #[test]
    fn test_read_past_end_is_short() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentedStore::open(dir.path(), 8).unwrap();
        store.write_at(0, b"abc").unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(store.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(store.read_at(100, &mut buf).unwrap(), 0);
    }
}
