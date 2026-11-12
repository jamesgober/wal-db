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
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crate::{
    error::{Result, WalError},
    store::{
        WalStore, durable_sync, flush_dir_if_dirty, parent_dir, pread_fill, pwrite_all, sync_dir,
    },
};

/// The fixed width of a segment file name, in zero-padded decimal digits — wide
/// enough for any `u64` index and lexically sortable.
const NAME_DIGITS: usize = 20;
/// The segment file extension.
const NAME_EXT: &str = "wal";
/// Name of the file that records the log's head (its lowest surviving offset)
/// after a prefix has been dropped. Absent until the first `truncate_before`.
const HEAD_FILE: &str = "head";
/// The marker is written here, flushed, and renamed over [`HEAD_FILE`], so a
/// crash leaves the old marker or the new one and never a torn one.
const HEAD_TMP_FILE: &str = "head.tmp";
/// Size of the head marker: an 8-byte offset plus a 4-byte CRC32C of it.
const HEAD_FILE_LEN: usize = 12;
/// Name of the file that records the segment size the log was written with
/// (since 1.4). Readers before 1.4 ignore it.
const META_FILE: &str = "meta";
/// Written here, flushed, and renamed over [`META_FILE`].
const META_TMP_FILE: &str = "meta.tmp";
/// The meta file's magic.
const META_MAGIC: &[u8; 8] = b"WALSEGSZ";
/// Size of the meta file: magic, the segment size as a u64, and a CRC32C of
/// both.
const META_FILE_LEN: usize = 8 + 8 + 4;

/// A [`WalStore`] that stripes one flat byte space across fixed-size segment
/// files in a directory.
///
/// Open one with [`SegmentedStore::open`] and hand it to
/// [`Wal::with_store`](crate::Wal::with_store), or use the
/// [`Wal::open_segmented`](crate::Wal::open_segmented) convenience constructor.
/// Segments are created lazily as the log grows, and [`sync`](WalStore::sync)
/// flushes every segment written since the last sync, not the whole history.
///
/// The segment size is recorded in a `meta` file next to the segments; opening
/// with another size is refused rather than reading the bytes at the wrong
/// offsets.
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
    segments: RwLock<HashMap<u64, Arc<Segment>>>,
    /// Highest byte offset ever written: the logical length.
    max_written: AtomicU64,
    /// Lowest offset still present — the start of the lowest segment that has not
    /// been dropped by `truncate_before`. Recovery scans from here.
    head: AtomicU64,
    /// A segment file was created or removed since the directory was last
    /// flushed. Set at open too: a sync must also cover names left unflushed by
    /// a process that stopped before flushing them.
    dir_dirty: AtomicBool,
    /// This store created the log directory, whose own name lives in its parent.
    parent_dirty: AtomicBool,
}

/// One open segment file and whether it holds bytes written since its last
/// flush.
#[derive(Debug)]
struct Segment {
    file: File,
    /// Set after every write to the segment; cleared by the sync that flushes
    /// it. A sync flushes every segment with the flag set, so bytes written
    /// late into an older segment (a record whose writer finished after a
    /// later one) are flushed by the sync that covers them.
    dirty: AtomicBool,
}

impl Segment {
    fn new(file: File, dirty: bool) -> Arc<Segment> {
        Arc::new(Segment {
            file,
            dirty: AtomicBool::new(dirty),
        })
    }
}

