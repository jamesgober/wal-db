//! Log configuration.

/// The default maximum record size: 64 MiB.
///
/// Generous enough for any realistic single record, small enough that a
/// corrupt length prefix cannot request a wild allocation.
const DEFAULT_MAX_RECORD_SIZE: u32 = 64 * 1024 * 1024;

/// How [`Wal::iter`](crate::Wal::iter) reacts to a damaged record.
///
/// This governs *iteration*, not what [`Wal::open`](crate::Wal::open) does with
/// damage it finds while opening; that is [`OpenPolicy`]. It matters when a
/// record in the middle of an already-recovered log is damaged — bit rot, say —
/// and you are reading the log back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecoveryPolicy {
    /// Stop at the first damaged record: yield it as a single
    /// [`WalError::Corruption`](crate::WalError::Corruption), then end.
    ///
    /// The default, and the right choice for an append-only log, where a damaged
    /// record means everything after it is untrustworthy.
    StopAtFirstError,

    /// Skip past a damaged record and keep going, for forensic or partial
    /// recovery.
    ///
    /// Each damaged record is still yielded as a
    /// [`WalError::Corruption`](crate::WalError::Corruption) — the loss is never
    /// silent — but iteration then resumes at the next record. This is only
    /// possible while a damaged record's length prefix is intact enough to locate
    /// the next one; a record whose length itself is unreadable (a short read, or
    /// a length past the maximum) still stops iteration, because there is no way
    /// to know where the following record begins.
    SkipBadRecords,
}

