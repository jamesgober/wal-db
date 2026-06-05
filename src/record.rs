//! On-disk record framing.
//!
//! Every record is written as a fixed 16-byte header followed by its payload:
//!
//! ```text
//! +-----------+-----------+-----------+----------------------+
//! | crc32c    | length    | lsn       | payload              |
//! | 4 bytes   | 4 bytes   | 8 bytes   | `length` bytes       |
//! +-----------+-----------+-----------+----------------------+
//! ```
//!
//! All integers are little-endian, fixed regardless of host byte order, so a
//! log written on one machine reads back identically on another.
//!
//! The checksum covers everything after it — the length, the LSN, and the
//! payload. Placing it first lets recovery read it before it knows how many
//! payload bytes follow, then confirm the whole record once those bytes are in
//! hand. A torn write (a crash partway through appending) leaves either too few
//! bytes to form a record or a payload that no longer matches the checksum;
//! either way it is detected.
//!
//! The algorithm is CRC32C (Castagnoli). It is the standard choice for storage
//! checksums: stronger error detection than the IEEE CRC32 used by zip, and
//! backed by a dedicated CPU instruction on x86-64 (SSE4.2) and aarch64.

/// Byte offset of the checksum within the header.
pub(crate) const CRC_OFFSET: usize = 0;
/// Byte offset of the payload-length field within the header.
pub(crate) const LEN_OFFSET: usize = 4;
/// Total header size in bytes.
pub(crate) const HEADER_LEN: usize = 16;

/// The parsed fields of a record header. The payload still has to be read and
/// checked against [`Header::crc`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Header {
    /// Stored checksum of `len`, `lsn`, and the payload.
    pub crc: u32,
    /// Declared payload length in bytes. Untrusted until the checksum verifies.
    pub len: u32,
    /// The record's log sequence number.
    pub lsn: u64,
}

/// Frame `payload` into `buf`, replacing whatever `buf` held.
///
/// `buf` is the caller's reusable scratch space: it is cleared and refilled, so
/// once it has grown to fit typical records no further allocation happens.
///
/// The caller must have already established that `payload.len()` fits in a
/// `u32` (the log enforces this through the maximum record size). The debug
/// assertion documents that contract; in release builds an over-long payload
/// would have been rejected before reaching here.
pub(crate) fn encode(buf: &mut Vec<u8>, lsn: u64, payload: &[u8]) {
    debug_assert!(
        payload.len() <= u32::MAX as usize,
        "payload length must fit in u32"
    );

    buf.clear();
    buf.reserve(HEADER_LEN + payload.len());
    buf.extend_from_slice(&[0u8; 4]); // checksum placeholder, overwritten below
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&lsn.to_le_bytes());
    buf.extend_from_slice(payload);

    // Checksum covers the length, the LSN, and the payload — everything past
    // the 4-byte checksum field itself.
    let crc = crc32c::crc32c(&buf[LEN_OFFSET..]);
    buf[CRC_OFFSET..LEN_OFFSET].copy_from_slice(&crc.to_le_bytes());
}

/// Parse the three header fields out of a full header's bytes.
pub(crate) fn parse_header(bytes: &[u8; HEADER_LEN]) -> Header {
    let crc = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let lsn = u64::from_le_bytes([
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]);
    Header { crc, len, lsn }
}

/// Recompute the checksum of a record from its header bytes and payload, and
/// compare it to the value the header claims.
///
/// Returns `true` when the record is intact. The computation mirrors
/// [`encode`]: the checksum runs over the length and LSN (the header past the
/// checksum field) and then the payload, which by the streaming property of
/// CRC32C equals the checksum of those bytes concatenated.
pub(crate) fn verify(header_bytes: &[u8; HEADER_LEN], payload: &[u8], expected_crc: u32) -> bool {
    let partial = crc32c::crc32c(&header_bytes[LEN_OFFSET..HEADER_LEN]);
    let crc = crc32c::crc32c_append(partial, payload);
    crc == expected_crc
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn header_array(buf: &[u8]) -> [u8; HEADER_LEN] {
        buf[..HEADER_LEN].try_into().unwrap()
    }

    #[test]
    fn test_encode_layout_and_roundtrip() {
        let mut buf = Vec::new();
        encode(&mut buf, 7, b"hello");

        assert_eq!(buf.len(), HEADER_LEN + 5);

        let header = parse_header(&header_array(&buf));
        assert_eq!(header.len, 5);
        assert_eq!(header.lsn, 7);

        let payload = &buf[HEADER_LEN..];
        assert_eq!(payload, b"hello");
        assert!(verify(&header_array(&buf), payload, header.crc));
    }

    #[test]
    fn test_encode_empty_payload() {
        let mut buf = Vec::new();
        encode(&mut buf, 0, b"");
        assert_eq!(buf.len(), HEADER_LEN);

        let header = parse_header(&header_array(&buf));
        assert_eq!(header.len, 0);
        assert!(verify(&header_array(&buf), &[], header.crc));
    }

    #[test]
    fn test_streaming_crc_equals_contiguous_crc() {
        // The verify path checksums the header tail and payload in two chunks;
        // encode checksums them as one. They must agree.
        let mut buf = Vec::new();
        encode(&mut buf, 99, b"some bytes here");
        let header = parse_header(&header_array(&buf));
        let contiguous = crc32c::crc32c(&buf[LEN_OFFSET..]);
        assert_eq!(contiguous, header.crc);
    }

    #[test]
    fn test_flipped_payload_byte_fails_verify() {
        let mut buf = Vec::new();
        encode(&mut buf, 1, b"payload");
        let header = parse_header(&header_array(&buf));

        let mut payload = buf[HEADER_LEN..].to_vec();
        payload[0] ^= 0x01;
        assert!(!verify(&header_array(&buf), &payload, header.crc));
    }

    #[test]
    fn test_reused_buffer_does_not_leak_previous_record() {
        let mut buf = Vec::new();
        encode(&mut buf, 1, b"a longer first record");
        encode(&mut buf, 2, b"short");
        assert_eq!(buf.len(), HEADER_LEN + 5);

        let header = parse_header(&header_array(&buf));
        assert_eq!(header.lsn, 2);
        assert_eq!(&buf[HEADER_LEN..], b"short");
        assert!(verify(&header_array(&buf), &buf[HEADER_LEN..], header.crc));
    }
}
