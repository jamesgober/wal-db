//! The crate error type.
//!
//! Every fallible operation in `wal-db` returns [`Result<T>`], whose error is
//! [`WalError`]. The type integrates with the portfolio's `error-forge`
//! framework — it implements [`error_forge::ForgeError`], so callers get the
//! stable `kind` / `is_fatal` metadata other crates rely on — while still
//! exposing the underlying [`std::io::Error`] through [`std::error::Error::source`]
//! for code that needs to inspect the OS error kind directly.

use std::{fmt, io};

use error_forge::ForgeError;

/// A specialised [`Result`](std::result::Result) for log operations.
///
/// Defaults its error to [`WalError`], so most signatures read `Result<T>`.
pub type Result<T, E = WalError> = std::result::Result<T, E>;

/// Everything that can go wrong while appending to, syncing, or recovering a log.
///
/// The type is [`#[non_exhaustive]`](https://doc.rust-lang.org/reference/attributes/type_system.html#the-non_exhaustive-attribute):
/// future versions may add variants without a major bump, so a `match` over it
/// must include a wildcard arm.
#[non_exhaustive]
#[derive(Debug)]
pub enum WalError {
    /// An underlying I/O operation failed.
    ///
    /// `context` names the operation that was attempted (for example
    /// `"open log file"` or `"flush to stable storage"`) so the message is
    /// actionable without a backtrace. The original [`io::Error`] is preserved
    /// as the [`source`](std::error::Error::source); inspect it when the OS
    /// error kind (disk full, permission denied, interrupted) drives recovery.
    Io {
        /// What the log was trying to do when the I/O error occurred.
        context: &'static str,
        /// The underlying operating-system error.
        source: io::Error,
    },

    /// An append was rejected because the record is larger than the configured
    /// limit.
    ///
    /// Records are bounded by [`WalConfig::max_record_size`](crate::WalConfig::max_record_size)
    /// so that a single oversized — or maliciously crafted — record cannot force
    /// an unbounded allocation on the recovery path. The append makes no change
    /// to the log; the caller may split the payload or raise the limit.
    RecordTooLarge {
        /// The length of the rejected record, in bytes.
        len: usize,
        /// The configured maximum record size, in bytes.
        max: u32,
    },

    /// Recovery reached a record that is not intact.
    ///
    /// Either the record's checksum did not match its bytes, or its length
    /// prefix is implausible. In an append-only log a damaged record means
    /// everything after it is untrustworthy, so iteration surfaces this once and
    /// then stops. `offset` is the byte position of the record where the break
    /// was detected.
    Corruption {
        /// Byte offset of the record at which recovery stopped.
        offset: u64,
        /// A short, human-readable reason the record was rejected.
        reason: &'static str,
    },

    /// A typed record could not be encoded or decoded.
    ///
    /// Produced only by the typed-record API (the `pack-io` feature):
    /// [`Wal::append_typed`](crate::Wal::append_typed) when a value fails to
    /// serialise, or [`Record::decode`](crate::Record::decode) when a record's
    /// bytes do not deserialise into the requested type. `detail` carries the
    /// underlying codec error's message.
    Encoding {
        /// The underlying serialization error, as text.
        detail: String,
    },
}

impl WalError {
    /// Wrap an [`io::Error`] with the static context describing the operation.
    pub(crate) fn io(context: &'static str, source: io::Error) -> Self {
        WalError::Io { context, source }
    }

    /// Build a [`WalError::Corruption`] for the record at `offset`.
    pub(crate) fn corruption(offset: u64, reason: &'static str) -> Self {
        WalError::Corruption { offset, reason }
    }

    /// Build a [`WalError::Encoding`] from a codec error's message.
    #[cfg(feature = "pack-io")]
    pub(crate) fn encoding(detail: impl core::fmt::Display) -> Self {
        WalError::Encoding {
            detail: detail.to_string(),
        }
    }
}

impl fmt::Display for WalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalError::Io { context, source } => {
                write!(f, "i/o error while {context}: {source}")
            }
            WalError::RecordTooLarge { len, max } => {
                write!(
                    f,
                    "record of {len} bytes exceeds the maximum of {max} bytes"
                )
            }
            WalError::Corruption { offset, reason } => {
                write!(f, "log corruption at byte offset {offset}: {reason}")
            }
            WalError::Encoding { detail } => {
                write!(f, "typed record codec error: {detail}")
            }
        }
    }
}

impl std::error::Error for WalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WalError::Io { source, .. } => Some(source),
            WalError::RecordTooLarge { .. }
            | WalError::Corruption { .. }
            | WalError::Encoding { .. } => None,
        }
    }
}

/// A bare [`io::Error`] converts into [`WalError::Io`] with a generic context.
///
/// Call sites that know what they were doing attach a specific context instead;
/// this exists for the `?` ergonomics of code that does not.
impl From<io::Error> for WalError {
    fn from(source: io::Error) -> Self {
        WalError::Io {
            context: "performing a log i/o operation",
            source,
        }
    }
}

impl ForgeError for WalError {
    fn kind(&self) -> &'static str {
        match self {
            WalError::Io { .. } => "Io",
            WalError::RecordTooLarge { .. } => "RecordTooLarge",
            WalError::Corruption { .. } => "Corruption",
            WalError::Encoding { .. } => "Encoding",
        }
    }

    fn caption(&self) -> &'static str {
        "write-ahead log error"
    }

    /// Corruption is unrecoverable by retry: the bytes on disk are already
    /// damaged. I/O and size errors are left non-fatal for the caller to judge.
    fn is_fatal(&self) -> bool {
        matches!(self, WalError::Corruption { .. })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_io_error_exposes_source() {
        let inner = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let err = WalError::io("open log file", inner);
        let source = std::error::Error::source(&err).expect("io error has a source");
        let io_source = source
            .downcast_ref::<io::Error>()
            .expect("source downcasts to io::Error");
        assert_eq!(io_source.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn test_record_too_large_has_no_source() {
        let err = WalError::RecordTooLarge {
            len: 4096,
            max: 1024,
        };
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_corruption_is_fatal_others_are_not() {
        assert!(WalError::corruption(128, "checksum mismatch").is_fatal());
        assert!(!WalError::RecordTooLarge { len: 1, max: 0 }.is_fatal());
        let io = WalError::io("x", io::Error::from(io::ErrorKind::Other));
        assert!(!io.is_fatal());
    }

    #[test]
    fn test_kind_matches_variant() {
        assert_eq!(WalError::corruption(0, "x").kind(), "Corruption");
        assert_eq!(
            WalError::RecordTooLarge { len: 1, max: 0 }.kind(),
            "RecordTooLarge"
        );
    }

    #[test]
    fn test_display_is_actionable() {
        let err = WalError::RecordTooLarge {
            len: 4096,
            max: 1024,
        };
        assert_eq!(
            err.to_string(),
            "record of 4096 bytes exceeds the maximum of 1024 bytes"
        );
    }
}
