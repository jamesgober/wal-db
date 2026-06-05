//! Selection of synchronization primitives.
//!
//! Under `--cfg loom` the concurrency primitives come from `loom`, whose model
//! checker explores every meaningful thread interleaving; otherwise they are the
//! standard-library types. The rest of the crate names them only through here,
//! so the same append and commit logic is what loom verifies and what ships.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::AtomicU64;
#[cfg(loom)]
pub(crate) use loom::sync::{Condvar, Mutex, MutexGuard};
#[cfg(not(loom))]
pub(crate) use std::sync::atomic::AtomicU64;
#[cfg(not(loom))]
pub(crate) use std::sync::{Condvar, Mutex, MutexGuard};