/// What opening a log does when its scan stops before the end of the store.
///
/// Opening a log scans it from the head and stops at the first record that is
/// incomplete or fails its checksum. Nothing from that point on is part of the
/// recovered log. The policy decides whether those bytes may be cut.
///
/// The common cause is a torn tail: a crash partway through an append, leaving
/// a final record that was never acknowledged durable. Cutting it loses nothing
/// the caller was promised. The other cause is damage in the middle of the log
/// (bit rot, a bad sector, a missing segment file), where the records after the
/// damage are intact and were acknowledged. Cutting those loses committed data.
/// [`TruncateTail`](OpenPolicy::TruncateTail) cuts in both cases;
/// [`Strict`](OpenPolicy::Strict) cuts only in the first.
///
/// Set it with [`WalConfig::with_open_policy`]. Under either policy,
/// [`Wal::truncated_on_open`](crate::Wal::truncated_on_open) reports how many
/// bytes were cut.
///
/// # Examples
///
/// ```
/// use wal_db::{OpenPolicy, Wal, WalConfig, WalError};
///
/// # fn main() -> Result<(), WalError> {
/// # let dir = tempfile::tempdir().map_err(WalError::from)?;
/// # let path = dir.path().join("log.wal");
/// // Two records, then damage in the first one's payload.
/// let wal = Wal::open(&path)?;
/// wal.append(b"first")?;
/// wal.append(b"second")?;
/// wal.sync()?;
/// drop(wal);
/// let mut bytes = std::fs::read(&path).map_err(WalError::from)?;
/// bytes[8] ^= 0xFF;
/// std::fs::write(&path, &bytes).map_err(WalError::from)?;
///
/// // Strict open refuses, because an intact record follows the damage, and
/// // leaves the file as it was.
/// let strict = WalConfig::new().with_open_policy(OpenPolicy::Strict);
/// let refused = Wal::open_with(&path, strict);
/// assert!(matches!(refused, Err(WalError::Corruption { offset: 0, .. })));
/// assert_eq!(std::fs::read(&path).map_err(WalError::from)?, bytes);
///
/// // The default cuts the log at the damage, intact second record included.
/// let wal = Wal::open(&path)?;
/// assert_eq!(wal.truncated_on_open(), 27);
/// assert!(wal.is_empty());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpenPolicy {
    /// Cut everything from the first damaged record to the end, then open.
    ///
    /// The default, and the behaviour of every 1.0 release. A torn tail is cut
    /// and the log opens with a clean append boundary. Damage in the middle of
    /// the log is cut the same way, together with every intact record after it.
    TruncateTail,

    /// Cut a damaged final record, but refuse to open if intact data follows the
    /// damage.
    ///
    /// When the scan stops at a damaged record, every byte offset after it is
    /// checked for the start of an intact record: a full header, a length within
    /// [`WalConfig::max_record_size`], the whole payload present, and a matching
    /// checksum. Every offset is tried because the damage may be in the length
    /// field, which leaves the next record's position unknown. If none is found,
    /// the damage is a torn tail and is cut as under
    /// [`TruncateTail`](OpenPolicy::TruncateTail). If one is found, or the store
    /// cannot read back bytes it reports holding (a missing segment file, say),
    /// opening fails with [`WalError::Corruption`](crate::WalError::Corruption)
    /// at the damaged record's offset and the store is not modified.
    ///
    /// A record rejected only because its length is above
    /// [`WalConfig::max_record_size`] (a log written with a larger limit) is also
    /// refused when the rest of it is intact, last record or not, rather than
    /// cut as a torn tail. Checking it costs one read of that record.
    ///
    /// # When it refuses a log that was only torn
    ///
    /// The check cannot tell damage from a crash that left a later record
    /// complete behind an incomplete earlier one. That happens when several
    /// appends are in flight together and the process stops before they all
    /// finish: concurrent appenders, or several appends before one sync with
    /// write-back persisting them out of order. None of those records was
    /// acknowledged durable, but strict open refuses anyway. It is exact for a
    /// log written one record at a time, each made durable
    /// ([`append_and_sync`](crate::Wal::append_and_sync), or `append` then
    /// `sync`) before the next is appended. Two rarer cases also refuse: a torn
    /// record whose payload itself contains a framed record, and random bytes
    /// matching a checksum by chance (about one in 2^32 per offset whose length
    /// field is plausible). Every refusal fails closed: nothing is cut, and
    /// reopening with [`TruncateTail`](OpenPolicy::TruncateTail) after
    /// inspection cuts as before.
    ///
    /// # Cost
    ///
    /// Nothing on a log that opens clean. When there is damage, one pass over
    /// the bytes after it: a few nanoseconds per byte, plus a checksum for each
    /// offset whose length field is plausible. A candidate that fits in the
    /// 64 KiB read window is checksummed from it; a longer one is checked from
    /// prefix checksums kept every 4 KiB, so its cost does not grow with its
    /// length. Memory: the window, plus 4 bytes per 4 KiB scanned.
    Strict,
}

/// Tunable parameters for a [`Wal`](crate::Wal).
///
/// `WalConfig` is a builder. Construct it with [`WalConfig::new`] (or
/// [`Default`]), set the parameters you care about with the `with_*` methods,
/// and pass it to [`Wal::open_with`](crate::Wal::open_with) or
/// [`Wal::with_store_and_config`](crate::Wal::with_store_and_config). The
/// builder methods take and return `self`, so they chain.
///
/// New parameters are added here as later milestones land (segment size, sync
/// policy, group-commit window). The builder shape means those additions do not
/// break existing call sites.
///
/// # Examples
///
/// ```
/// use wal_db::WalConfig;
///
/// // Cap records at 1 MiB instead of the 64 MiB default.
/// let config = WalConfig::new().with_max_record_size(1024 * 1024);
/// assert_eq!(config.max_record_size(), 1024 * 1024);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalConfig {
    max_record_size: u32,
    recovery_policy: RecoveryPolicy,
    open_policy: OpenPolicy,
}

