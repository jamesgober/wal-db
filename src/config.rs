//! Log configuration.

/// The default maximum record size: 64 MiB.
///
/// Generous enough for any realistic single record, small enough that a
/// corrupt length prefix cannot request a wild allocation.
const DEFAULT_MAX_RECORD_SIZE: u32 = 64 * 1024 * 1024;

/// How [`Wal::iter`](crate::Wal::iter) reacts to a damaged record.
///
/// This governs *iteration*, not the torn-tail truncation that
/// [`Wal::open`](crate::Wal::open) always performs to keep the append boundary
/// clean. It matters when a record in the middle of an already-recovered log is
/// damaged — bit rot, say — and you are reading the log back.
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
    fn test_config_is_copy_and_eq() {
        let a = WalConfig::new().with_max_record_size(10);
        let b = a;
        assert_eq!(a, b);
    }
}
