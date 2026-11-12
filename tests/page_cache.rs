//! Linux 6.5 or newer: after a sync returns, no page of the log is still
//! dirty in the page cache, read with `cachestat(2)`. A power loss drops
//! exactly those pages, so this is the durability promise checked from the
//! kernel's side. On an older kernel (no `cachestat`) the tests print a note
//! and pass.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use wal_db::{SegmentedStore, Wal, WalStore};

#[repr(C)]
struct Range {
    off: u64,
    len: u64,
}

#[repr(C)]
#[derive(Default)]
struct Stat {
    nr_cache: u64,
    nr_dirty: u64,
    nr_writeback: u64,
    nr_evicted: u64,
    nr_recently_evicted: u64,
}

const SYS_CACHESTAT: libc::c_long = 451;

/// Dirty and writeback pages of `path`, or `None` without `cachestat`.
fn unflushed(path: &Path) -> Option<u64> {
    let file = std::fs::File::open(path).unwrap();
    let range = Range { off: 0, len: 0 };
    let mut stat = Stat::default();
    // SAFETY: `cachestat(fd, &range, &stat, 0)` reads `range` and writes
    // `stat`, both live and of the layout the kernel expects; `fd` is open
    // for the whole call.
    let rc = unsafe {
        libc::syscall(
            SYS_CACHESTAT,
            file.as_raw_fd(),
            &range as *const Range,
            &mut stat as *mut Stat,
            0u32,
        )
    };
    if rc != 0 {
        eprintln!(
            "cachestat unavailable ({}): skipped",
            std::io::Error::last_os_error()
        );
        return None;
    }
    Some(stat.nr_dirty + stat.nr_writeback)
}

fn segment(dir: &Path, index: u64) -> PathBuf {
    dir.join(format!("{index:020}.wal"))
}

const SIZE: u64 = 64 * 1024;

/// A record reserved first but written last lands in the older segment after
/// a later record and a sync: the sync that covers it flushes it.
#[test]
fn test_a_late_write_into_an_older_segment_reaches_the_disk() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let store = SegmentedStore::open(dir.path(), SIZE).unwrap();
    store.write_at(0, &vec![1u8; (SIZE - 10) as usize]).unwrap();
    store.sync().unwrap();
    store.write_at(SIZE + 10, &[2u8; 100]).unwrap();
    store.sync().unwrap();
    store.write_at(SIZE - 10, &[3u8; 20]).unwrap();
    store.sync().unwrap();
    for index in [0, 1] {
        if let Some(pages) = unflushed(&segment(dir.path(), index)) {
            assert_eq!(pages, 0, "segment {index} still has unflushed pages");
        }
    }
}

/// Records a stopped process left unsynced in older segments are flushed by
/// the next open, before any later record is acknowledged.
#[test]
fn test_a_reopen_flushes_what_a_stopped_process_left() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    {
        let wal = Wal::open_segmented(dir.path(), SIZE).unwrap();
        for _ in 0..200 {
            let _ = wal.append(&[5u8; 500]).unwrap(); // about 100 KiB: two segments
        }
        // Dropped without a sync: the bytes survive only in the page cache.
    }
    let wal = Wal::open_segmented(dir.path(), SIZE).unwrap();
    assert_eq!(wal.iter().unwrap().count(), 200);
    let _ = wal.append_and_sync(b"acknowledged").unwrap();
    for index in [0, 1] {
        if let Some(pages) = unflushed(&segment(dir.path(), index)) {
            assert_eq!(pages, 0, "segment {index} still has unflushed pages");
        }
    }
}
