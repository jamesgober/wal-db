//! The strict-open check: is a damaged record followed by intact data?
//!
//! Under [`OpenPolicy::Strict`](crate::OpenPolicy::Strict) the open scan may cut
//! the log at a damaged record only when nothing after the damage is an intact
//! record. The damage may be in the length field, so where the next record would
//! start is unknown: every byte offset after the damage is tried as a record
//! start.
//!
//! Most offsets fail on the length alone: longer than the maximum record size,
//! or running past the end of the log. The rest need a checksum. A candidate
//! that fits in the read window is checksummed directly from it. A longer one is
//! checked from prefix checksums instead: the CRC32C of bytes `[a, b)` follows
//! from the CRC32C of `[base, a)`, the CRC32C of `[base, b)`, and a fixed "zero
//! shift" operator, so the cost of one candidate does not grow with its length.
//! Checksumming every long candidate directly would make a large torn record of
//! random bytes (ciphertext, say) cost time cubic in its size.
//!
//! None of this runs on a clean log, or under the default policy: it starts only
//! once the open scan has already stopped short of the end.

use crate::{
    error::Result,
    record::{self, HEADER_LEN, LEN_OFFSET},
    store::WalStore,
};

/// Bytes read into the scan window at a time. The window is refilled while at
/// least half of it still lies ahead, so a candidate of up to half a window is
/// always checksummed straight from it.
const WINDOW: usize = 64 * 1024;

/// A prefix checksum is remembered at every multiple of this many bytes, so
/// deriving the checksum up to any offset reads at most this much.
const MARK_INTERVAL: u64 = 4096;

/// The CRC32C (Castagnoli) polynomial, bit-reflected.
const POLY: u32 = 0x82F6_3B78;

/// `ZERO_BYTES[k]` is the GF(2) matrix that advances a CRC32C over `2^k` zero
/// bytes. Built at compile time.
static ZERO_BYTES: [[u32; 32]; 64] = zero_byte_operators();

/// What follows a damaged record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tail {
    /// No intact record follows the damage: it is a torn tail, safe to cut.
    Torn,
    /// An intact record starts after the damage.
    IntactAfter,
    /// The store reports bytes past the damage that it cannot read back: a
    /// hole, such as a missing segment file. Nothing beyond it can be ruled out.
    Unreadable,
    /// The "damaged" record is intact; only its length is above the configured
    /// maximum. The log was written with a larger `max_record_size`.
    Oversized,
}

/// Classify the record at `damaged`, where the open scan stopped, and the bytes
/// after it, up to `end` (the store's length).
///
/// Every allocation is bounded: the window is [`WINDOW`] bytes, the prefix marks
/// are four bytes per [`MARK_INTERVAL`] bytes scanned, and the scratch buffer is
/// at most `MARK_INTERVAL` bytes.
pub(crate) fn classify<S: WalStore>(
    store: &S,
    max_record_size: u32,
    damaged: u64,
    end: u64,
) -> Result<Tail> {
    if let Some(tail) = oversized(store, max_record_size, damaged, end)? {
        return Ok(tail);
    }
    let Some(first) = damaged.checked_add(1) else {
        return Ok(Tail::Torn);
    };
    let mut window = Window::new(first);
    let mut prefix = Prefix::new(first);
    let mut at = first;

    while end.saturating_sub(at) >= HEADER_LEN as u64 {
        if !window.covers(at, end) && !window.fill(store, at, end)? {
            return Ok(Tail::Unreadable);
        }
        let header = window.header(at);
        let parsed = record::parse_header(&header);
        let room = end - at - HEADER_LEN as u64;

        if parsed.len <= max_record_size && u64::from(parsed.len) <= room {
            let payload_start = at + HEADER_LEN as u64;
            let record_end = payload_start + u64::from(parsed.len);
            let intact = match window.slice(payload_start, record_end) {
                Some(payload) => record::verify(&header, payload, parsed.crc),
                None => match prefix.span(store, at + LEN_OFFSET as u64, record_end)? {
                    Some(crc) => crc == parsed.crc,
                    None => return Ok(Tail::Unreadable),
                },
            };
            if intact {
                return Ok(Tail::IntactAfter);
            }
        }
        at += 1;
    }
    Ok(Tail::Torn)
}

