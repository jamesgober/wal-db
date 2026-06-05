//! Log configuration.

/// The default maximum record size: 64 MiB.
///
/// Generous enough for any realistic single record, small enough that a
/// corrupt length prefix cannot request a wild allocation.
const DEFAULT_MAX_RECORD_SIZE: u32 = 64 * 1024 * 1024;

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
