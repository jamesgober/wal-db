<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>wal-db</b><br>
    <sub><sup>ON-DISK FORMAT</sup></sub>
</h1>

<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <a href="./API.md" title="API Reference"><b>API</b></a>
        <span>&nbsp;│&nbsp;</span>
        <span>ON-DISK FORMAT</span>
    </sup>
</div>

<br>

> Normative specification of the bytes `wal-db` writes. The **record format** in
> this document is **frozen for the 1.x line** as of `0.3.0`: a record written by
> any `>= 0.3.0`, `< 2.0.0` release reads back identically on any other. The
> multi-file segment layout is added in `0.3.1`; this document covers a single
> log file.

## Status and stability

| Element | Stability |
|---------|-----------|
| Record framing (this document) | Frozen for 1.x as of 0.3.0 |
| Segment-file naming / directory layout | Defined in 0.3.1 |

A change to the frozen record format would be a breaking change requiring a major
version and a documented migration. Additive, backward-compatible changes (a new
optional trailing section that older readers can ignore) may appear in a minor
version.

## Conventions

- All multi-byte integers are **little-endian**, independent of host byte order.
- Offsets and lengths are in **bytes**.
- The notation `u32`/`u64` denotes unsigned little-endian integers of that width.

## Log structure

A log file is a bare sequence of records, back to back, starting at offset 0.
There is no file header. A record's **log sequence number (LSN) is the byte
offset at which it begins**; the first record has LSN 0.

```text
offset 0      LSN_1          LSN_2                    end
  |  record 1  |   record 2   |   record 3   | ... |
```

## Record layout

Each record is an 8-byte header followed by its payload:

```text
        +--------------------+--------------------+----------------------+
 field  | crc32c             | length             | payload              |
 type   | u32 (LE)           | u32 (LE)           | length bytes         |
 offset | +0                 | +4                 | +8                   |
        +--------------------+--------------------+----------------------+
```

| Field | Type | Offset | Meaning |
|-------|------|--------|---------|
| `crc32c` | `u32` | 0 | Checksum of `length` and `payload` (every byte from offset +4 to the end of the payload). |
| `length` | `u32` | 4 | Payload length in bytes. Must not exceed the reader's configured maximum record size. |
| `payload` | bytes | 8 | The caller's record bytes, exactly `length` of them. |

A record's total on-disk size is `8 + length` bytes. The next record begins
immediately after, so a record at offset `L` with payload length `n` is followed
by a record at offset `L + 8 + n`.

There is no sequence-number field: a record's LSN is its offset, which the reader
already knows while scanning, so storing it again would only waste space.

## Checksum

The checksum is **CRC32C** (Castagnoli), the standard storage checksum, computed
with these parameters:

| Parameter | Value |
|-----------|-------|
| Width | 32 bits |
| Polynomial | `0x1EDC6F41` |
| Initial value | `0xFFFFFFFF` |
| Reflect input | yes |
| Reflect output | yes |
| Final XOR | `0xFFFFFFFF` |
| Check value (`"123456789"`) | `0xE3069283` |

This matches the CRC-32C used by iSCSI, SCTP, ext4, and the Rust `crc32c` crate.
On x86-64 and aarch64 it compiles to the hardware CRC instruction.

The checksum covers the `length` field and the `payload`, in that order — every
byte of the record except the 4-byte checksum field itself. Equivalently, it is
`crc32c(length_le_bytes ++ payload)`.

## Writing

To append a record with payload `P`:

1. Reserve a byte range of size `8 + P.len()` at the current end of the log; the
   range's start offset is the record's LSN.
2. Lay out the header: write `P.len()` as the `length` field, compute the CRC32C
   over `length` followed by `P`, and write it as the `crc32c` field.
3. Write the 8-byte header followed by `P` into the reserved range.

Reservations are disjoint, so concurrent writers never overlap. A writer's bytes
reach stable storage only when a subsequent `sync` (the platform durability
barrier) returns.

## Reading and recovery

To read the log, scan forward from offset 0. At each offset `L < end`:

1. Read 8 bytes. If fewer than 8 are available, stop — the log ends in a partial
   header (a torn tail).
2. Parse `crc32c` and `length`. If `length` exceeds the configured maximum record
   size, stop — the length is implausible; treat the remainder as a torn tail.
   **The length is validated before any payload bytes are read, so a corrupt
   length can never drive an unbounded allocation.**
3. Read `length` payload bytes. If fewer are available, stop — a torn tail.
4. Recompute the CRC32C over `length` and the payload and compare it to the stored
   `crc32c`. If they differ, stop — the record is damaged.
5. Otherwise the record is intact: its LSN is `L`, and the next record is at
   `L + 8 + length`.

Recovery returns every record up to the first stop and no partial record. A log
opened for appending truncates anything beyond the last intact record, so the
next append lands on a clean boundary.

### Torn writes

A crash partway through an append leaves either too few bytes to form a record
(caught at step 1 or 3) or a payload that no longer matches the checksum (caught
at step 4). In every case recovery stops cleanly at that record and never reports
it as complete.

<hr>
<br>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>JAMES GOBER.</strong></sup>
</div>