impl SegmentedStore {
    /// Open the segmented log in `dir`, creating the directory if needed, with
    /// segments of `segment_size` bytes.
    ///
    /// Existing segment files are picked up so the log can be recovered. The
    /// store does not validate record contents — that is
    /// [`Wal::open`](crate::Wal::open)'s recovery scan, which runs unchanged over
    /// the flat space. Every existing segment is flushed first: a process that
    /// stopped without syncing may have left bytes only in the page cache, and
    /// recovery must not trust what a power loss could still take away.
    ///
    /// # Errors
    ///
    /// - [`WalError::Io`] if `segment_size` is zero, or if the directory or a
    ///   segment cannot be created, read, or flushed.
    /// - [`WalError::Corruption`] if the log was written with another segment
    ///   size, or if its first segments were dropped and the head marker that
    ///   says where it now starts is missing or damaged. Nothing is deleted.
    pub fn open(dir: impl AsRef<Path>, segment_size: u64) -> Result<Self> {
        if segment_size == 0 {
            return Err(WalError::io(
                "opening the segmented log",
                io::Error::other("segment size must be non-zero"),
            ));
        }
        let dir = dir.as_ref().to_path_buf();
        let created = !dir.is_dir();
        fs::create_dir_all(&dir).map_err(|e| WalError::io("creating the log directory", e))?;

        // Every segment, by index, with its file length.
        let mut found: Vec<(u64, u64)> = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| WalError::io("reading the log directory", e))? {
            let entry = entry.map_err(|e| WalError::io("reading the log directory", e))?;
            if let Some(index) = parse_segment_name(&entry.file_name()) {
                let len = entry
                    .metadata()
                    .map_err(|e| WalError::io("reading segment metadata", e))?
                    .len();
                found.push((index, len));
            }
        }
        found.sort_unstable();

        check_segment_size(&dir, segment_size, &found)?;

        let total_len = match found.last() {
            Some(&(index, len)) => index.saturating_mul(segment_size).saturating_add(len),
            None => 0,
        };
        // The head is the exact record boundary recorded by the last prefix
        // truncation, or 0 for a log that has never had one. It is durable, so
        // recovery resumes from the same place after a crash.
        let lowest = found.first().map_or(0, |&(index, _)| index);
        let head = match read_head_file(&dir)? {
            Some(head) => head.min(total_len),
            // No marker (or a damaged one) and segment 0 still here: the whole
            // log is present and recovery reads it from the start.
            None if lowest == 0 => 0,
            None => {
                return Err(WalError::corruption(
                    lowest.saturating_mul(segment_size),
                    "the log's first segments were dropped and its head marker is missing or damaged",
                ));
            }
        };
        if !found.is_empty() && head / segment_size < lowest {
            return Err(WalError::corruption(
                head,
                "the segment holding the log's head is missing",
            ));
        }

        // Flush every segment before recovery reads it, and keep the handles.
        let mut segments = HashMap::with_capacity(found.len());
        for &(index, _) in &found {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(dir.join(segment_name(index)))
                .map_err(|e| WalError::io("opening a log segment", e))?;
            durable_sync(&file).map_err(|e| WalError::io("flushing a log segment", e))?;
            let _ = segments.insert(index, Segment::new(file, false));
        }