impl WalConfig {
    /// Start from the defaults.
    ///
    /// The only default that matters today is a 64 MiB maximum record size.
    ///
    /// ```
    /// use wal_db::WalConfig;
    /// let config = WalConfig::new();
    /// assert_eq!(config.max_record_size(), 64 * 1024 * 1024);
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        WalConfig {
            max_record_size: DEFAULT_MAX_RECORD_SIZE,
            recovery_policy: RecoveryPolicy::StopAtFirstError,
            open_policy: OpenPolicy::TruncateTail,
        }
    }

    /// Set the largest record the log will accept, in bytes.
    ///
    /// [`Wal::append`](crate::Wal::append) rejects any record larger than this
    /// with [`WalError::RecordTooLarge`](crate::WalError::RecordTooLarge), and
    /// recovery rejects any on-disk length prefix that claims to be larger
    /// before reading the payload. That second use is the security-relevant one:
    /// it bounds the allocation a corrupt or hostile log can request.
    ///
    /// ```
    /// use wal_db::WalConfig;
    /// let config = WalConfig::new().with_max_record_size(4096);
    /// assert_eq!(config.max_record_size(), 4096);
    /// ```
    #[must_use]
    pub const fn with_max_record_size(mut self, bytes: u32) -> Self {
        self.max_record_size = bytes;
        self
    }

    /// The configured maximum record size, in bytes.
    #[must_use]
    pub const fn max_record_size(self) -> u32 {
        self.max_record_size
    }

    /// Set how iteration reacts to a damaged record.
    ///
    /// Defaults to [`RecoveryPolicy::StopAtFirstError`].
    ///
    /// ```
    /// use wal_db::{RecoveryPolicy, WalConfig};
    /// let config = WalConfig::new().with_recovery_policy(RecoveryPolicy::SkipBadRecords);
    /// assert_eq!(config.recovery_policy(), RecoveryPolicy::SkipBadRecords);
    /// ```
    #[must_use]
    pub const fn with_recovery_policy(mut self, policy: RecoveryPolicy) -> Self {
        self.recovery_policy = policy;
        self
    }

    /// The configured recovery policy.
    #[must_use]
    pub const fn recovery_policy(self) -> RecoveryPolicy {
        self.recovery_policy
    }

    /// Set what opening the log does with damage it finds.
    ///
    /// Defaults to [`OpenPolicy::TruncateTail`]. Use [`OpenPolicy::Strict`]
    /// when a damaged record in the middle of the log must stop the open
    /// instead of cutting the intact records after it.
    ///
    /// ```
    /// use wal_db::{OpenPolicy, WalConfig};
    /// let config = WalConfig::new().with_open_policy(OpenPolicy::Strict);
    /// assert_eq!(config.open_policy(), OpenPolicy::Strict);
    /// ```
    #[must_use]
    pub const fn with_open_policy(mut self, policy: OpenPolicy) -> Self {
        self.open_policy = policy;
        self
    }

    /// The configured open policy.
    #[must_use]
    pub const fn open_policy(self) -> OpenPolicy {
        self.open_policy
    }
}

impl Default for WalConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_max_record_size_is_64_mib() {
        assert_eq!(WalConfig::new().max_record_size(), 64 * 1024 * 1024);
        assert_eq!(WalConfig::default().max_record_size(), 64 * 1024 * 1024);
    }

    #[test]
    fn test_with_max_record_size_overrides_default() {
        let config = WalConfig::new().with_max_record_size(123);
        assert_eq!(config.max_record_size(), 123);
    }

    #[test]
    fn test_default_open_policy_truncates_the_tail() {
        assert_eq!(WalConfig::new().open_policy(), OpenPolicy::TruncateTail);
        assert_eq!(WalConfig::default().open_policy(), OpenPolicy::TruncateTail);
    }

    #[test]
    fn test_with_open_policy_leaves_other_settings_alone() {
        let config = WalConfig::new().with_open_policy(OpenPolicy::Strict);
        assert_eq!(config.open_policy(), OpenPolicy::Strict);
        assert_eq!(config.recovery_policy(), RecoveryPolicy::StopAtFirstError);
        assert_eq!(config.max_record_size(), 64 * 1024 * 1024);
    }

    #[test]
    fn test_config_is_copy_and_eq() {
        let a = WalConfig::new().with_max_record_size(10);
        let b = a;
        assert_eq!(a, b);
    }
}