/// Check whether the record at `at` was rejected only for its length: a full
/// header whose length is above `max_record_size`, the whole payload present,
/// and a matching checksum.
///
/// Returns [`Tail::Oversized`] for such a record, [`Tail::Unreadable`] if the
/// store read short, or `None` when the record is not one (its length is within
/// the maximum, it runs past `end`, or its checksum fails). The checksum is
/// streamed in [`MARK_INTERVAL`] chunks, so a length of up to 4 GiB never
/// becomes an allocation; the cost is one read of the record.
fn oversized<S: WalStore>(
    store: &S,
    max_record_size: u32,
    at: u64,
    end: u64,
) -> Result<Option<Tail>> {
    let mut header = [0u8; HEADER_LEN];
    if end.saturating_sub(at) < HEADER_LEN as u64 {
        return Ok(None);
    }
    if store.read_at(at, &mut header)? < HEADER_LEN {
        return Ok(Some(Tail::Unreadable));
    }
    let parsed = record::parse_header(&header);
    let payload_start = at + HEADER_LEN as u64;
    if parsed.len <= max_record_size || u64::from(parsed.len) > end - payload_start {
        return Ok(None);
    }

    let mut crc = crc32c::crc32c(&header[LEN_OFFSET..]);
    let mut chunk = vec![0u8; MARK_INTERVAL as usize];
    let record_end = payload_start + u64::from(parsed.len);
    let mut from = payload_start;
    while from < record_end {
        let len = (record_end - from).min(MARK_INTERVAL) as usize;
        let buf = &mut chunk[..len];
        if store.read_at(from, buf)? < len {
            return Ok(Some(Tail::Unreadable));
        }
        crc = crc32c::crc32c_append(crc, buf);
        from += len as u64;
    }
    Ok((crc == parsed.crc).then_some(Tail::Oversized))
}

/// A run of the store's bytes held in memory for the byte-by-byte scan.
struct Window {
    start: u64,
    bytes: Vec<u8>,
}

impl Window {
    fn new(start: u64) -> Self {
        Window {
            start,
            bytes: Vec::new(),
        }
    }

    fn end(&self) -> u64 {
        self.start + self.bytes.len() as u64
    }

    /// Whether the window holds the header at `at` and either half a window
    /// after it or everything up to `end`. The caller guarantees a full header
    /// fits before `end`.
    fn covers(&self, at: u64, end: u64) -> bool {
        at >= self.start && (self.end() >= end || self.end() - at >= (WINDOW / 2) as u64)
    }

    /// Refill the window from `at`. Returns `false` if the store read short of
    /// what it claims to hold.
    fn fill<S: WalStore>(&mut self, store: &S, at: u64, end: u64) -> Result<bool> {
        let len = (end - at).min(WINDOW as u64) as usize;
        self.start = at;
        self.bytes.resize(len, 0);
        Ok(store.read_at(at, &mut self.bytes)? == len)
    }

    /// The header bytes at `at`, which [`covers`](Window::covers) guaranteed.
    fn header(&self, at: u64) -> [u8; HEADER_LEN] {
        let from = (at - self.start) as usize;
        let mut header = [0u8; HEADER_LEN];
        header.copy_from_slice(&self.bytes[from..from + HEADER_LEN]);
        header
    }

    /// The bytes `[from, to)`, if the window holds all of them.
    fn slice(&self, from: u64, to: u64) -> Option<&[u8]> {
        if from < self.start || to > self.end() {
            return None;
        }
        let base = self.start;
        Some(&self.bytes[(from - base) as usize..(to - base) as usize])
    }
}

/// CRC32C of the store's bytes from a fixed `base`, remembered every
/// [`MARK_INTERVAL`] bytes and extended lazily as the scan reaches further.
struct Prefix {
    base: u64,
    /// `marks[i]` is the CRC32C of `[base, base + i * MARK_INTERVAL)`.
    marks: Vec<u32>,
    scratch: Vec<u8>,
}

