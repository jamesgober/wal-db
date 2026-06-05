//! Log sequence numbers.

use std::fmt;

/// A log sequence number: a record's byte position in the log, assigned at
/// append time.
///
/// An LSN is the byte offset where the record's frame begins. The first record
/// appended to a fresh log is [`Lsn(0)`](Lsn); the next starts at the first
/// record's end, and so on. LSNs are therefore **monotonic and unique but not
/// consecutive** — each one is larger than the last by exactly the previous
/// record's framed size. Defining the LSN as the offset is what lets the
/// multi-writer append path stay lock-free: a single atomic reservation hands
/// out the offset, and offset order is append order by construction, so two
/// records can never share an LSN or land out of order.
///
/// A record's LSN is stable for the life of the log and is the value returned by
/// [`Record::lsn`](crate::Record::lsn). Comparing two LSNs establishes which
/// record came first.
///
/// The number is a `u64`. Even an exabyte log uses only 60 bits of it, so
/// wraparound is not a concern this type guards against.
///
/// # Examples
///
/// ```
/// use wal_db::Lsn;
///
/// // The first record always starts at offset 0.
/// let first = Lsn::new(0);
/// // A later record sits at a higher offset; the exact value depends on the
/// // sizes of the records before it.
/// let later = Lsn::new(64);
///
/// assert!(first < later);
/// assert_eq!(later.get(), 64);
/// assert_eq!(u64::from(later), 64);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[must_use]
pub struct Lsn(u64);

impl Lsn {
    /// Construct an LSN from its raw `u64` value.
    ///
    /// This is rarely needed directly — [`Wal::append`](crate::Wal::append)
    /// returns the LSN it assigned — but it is useful for comparisons and tests.
    ///
    /// ```
    /// use wal_db::Lsn;
    /// assert_eq!(Lsn::new(42).get(), 42);
    /// ```
    #[inline]
    pub const fn new(value: u64) -> Self {
        Lsn(value)
    }

    /// Return the raw `u64` value.
    ///
    /// ```
    /// use wal_db::Lsn;
    /// let lsn = Lsn::new(7);
    /// assert_eq!(lsn.get(), 7);
    /// ```
    #[inline]
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<Lsn> for u64 {
    #[inline]
    fn from(lsn: Lsn) -> Self {
        lsn.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lsn_get_roundtrips_value() {
        assert_eq!(Lsn::new(0).get(), 0);
        assert_eq!(Lsn::new(u64::MAX).get(), u64::MAX);
    }

    #[test]
    fn test_lsn_ordering_is_numeric() {
        assert!(Lsn::new(0) < Lsn::new(1));
        assert!(Lsn::new(100) > Lsn::new(99));
        assert_eq!(Lsn::new(5), Lsn::new(5));
    }

    #[test]
    fn test_lsn_converts_to_u64() {
        assert_eq!(u64::from(Lsn::new(123)), 123);
    }

    #[test]
    fn test_lsn_display_is_the_number() {
        assert_eq!(Lsn::new(42).to_string(), "42");
    }
}