        Ok(SegmentedStore {
            dir,
            segment_size,
            segments: RwLock::new(segments),
            max_written: AtomicU64::new(total_len),
            head: AtomicU64::new(head),
            dir_dirty: AtomicBool::new(true),
            parent_dirty: AtomicBool::new(created),
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

    fn read_map(&self) -> RwLockReadGuard<'_, HashMap<u64, Arc<Segment>>> {
        self.segments.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_map(&self) -> RwLockWriteGuard<'_, HashMap<u64, Arc<Segment>>> {
        self.segments
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Get segment `index`, creating the file if it does not exist yet.
    fn segment_for_write(&self, index: u64) -> Result<Arc<Segment>> {
        if let Some(segment) = self.read_map().get(&index) {
            return Ok(Arc::clone(segment));
        }
        let mut map = self.write_map();
        if let Some(segment) = map.get(&index) {
            return Ok(Arc::clone(segment));
        }
        let path = self.dir.join(segment_name(index));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| WalError::io("creating a log segment", e))?;
        // Set before the write that follows completes, so the sync covering it
        // flushes the new name. The append path pays one atomic store per
        // segment; the directory flush itself happens in `sync`.
        self.dir_dirty.store(true, Ordering::Release);
        let segment = Segment::new(file, false);
        let _ = map.insert(index, Arc::clone(&segment));
        Ok(segment)
    }

    /// Get segment `index` only if it already exists, without creating it.
    /// `None` means the log ends before this segment.
    fn segment_for_read(&self, index: u64) -> Result<Option<Arc<Segment>>> {
        if let Some(segment) = self.read_map().get(&index) {
            return Ok(Some(Arc::clone(segment)));
        }
        let path = self.dir.join(segment_name(index));
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => {
                let mut map = self.write_map();
                if let Some(existing) = map.get(&index) {
                    return Ok(Some(Arc::clone(existing)));
                }
                let segment = Segment::new(file, false);
                let _ = map.insert(index, Arc::clone(&segment));
                Ok(Some(segment))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(WalError::io("opening a log segment", error)),
        }
    }

    /// Durably record `head` so a later open resumes from the same boundary.
    ///
    /// The marker is checksummed (`[head: u64][crc32c(head): u32]`) so a torn
    /// write of the marker itself is detected on read rather than trusted — a
    /// corrupt marker must never make recovery skip live records.
    fn write_head_file(&self, head: u64) -> Result<()> {
        let mut buf = [0u8; HEAD_FILE_LEN];
        buf[..8].copy_from_slice(&head.to_le_bytes());
        let crc = crate::crc::crc32c(&buf[..8]);
        buf[8..].copy_from_slice(&crc.to_le_bytes());
        write_small_file(&self.dir, HEAD_TMP_FILE, HEAD_FILE, &buf, "the head marker")
    }
}

/// Write `bytes` to `dir/name` atomically and durably: through `tmp`,
/// flushed, renamed over `name`, and the directory flushed.
fn write_small_file(
    dir: &Path,
    tmp: &str,
    name: &str,
    bytes: &[u8],
    what: &'static str,
) -> Result<()> {
    let tmp = dir.join(tmp);
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|e| WalError::io(what, e))?;
    pwrite_all(&file, 0, bytes).map_err(|e| WalError::io(what, e))?;
    durable_sync(&file).map_err(|e| WalError::io(what, e))?;
    // Windows refuses to rename a file that still has an open handle.
    drop(file);
    fs::rename(&tmp, dir.join(name)).map_err(|e| WalError::io(what, e))?;
    sync_dir(dir).map_err(|e| WalError::io("flushing the log directory", e))
}

/// Check `segment_size` against the size the log in `dir` was written with,
/// and record it for a log that has no record of it yet.
///
/// A log from 1.4 on records its size in the `meta` file. For an older log
/// the segments' lengths tell: every segment before the last one is full, so
/// one that is longer than `segment_size`, or a set of them none of which is
/// exactly `segment_size` long, means another size. (A segment before the
/// last may be short only if a crash cut writes that were still in flight.)
fn check_segment_size(dir: &Path, segment_size: u64, found: &[(u64, u64)]) -> Result<()> {
    let mismatch = || {
        WalError::corruption(
            0,
            "the log was written with another segment size: open it with that size",
        )
    };
    if let Some(recorded) = read_meta_file(dir)? {
        return if recorded == segment_size {
            Ok(())
        } else {
            Err(mismatch())
        };
    }
    let before_last = found.len().saturating_sub(1);
    let full = &found[..before_last];
    if found.iter().any(|&(_, len)| len > segment_size)
        || (!full.is_empty() && !full.iter().any(|&(_, len)| len == segment_size))
    {
        return Err(mismatch());
    }
    let mut buf = [0u8; META_FILE_LEN];
    buf[..8].copy_from_slice(META_MAGIC);
    buf[8..16].copy_from_slice(&segment_size.to_le_bytes());
    let crc = crate::crc::crc32c(&buf[..16]);
    buf[16..].copy_from_slice(&crc.to_le_bytes());
    write_small_file(dir, META_TMP_FILE, META_FILE, &buf, "the log's meta file")
}