impl Prefix {
    fn new(base: u64) -> Self {
        // The CRC32C of no bytes is 0.
        Prefix {
            base,
            marks: vec![0],
            scratch: Vec::new(),
        }
    }

    /// CRC32C of `[from, to)`, or `None` if the store read short.
    ///
    /// From `crc32c(A ++ B) == zero_shift(crc32c(A), len(B)) ^ crc32c(B)` with
    /// `A = [base, from)` and `B = [from, to)`.
    fn span<S: WalStore>(&mut self, store: &S, from: u64, to: u64) -> Result<Option<u32>> {
        let Some(before) = self.up_to(store, from)? else {
            return Ok(None);
        };
        let Some(through) = self.up_to(store, to)? else {
            return Ok(None);
        };
        Ok(Some(through ^ zero_shift(before, to - from)))
    }

    /// CRC32C of `[base, at)`, or `None` if the store read short.
    fn up_to<S: WalStore>(&mut self, store: &S, at: u64) -> Result<Option<u32>> {
        let Ok(index) = usize::try_from((at - self.base) / MARK_INTERVAL) else {
            // More marks than this platform can address: refuse rather than
            // guess.
            return Ok(None);
        };
        while self.marks.len() <= index {
            let last = self.marks.len() - 1;
            let seed = self.marks[last];
            let from = self.base + last as u64 * MARK_INTERVAL;
            let Some(crc) = self.extend(store, seed, from, MARK_INTERVAL)? else {
                return Ok(None);
            };
            self.marks.push(crc);
        }
        let from = self.base + index as u64 * MARK_INTERVAL;
        self.extend(store, self.marks[index], from, at - from)
    }

    /// Continue `crc` over the `len` bytes at `from` (`len <= MARK_INTERVAL`).
    fn extend<S: WalStore>(
        &mut self,
        store: &S,
        crc: u32,
        from: u64,
        len: u64,
    ) -> Result<Option<u32>> {
        let len = len as usize;
        self.scratch.resize(len, 0);
        if store.read_at(from, &mut self.scratch)? < len {
            return Ok(None);
        }
        Ok(Some(crc32c::crc32c_append(crc, &self.scratch)))
    }
}

/// Advance a CRC32C over `len` zero bytes.
///
/// For byte strings `A` and `B`: `crc32c(A ++ B) == zero_shift(crc32c(A),
/// B.len()) ^ crc32c(B)`. The CRC32C initial value and final XOR are both
/// `0xFFFF_FFFF`, so they cancel and the identity holds on finished checksums.
/// Costs one 32x32 GF(2) matrix-vector product per set bit of `len`.
fn zero_shift(mut crc: u32, mut len: u64) -> u32 {
    let mut k = 0;
    while len != 0 {
        if len & 1 == 1 {
            crc = gf2_times(&ZERO_BYTES[k], crc);
        }
        len >>= 1;
        k += 1;
    }
    crc
}

/// Multiply the GF(2) matrix `matrix` (column `i` is the image of bit `i`) by
/// `vector`.
const fn gf2_times(matrix: &[u32; 32], mut vector: u32) -> u32 {
    let mut sum = 0;
    let mut i = 0;
    while vector != 0 {
        if vector & 1 == 1 {
            sum ^= matrix[i];
        }
        vector >>= 1;
        i += 1;
    }
    sum
}

/// Square a GF(2) matrix: the operator applied twice.
const fn gf2_square(matrix: &[u32; 32]) -> [u32; 32] {
    let mut out = [0u32; 32];
    let mut n = 0;
    while n < 32 {
        out[n] = gf2_times(matrix, matrix[n]);
        n += 1;
    }
    out
}

