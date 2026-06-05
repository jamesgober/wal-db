//! Log sequence numbers.

use std::fmt;

/// A log sequence number: the position of a record in the log, assigned at
/// append time.
///
/// LSNs are dense and monotonic. The first record appended to a fresh log is
/// [`Lsn(0)`](Lsn), the next is `Lsn(1)`, and so on, with no gaps. A record's
/// LSN is stable: it identifies that record for the life of the log and is the
/// value returned to recovery by [`Record::lsn`](crate::Record::lsn). Because
/// they are dense and ordered, LSNs from one log can be compared directly to
/// establish which record came first.
///
/// The number is a `u64`. At one appended record per nanosecond it would take
/// roughly 585 years to exhaust the space, so wraparound is not a concern this
/// type guards against.
///
/// # Examples
///
/// ```
/// use wal_db::Lsn;
///
/// let first = Lsn::new(0);
/// let second = Lsn::new(1);
///
/// assert!(first < second);
/// assert_eq!(second.get(), 1);
/// assert_eq!(u64::from(second), 1);
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
