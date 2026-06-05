//! Group-commit coordination.
//!
//! The append path is lock-free: a writer reserves a byte range with one atomic
//! step and writes its record into that range, concurrently with other writers.
//! That leaves two jobs this module owns:
//!
//! 1. **The contiguous-written watermark.** Writers finish out of order — a
//!    writer that reserved a later range can complete before one that reserved an
//!    earlier range. A record is only *recoverable* once every byte before it is
//!    written, because recovery stops at the first gap. So durability is defined
//!    against the highest offset below which there are no gaps, not against the
//!    reservation tail.
//!
//! 2. **Group commit.** When several threads call `sync` at once, only one of
//!    them should issue the fsync; the rest wait for it and return. This is the
//!    throughput multiplier that makes a WAL viable: N commits, one flush.
//!
//! Both are handled by a single short critical section. The expensive work —
//! framing, the write syscall, the fsync itself — happens outside the lock; the
//! lock only orders the watermark update and elects the fsync leader.

use std::collections::BTreeMap;

use crate::{
    error::{Result, WalError},
    store::WalStore,
    sync::{Condvar, Mutex, MutexGuard},
};

/// Coordinates the durable watermark and group commit for one log.
#[derive(Debug)]
pub(crate) struct Commit {
    state: Mutex<State>,
    cond: Condvar,
}

#[derive(Debug)]
struct State {
    /// Highest offset below which every byte is written (no gaps).
    committed: u64,
    /// Highest offset below which every byte is on stable storage.
    durable: u64,
    /// Completed writes that are not yet contiguous with `committed`, keyed by
    /// start offset. Drained into `committed` as the gaps before them fill.
    pending: BTreeMap<u64, u64>,
    /// Whether a thread is currently inside the fsync syscall as the leader.
    syncing: bool,
    /// Number of threads waiting on `cond`, so appenders can skip the notify
    /// when nobody is listening — the common case during a burst of appends.
    waiters: usize,
    /// Offset of the first failed write, if any. Nothing at or beyond it can be
    /// made durable: the log is effectively truncated here.
    poison: Option<u64>,
}

impl Commit {
    /// Create a coordinator whose watermarks start at `recovered` — the end of
    /// the last intact record found on open.
    pub(crate) fn new(recovered: u64) -> Self {
        Commit {
            state: Mutex::new(State {
                committed: recovered,
                durable: recovered,
                pending: BTreeMap::new(),
                syncing: false,
                waiters: 0,
                poison: None,
            }),
            cond: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Record that the write covering `[start, end)` completed successfully.
    pub(crate) fn mark_written(&self, start: u64, end: u64) {
        let mut state = self.lock();
        state.insert_region(start, end);
        if state.waiters > 0 {
            self.cond.notify_all();
        }
    }

    /// Record that the write reserved at `start` failed. The log is poisoned
    /// from there on: recovery will stop at the gap, and syncs covering it error.
    pub(crate) fn mark_failed(&self, start: u64) {
        let mut state = self.lock();
        state.poison = Some(state.poison.map_or(start, |p| p.min(start)));
        if state.waiters > 0 {
            self.cond.notify_all();
        }
    }

    /// The contiguous-written watermark — the end of the readable prefix.
    pub(crate) fn committed(&self) -> u64 {
        self.lock().committed
    }

    /// Make every byte below `target` durable, coalescing concurrent callers
    /// into a single fsync.
    ///
    /// Returns once `target` is on stable storage. If a write below `target`
    /// failed, returns [`WalError::Corruption`] instead — that data cannot be
    /// made durable.
    pub(crate) fn sync_to<S: WalStore>(&self, store: &S, target: u64) -> Result<()> {
        let mut state = self.lock();
        loop {
            if let Some(poison) = state.poison {
                if target > poison {
                    return Err(WalError::corruption(
                        poison,
                        "a record write failed; the log is truncated at this offset",
                    ));
                }
            }
            if state.durable >= target {
                return Ok(());
            }

            if state.committed >= target && !state.syncing {
                // Become the fsync leader: flush everything contiguously written,
                // not just up to `target`, so one syscall serves later callers too.
                state.syncing = true;
                let flush_to = state.committed;
                drop(state);

                let result = store.sync();

                state = self.lock();
                state.syncing = false;
                match result {
                    Ok(()) => {
                        if state.durable < flush_to {
                            state.durable = flush_to;
                        }
                        self.cond.notify_all();
                        // Loop: `durable >= target` now holds, so we return.
                    }
                    Err(error) => {
                        self.cond.notify_all();
                        return Err(error);
                    }
                }
            } else {
                // Either a gap below `target` is still being written, or another
                // thread is the fsync leader. Wait to be woken and re-check.
                state.waiters += 1;
                state = match self.cond.wait(state) {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                state.waiters -= 1;
            }
        }
    }
}

impl State {
    /// Fold a completed region into the watermark, advancing `committed` across
    /// any pending regions that are now contiguous.
    fn insert_region(&mut self, start: u64, end: u64) {
        if start == self.committed {
            self.committed = end;
            while let Some(next_end) = self.pending.remove(&self.committed) {
                self.committed = next_end;
            }
        } else {
            // Out of order: a gap before `start` is still open. Hold the region
            // until the gap fills. (Disjoint reservations guarantee start > committed.)
            let _ = self.pending.insert(start, end);
        }
    }
}

#[cfg(all(test, not(loom)))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    #[test]
    fn test_in_order_completion_advances_committed() {
        let commit = Commit::new(0);
        commit.mark_written(0, 10);
        assert_eq!(commit.committed(), 10);
        commit.mark_written(10, 25);
        assert_eq!(commit.committed(), 25);
    }

    #[test]
    fn test_out_of_order_completion_holds_until_gap_fills() {
        let commit = Commit::new(0);
        // The [10, 25) write finishes before [0, 10): committed must not jump.
        commit.mark_written(10, 25);
        assert_eq!(commit.committed(), 0);
        // Filling the gap merges both regions at once.
        commit.mark_written(0, 10);
        assert_eq!(commit.committed(), 25);
    }

    #[test]
    fn test_sync_to_covered_target_is_durable() {
        let store = MemStore::new();
        let commit = Commit::new(0);
        commit.mark_written(0, 32);
        commit.sync_to(&store, 32).unwrap();
        // A second sync to the same target is already satisfied.
        commit.sync_to(&store, 32).unwrap();
    }

    #[test]
    fn test_sync_past_poison_errors() {
        let store = MemStore::new();
        let commit = Commit::new(0);
        commit.mark_written(0, 10);
        commit.mark_failed(10);
        // Below the poison is fine.
        commit.sync_to(&store, 10).unwrap();
        // At or past it is an error.
        let err = commit.sync_to(&store, 11).unwrap_err();
        assert!(matches!(err, WalError::Corruption { .. }));
    }
}