/// Build [`ZERO_BYTES`] by repeated squaring of the one-zero-byte operator.
const fn zero_byte_operators() -> [[u32; 32]; 64] {
    // One zero bit through the reflected register: bit 0 feeds the polynomial
    // back, every other bit moves down one place.
    let mut one_bit = [0u32; 32];
    one_bit[0] = POLY;
    let mut n = 1;
    while n < 32 {
        one_bit[n] = 1 << (n - 1);
        n += 1;
    }
    let two_bits = gf2_square(&one_bit);
    let four_bits = gf2_square(&two_bits);

    let mut ops = [[0u32; 32]; 64];
    ops[0] = gf2_square(&four_bits);
    let mut k = 1;
    while k < 64 {
        let next = gf2_square(&ops[k - 1]);
        ops[k] = next;
        k += 1;
    }
    ops
}

#[cfg(all(test, not(loom)))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{error::WalError, store::MemStore};

    /// Deterministic filler bytes (xorshift), so tests need no RNG crate.
    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            out.extend_from_slice(&seed.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        record::encode(&mut buf, payload);
        buf
    }

    /// Noise that is guaranteed to hold no intact record: every 8-byte window
    /// claims a length above `max`, so nothing reaches the checksum.
    fn implausible(len: usize) -> Vec<u8> {
        vec![0xFF; len]
    }

    #[test]
    fn test_zero_shift_matches_concatenation() {
        for (a_len, b_len) in [
            (0, 1),
            (1, 1),
            (5, 7),
            (100, 4096),
            (3, 100_000),
            (70_000, 1),
        ] {
            let a = noise(a_len, 1 + a_len as u64);
            let b = noise(b_len, 2 + b_len as u64);
            let mut ab = a.clone();
            ab.extend_from_slice(&b);
            let expected = crc32c::crc32c(&ab);
            let derived = zero_shift(crc32c::crc32c(&a), b.len() as u64) ^ crc32c::crc32c(&b);
            assert_eq!(derived, expected, "a={a_len} b={b_len}");
        }
    }

    #[test]
    fn test_zero_shift_of_zero_length_is_identity() {
        assert_eq!(zero_shift(0xDEAD_BEEF, 0), 0xDEAD_BEEF);
    }

    #[test]
    fn test_prefix_span_matches_direct_checksum() {
        let bytes = noise(20_000, 9);
        let store = MemStore::from_bytes(bytes.clone());
        let base = 17;
        let mut prefix = Prefix::new(base);
        for (from, to) in [
            (17, 17),
            (17, 18),
            (20, 4113),
            (4113, 4114),
            (5000, 19_999),
            (8192 + 17, 12_288 + 17),
            (100, 20_000),
        ] {
            let got = prefix.span(&store, from, to).unwrap().unwrap();
            let want = crc32c::crc32c(&bytes[from as usize..to as usize]);
            assert_eq!(got, want, "[{from}, {to})");
        }
    }

    #[test]
    fn test_random_tail_is_torn() {
        let store = MemStore::from_bytes(noise(200_000, 3));
        assert_eq!(classify(&store, 1 << 20, 0, 200_000).unwrap(), Tail::Torn);
    }

    #[test]
    fn test_zero_filled_tail_is_torn() {
        // A zero header claims an empty record whose checksum would be the
        // CRC32C of four zero bytes, which is not zero, so zeros never parse.
        let store = MemStore::from_bytes(vec![0; 10_000]);
        assert_eq!(classify(&store, 1 << 20, 0, 10_000).unwrap(), Tail::Torn);
    }

    #[test]
    fn test_short_tail_is_torn() {
        let store = MemStore::from_bytes(vec![0xAB; 7]);
        assert_eq!(classify(&store, 1 << 20, 0, 7).unwrap(), Tail::Torn);
    }

    #[test]
    fn test_intact_record_right_after_damage_is_found() {
        let mut bytes = implausible(1);
        bytes.extend_from_slice(&frame(b"survivor"));
        let end = bytes.len() as u64;
        let store = MemStore::from_bytes(bytes);
        assert_eq!(
            classify(&store, 1 << 20, 0, end).unwrap(),
            Tail::IntactAfter
        );
    }

    #[test]
    fn test_intact_record_beyond_the_window_is_found() {
        let mut bytes = implausible(3 * WINDOW + 5);
        bytes.extend_from_slice(&frame(b"far away"));
        let end = bytes.len() as u64;
        let store = MemStore::from_bytes(bytes);
        assert_eq!(
            classify(&store, 1 << 20, 0, end).unwrap(),
            Tail::IntactAfter
        );
    }

    #[test]
    fn test_record_longer_than_the_window_is_found_through_prefix_checksums() {
        let mut bytes = implausible(11);
        bytes.extend_from_slice(&frame(&noise(5 * WINDOW + 3, 4)));
        let end = bytes.len() as u64;
        let store = MemStore::from_bytes(bytes);
        assert_eq!(
            classify(&store, 1 << 24, 0, end).unwrap(),
            Tail::IntactAfter
        );
    }

    #[test]
    fn test_intact_record_above_max_is_not_counted() {
        let mut bytes = implausible(1);
        bytes.extend_from_slice(&frame(&[7u8; 64]));
        let end = bytes.len() as u64;
        let store = MemStore::from_bytes(bytes);
        assert_eq!(classify(&store, 63, 0, end).unwrap(), Tail::Torn);
    }

    #[test]
    fn test_record_running_past_the_end_is_not_counted() {
        let mut bytes = implausible(1);
        let framed = frame(b"cut short");
        bytes.extend_from_slice(&framed[..framed.len() - 1]);
        let end = bytes.len() as u64;
        let store = MemStore::from_bytes(bytes);
        assert_eq!(classify(&store, 1 << 20, 0, end).unwrap(), Tail::Torn);
    }

    #[test]
    fn test_intact_record_above_max_at_the_damage_is_oversized() {
        let bytes = frame(&noise(10_000, 5));
        let end = bytes.len() as u64;
        let store = MemStore::from_bytes(bytes);
        assert_eq!(classify(&store, 9_999, 0, end).unwrap(), Tail::Oversized);
    }

    #[test]
    fn test_damaged_record_above_max_is_not_oversized() {
        let mut bytes = frame(&noise(10_000, 5));
        bytes[HEADER_LEN + 17] ^= 0x01;
        let end = bytes.len() as u64;
        let store = MemStore::from_bytes(bytes);
        assert_eq!(classify(&store, 9_999, 0, end).unwrap(), Tail::Torn);
    }

    #[test]
    fn test_record_above_max_running_past_the_end_is_not_oversized() {
        let framed = frame(&noise(10_000, 5));
        let bytes = framed[..framed.len() - 1].to_vec();
        let end = bytes.len() as u64;
        let store = MemStore::from_bytes(bytes);
        assert_eq!(classify(&store, 9_999, 0, end).unwrap(), Tail::Torn);
    }

    /// A store that claims more bytes than it can read: a hole.
    struct Holey(MemStore, u64);

    impl WalStore for Holey {
        fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<()> {
            self.0.write_at(offset, bytes)
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
            self.0.read_at(offset, buf)
        }
        fn truncate(&self, len: u64) -> Result<()> {
            self.0.truncate(len)
        }
        fn sync(&self) -> Result<()> {
            Ok(())
        }
        fn len(&self) -> Result<u64> {
            Ok(self.1)
        }
    }

    #[test]
    fn test_short_read_before_the_end_is_unreadable() {
        let store = Holey(MemStore::from_bytes(implausible(100)), 1_000);
        assert_eq!(
            classify(&store, 1 << 20, 0, 1_000).unwrap(),
            Tail::Unreadable
        );
    }

    #[test]
    fn test_read_error_propagates() {
        struct Failing;
        impl WalStore for Failing {
            fn write_at(&self, _: u64, _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn read_at(&self, _: u64, _: &mut [u8]) -> Result<usize> {
                Err(WalError::io("reading", std::io::Error::other("boom")))
            }
            fn truncate(&self, _: u64) -> Result<()> {
                Ok(())
            }
            fn sync(&self) -> Result<()> {
                Ok(())
            }
            fn len(&self) -> Result<u64> {
                Ok(100)
            }
        }
        assert!(matches!(
            classify(&Failing, 1 << 20, 0, 100),
            Err(WalError::Io { .. })
        ));
    }
}
