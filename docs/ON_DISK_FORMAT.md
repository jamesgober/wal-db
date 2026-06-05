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
> **segment layout** below is frozen as of `0.3.1`.

## Status and stability

| Element | Stability |
|---------|-----------|
| Record framing | Frozen for 1.x as of 0.3.0 |
| Segment-file naming / directory layout | Frozen for 1.x as of 0.3.1 |

A change to the frozen record format would be a breaking change requiring a major
version and a documented migration. Additive, backward-compatible changes (a new
optional trailing section that older readers can ignore) may appear in a minor
version.

## Conventions

- All multi-byte integers are **little-endian**, independent of host byte order.
- Offsets and lengths are in **bytes**.
- The notation `u32`/`u64` denotes unsigned little-endian integers of that width.

## Log structure

A log is a bare sequence of records, back to back, in one continuous byte
address space starting at offset 0. There is no file header. A record's **log
sequence number (LSN) is the byte offset at which it begins**; the first record
has LSN 0.

```text
offset 0      LSN_1          LSN_2                    end
  |  record 1  |   record 2   |   record 3   | ... |
```

That address space may live in a single file or be striped across segment files
(see [Segments](#segments) below); either way the bytes and the offsets are
identical. The record format does not depend on which storage is used.

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

## Segments

A log may be stored as a single file, or striped across **segment files** in a
directory so each file stays bounded (which keeps recovery time bounded and lets
old, fully superseded files be archived or pruned). The two are equivalent at the
byte level: the continuous address space above is simply split into fixed-size
pieces.

### Directory layout

Each segment is a file named with its **zero-padded 20-digit decimal index** and
the extension `.wal`:

```text
00000000000000000000.wal   segment 0  -> byte range [0,           S)
00000000000000000001.wal   segment 1  -> byte range [S,         2*S)
00000000000000000002.wal   segment 2  -> byte range [2*S,       3*S)
...
```

where `S` is the configured segment size in bytes. Segment `k` holds the
log's bytes `[k*S, (k+1)*S)`. The index is fixed-width so the files sort
lexically into log order, and any file in the directory whose name is not exactly
20 digits followed by `.wal` is ignored.

### Mapping and spanning

A logical byte at offset `O` lives in segment `O / S` at local offset `O % S`. A
record is **not** aligned to segments and may **span** them: a write or read that
crosses a boundary is split across the two files. Every segment except the last
is exactly `S` bytes; the last is partially filled. There are no gaps and no
padding — the address space is contiguous across files, exactly as in a single
file.

This is the same scheme PostgreSQL uses for its WAL (a continuous record stream
divided into fixed-size segment files).

### Recovery and truncation across segments

Recovery is the same forward scan as above, reading through segment boundaries
transparently. It ends when a read returns short — either a torn record (as
above) or a missing segment file, which marks the end of the log. Suffix
truncation to length `L` shrinks the segment containing `L` to `L % S` bytes and
deletes every segment file with index greater than `L / S`.

### Prefix truncation and the `head` marker

A segmented log can also drop its **prefix** — reclaim old, already-applied
records — by deleting whole leading segment files. Because records span segments,
the lowest surviving segment may begin in the middle of a record, so recovery
cannot infer where the first complete record starts. The directory therefore
holds an optional file named `head`: 8 little-endian bytes giving the byte offset
of the first surviving record (a real record boundary). It is written and flushed
*before* any segment is deleted, so a crash mid-truncation still recovers from the
correct boundary. The file is absent until the first prefix truncation; when
absent, the head is 0. A reader that does not understand the `head` file ignores
it (it is not a `.wal` segment), which is safe only for logs that never had a
prefix dropped.

<hr>
<br>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>JAMES GOBER.</strong></sup>
</div>