/// The segment size recorded in the meta file, or `None` when there is none.
fn read_meta_file(dir: &Path) -> Result<Option<u64>> {
    match fs::read(dir.join(META_FILE)) {
        Ok(bytes) => {
            if bytes.len() != META_FILE_LEN
                || &bytes[..8] != META_MAGIC
                || crate::crc::crc32c(&bytes[..16])
                    != u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]])
            {
                return Err(WalError::corruption(0, "the log's meta file is damaged"));
            }
            let mut size = [0u8; 8];
            size.copy_from_slice(&bytes[8..16]);
            Ok(Some(u64::from_le_bytes(size)))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(WalError::io("reading the log's meta file", error)),
    }
}

/// Read the durably recorded head, or `None` if no prefix has been dropped
/// or the marker is unreadable.
///
/// A marker that is absent, too short, or whose checksum does not match is
/// `None`. The caller then reads the log from 0 only if segment 0 is still
/// there; once a prefix has been dropped it refuses to open instead, since
/// recovery from 0 would find nothing and cut the log.
fn read_head_file(dir: &Path) -> Result<Option<u64>> {
    match fs::read(dir.join(HEAD_FILE)) {
        Ok(bytes) if bytes.len() >= HEAD_FILE_LEN => {
            let head = u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]);
            let stored = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
            if crate::crc::crc32c(&bytes[..8]) == stored {
                Ok(Some(head))
            } else {
                Ok(None)
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

            let segment = self.segment_for_write(index)?;
            pwrite_all(&segment.file, local, &remaining[..take])
                .map_err(|e| WalError::io("writing a record", e))?;
            // After the bytes are in the file: a sync that reads the flag as set
            // flushes them; one that cleared it just before has flushed them too.
            segment.dirty.store(true, Ordering::Release);

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

            let Some(segment) = self.segment_for_read(index)? else {
                break; // no such segment: the log ends here
            };
            let got = pread_fill(&segment.file, local, &mut buf[filled..filled + want])
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
            // A segment that would keep no bytes is removed, not emptied: an
            // emptied file whose shrink a power loss undid would bring the
            // dropped records back at the very offset they had.
            let remove = index > last_index || (index == last_index && last_local == 0);
            if remove {
                // Drop our handle before unlinking — Windows refuses to remove a
                // file that still has an open handle.
                let _ = self.write_map().remove(&index);
                fs::remove_file(entry.path())
                    .map_err(|e| WalError::io("removing a truncated segment", e))?;
                self.dir_dirty.store(true, Ordering::Release);
            } else if index == last_index {
                // Straddles the new end: shrink it and flush the shrink now.
                let segment = self.segment_for_write(index)?;
                segment
                    .file
                    .set_len(last_local)
                    .map_err(|e| WalError::io("truncating a log segment", e))?;
                durable_sync(&segment.file)
                    .map_err(|e| WalError::io("flushing a truncated segment", e))?;
            }
        }

        self.max_written.store(len, Ordering::Relaxed);
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        // Directory first: on macOS only the file flush below empties the
        // drive's write cache.
        flush_dir_if_dirty(&self.dir_dirty, &self.dir)?;
        flush_dir_if_dirty(&self.parent_dirty, &parent_dir(&self.dir))?;

        // Every segment written since its last flush, whichever it is: a record
        // whose writer finished late may have landed in an older one.
        let dirty: Vec<Arc<Segment>> = self
            .read_map()
            .values()
            .filter(|segment| segment.dirty.load(Ordering::Acquire))
            .cloned()
            .collect();
        for segment in dirty {
            if segment.dirty.swap(false, Ordering::AcqRel) {
                if let Err(e) = durable_sync(&segment.file) {
                    segment.dirty.store(true, Ordering::Release);
                    return Err(WalError::io("flushing to stable storage", e));
                }
            }
        }
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
                self.dir_dirty.store(true, Ordering::Release);
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

    fn dir_dirty(store: &SegmentedStore) -> bool {
        store.dir_dirty.load(Ordering::Acquire)
    }

    fn segment_dirty(store: &SegmentedStore, index: u64) -> bool {
        store.read_map()[&index].dirty.load(Ordering::Acquire)
    }

    /// The order the commit layer produces when a record that straddles a
    /// segment boundary is reserved first but written last: a later record
    /// completes, a sync runs, then the late bytes land in the older segment.
    /// The sync that covers them must flush that older segment (before 1.4
    /// it flushed only from the segment of the highest write on).
    #[test]
    fn test_a_late_write_into_an_older_segment_is_flushed() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentedStore::open(dir.path(), 64).unwrap();
        store.write_at(0, &[1u8; 54]).unwrap();
        store.sync().unwrap();
        store.write_at(74, &[2u8; 30]).unwrap(); // the later record, segment 1
        store.sync().unwrap();
        assert!(!segment_dirty(&store, 0) && !segment_dirty(&store, 1));
        store.write_at(54, &[3u8; 20]).unwrap(); // the late one, segments 0 and 1
        assert!(segment_dirty(&store, 0) && segment_dirty(&store, 1));
        store.sync().unwrap();
        assert!(!segment_dirty(&store, 0) && !segment_dirty(&store, 1));
    }

    /// A reopened log flushes every segment before recovery trusts it: a
    /// process that stopped without syncing may have left bytes only in the
    /// page cache.
    #[test]
    fn test_open_flushes_every_existing_segment() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = SegmentedStore::open(dir.path(), 64).unwrap();
            store.write_at(0, &[1u8; 200]).unwrap(); // four segments, never synced
        }
        let store = SegmentedStore::open(dir.path(), 64).unwrap();
        for index in 0..4 {
            assert!(!segment_dirty(&store, index), "segment {index}");
        }
        assert_eq!(store.len().unwrap(), 200);
    }

    #[test]
    fn test_new_segment_marks_the_directory_until_the_next_sync() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentedStore::open(dir.path(), 16).unwrap();
        assert!(dir_dirty(&store), "a fresh open has names to flush");

        store.write_at(0, &[1; 8]).unwrap();
        store.sync().unwrap();
        assert!(!dir_dirty(&store));

        // Same segment: no new name, so no directory flush.
        store.write_at(8, &[1; 4]).unwrap();
        assert!(!dir_dirty(&store));

        // Crossing into a new segment creates a name.
        store.write_at(12, &[1; 8]).unwrap();
        assert!(dir_dirty(&store));
        store.sync().unwrap();
        assert!(!dir_dirty(&store));
    }

    #[test]
    fn test_removing_a_segment_marks_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentedStore::open(dir.path(), 16).unwrap();
        store.write_at(0, &[1; 40]).unwrap();
        store.sync().unwrap();

        store.truncate(8).unwrap();
        assert!(dir_dirty(&store));
        store.sync().unwrap();
        assert!(!dir_dirty(&store));
    }

    #[test]
    fn test_only_a_created_directory_marks_its_parent() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("log");

        let store = SegmentedStore::open(&path, 16).unwrap();
        assert!(store.parent_dirty.load(Ordering::Acquire));
        store.sync().unwrap();
        assert!(!store.parent_dirty.load(Ordering::Acquire));
        drop(store);

        let store = SegmentedStore::open(&path, 16).unwrap();
        assert!(!store.parent_dirty.load(Ordering::Acquire));
    }

    #[test]
    fn test_head_marker_replacement_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentedStore::open(dir.path(), 16).unwrap();
        store.write_head_file(1).unwrap();
        store.write_head_file(2).unwrap();
        assert_eq!(read_head_file(dir.path()).unwrap(), Some(2));
        assert!(!dir.path().join(HEAD_TMP_FILE).exists());
    }

    #[test]
    fn test_torn_temporary_marker_never_touches_the_live_one() {
        // A crash while writing the replacement: the live marker is what counts.
        let dir = tempfile::tempdir().unwrap();
        let store = SegmentedStore::open(dir.path(), 16).unwrap();
        store.write_head_file(7).unwrap();
        fs::write(dir.path().join(HEAD_TMP_FILE), [0xAB; 3]).unwrap();
        assert_eq!(read_head_file(dir.path()).unwrap(), Some(7));

        // The next replacement overwrites the leftover.
        store.write_head_file(9).unwrap();
        assert_eq!(read_head_file(dir.path()).unwrap(), Some(9));
    }

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
