//! TAK Protocol v1 wire framing.
//!
//! Two framing formats are supported, both prefixed with magic byte `0xBF`:
//!
//! - **Stream** ([`decode_stream`] / [`encode_stream`]) — `0xBF <varint length> <payload>`.
//!   Used over TCP / TLS / QUIC where multiple frames flow concatenated; the
//!   varint tells the decoder where one frame ends and the next begins.
//! - **Mesh** ([`decode_mesh`] / [`encode_mesh`]) — `0xBF 0x01 0xBF <payload>`.
//!   Fixed 3-byte header used over UDP where one datagram = one frame; the
//!   payload occupies the rest of the datagram.
//!
//! Legacy v0 (raw CoT XML over TCP, no framing) has no decode entry point
//! here — callers detect it by peeking the first byte (`0xBF` ⇒ v1, else
//! v0) and dispatch to the XML codec for v0 input.
//!
//! Decoders take `&'a [u8]` and return `&'a [u8]` slices into the input —
//! invariant H2. Encoders write to `io::Write`, allocating only at the
//! caller's buffer (no internal heap traffic).

use crate::{Error, Result};
use std::io;

/// Magic byte that prefixes every framed TAK Protocol v1 message.
pub const MAGIC: u8 = 0xBF;

/// Fixed 3-byte header for mesh framing (single UDP datagram).
pub const MESH_HEADER: [u8; 3] = [MAGIC, 0x01, MAGIC];

/// Default UDP multicast group for SA mesh.
pub const MULTICAST_GROUP: &str = "239.2.3.1";

/// Default UDP port for the mesh.
pub const MULTICAST_PORT: u16 = 6969;

/// Maximum bytes a varint can occupy when encoding a `u64` (per protobuf wire spec).
const MAX_VARINT_BYTES: usize = 10;

/// Wire-protocol version byte values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ProtoVersion {
    /// Raw CoT XML, no header (legacy).
    Xml = 0x00,
    /// TAK Protocol v1, protobuf payload.
    V1 = 0x01,
}

/// What kind of frame the next byte announces.
///
/// Use [`peek`] before calling [`decode_stream`] when the input might mix v0
/// (raw XML) and v1 (TAK Protocol) on the same connection — listeners on
/// port 8088 historically allow either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Buffer starts with magic `0xBF` — TAK Protocol v1 frame follows.
    V1,
    /// Buffer does not start with magic — caller should treat as v0 (raw
    /// CoT XML, no framing) and scan for `</event>` boundaries.
    LegacyXml,
}

/// Peek the next frame kind without consuming.
///
/// # Errors
///
/// [`Error::Incomplete`] if the buffer is empty.
pub fn peek(buf: &[u8]) -> Result<FrameKind> {
    match buf.first() {
        None => Err(Error::Incomplete { need: 1, have: 0 }),
        Some(&MAGIC) => Ok(FrameKind::V1),
        Some(_) => Ok(FrameKind::LegacyXml),
    }
}

// ---------------------------------------------------------------------------
// Stream (TCP/TLS/QUIC) — `0xBF <varint length> <payload>`
// ---------------------------------------------------------------------------

/// Decode one stream-framed message from `buf`.
///
/// Returns `(bytes_consumed, payload_slice)`. The caller advances its read
/// buffer by `bytes_consumed` and decodes the payload separately (typically
/// as a `prost`-decoded [`tak_proto::v1::TakMessage`]).
///
/// # Errors
///
/// - [`Error::InvalidMagic`] if `buf[0]` is not `0xBF`.
/// - [`Error::Incomplete`] if the buffer is shorter than the header or the
///   declared payload — caller should read more bytes and retry.
/// - [`Error::Framing`] if the varint length is malformed (>10 bytes or
///   would overflow `usize`).
///
/// [`tak_proto::v1::TakMessage`]: ../../tak_proto/v1/struct.TakMessage.html
pub fn decode_stream(buf: &[u8]) -> Result<(usize, &[u8])> {
    if buf.is_empty() {
        return Err(Error::Incomplete { need: 1, have: 0 });
    }
    if buf[0] != MAGIC {
        return Err(Error::InvalidMagic(buf[0]));
    }
    let (len, varint_size) = read_varint(&buf[1..])?;
    let len_usize = usize::try_from(len).map_err(|_| Error::Framing("frame length overflow"))?;
    let header = 1usize.saturating_add(varint_size);
    let total = header
        .checked_add(len_usize)
        .ok_or(Error::Framing("frame size overflow"))?;
    if buf.len() < total {
        return Err(Error::Incomplete {
            need: total,
            have: buf.len(),
        });
    }
    Ok((total, &buf[header..total]))
}

/// Encode `payload` as a stream frame, writing `0xBF <varint length> <payload>` to `out`.
///
/// # Errors
///
/// - [`Error::Io`] if the writer fails.
pub fn encode_stream<W: io::Write>(payload: &[u8], out: &mut W) -> Result<()> {
    out.write_all(&[MAGIC])?;
    write_varint(payload.len() as u64, out)?;
    out.write_all(payload)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Mesh (UDP unicast/multicast) — `0xBF 0x01 0xBF <payload>`
// ---------------------------------------------------------------------------

/// Decode one mesh-framed datagram, returning the payload slice (everything past the 3-byte header).
///
/// # Errors
///
/// - [`Error::Incomplete`] if the buffer is shorter than the 3-byte header.
/// - [`Error::InvalidMagic`] if any of the 3 header bytes don't match `MESH_HEADER`.
pub fn decode_mesh(buf: &[u8]) -> Result<&[u8]> {
    if buf.len() < MESH_HEADER.len() {
        return Err(Error::Incomplete {
            need: MESH_HEADER.len(),
            have: buf.len(),
        });
    }
    if buf[..3] != MESH_HEADER {
        return Err(Error::InvalidMagic(buf[0]));
    }
    Ok(&buf[MESH_HEADER.len()..])
}

/// Encode `payload` as a mesh datagram (`0xBF 0x01 0xBF <payload>`).
///
/// # Errors
///
/// - [`Error::Io`] if the writer fails.
pub fn encode_mesh<W: io::Write>(payload: &[u8], out: &mut W) -> Result<()> {
    out.write_all(&MESH_HEADER)?;
    out.write_all(payload)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Varint (protobuf wire format) — small u64 in 1-10 bytes.
// ---------------------------------------------------------------------------

/// Decode a protobuf-style varint, returning `(value, bytes_consumed)`.
fn read_varint(buf: &[u8]) -> Result<(u64, usize)> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &byte) in buf.iter().enumerate() {
        if i >= MAX_VARINT_BYTES {
            return Err(Error::Framing("varint exceeds 10 bytes"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, i.saturating_add(1)));
        }
        shift = shift.saturating_add(7);
    }
    Err(Error::Incomplete {
        need: buf.len().saturating_add(1),
        have: buf.len(),
    })
}

/// Encode `value` as a protobuf-style varint to `out`.
#[allow(clippy::cast_possible_truncation)] // both casts are masked to fit u8
fn write_varint<W: io::Write>(mut value: u64, out: &mut W) -> Result<()> {
    let mut buf = [0u8; MAX_VARINT_BYTES];
    let mut i = 0usize;
    while value >= 0x80 {
        buf[i] = ((value & 0x7f) as u8) | 0x80;
        value >>= 7;
        i = i.saturating_add(1);
    }
    buf[i] = (value & 0x7f) as u8;
    out.write_all(&buf[..=i])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn constants_match_spec() {
        assert_eq!(MAGIC, 0xBF);
        assert_eq!(MESH_HEADER, [0xBF, 0x01, 0xBF]);
        assert_eq!(MULTICAST_GROUP, "239.2.3.1");
        assert_eq!(MULTICAST_PORT, 6969);
        assert_eq!(ProtoVersion::Xml as u8, 0x00);
        assert_eq!(ProtoVersion::V1 as u8, 0x01);
    }

    #[test]
    fn varint_round_trip_for_known_values() {
        for v in [
            0u64,
            1,
            127,    // max 1-byte
            128,    // first 2-byte
            16_383, // max 2-byte
            16_384, // first 3-byte
            1 << 21,
            1 << 28,
            1 << 35,
            u64::MAX,
        ] {
            let mut buf = Vec::with_capacity(10);
            write_varint(v, &mut buf).unwrap();
            let (decoded, consumed) = read_varint(&buf).unwrap();
            assert_eq!(decoded, v, "value mismatch for {v}");
            assert_eq!(consumed, buf.len(), "byte count mismatch for {v}");
        }
    }

    #[test]
    fn varint_truncated_returns_incomplete() {
        // 0x80 means "more bytes follow"; with only this byte the decoder
        // should ask for more.
        let buf = [0x80u8];
        let err = read_varint(&buf).unwrap_err();
        assert!(matches!(err, Error::Incomplete { .. }));
    }

    #[test]
    fn varint_overflow_caps_at_ten_bytes() {
        let oversized = [0x80u8; 11];
        let err = read_varint(&oversized).unwrap_err();
        assert!(matches!(err, Error::Framing(msg) if msg.contains("10 bytes")));
    }

    // ---------- stream ----------

    #[test]
    fn stream_round_trip_empty_payload() {
        let mut buf = Vec::new();
        encode_stream(&[], &mut buf).unwrap();
        assert_eq!(buf, vec![MAGIC, 0x00]);
        let (consumed, payload) = decode_stream(&buf).unwrap();
        assert_eq!(consumed, 2);
        assert!(payload.is_empty());
    }

    #[test]
    fn stream_round_trip_small_payload() {
        let payload = b"\x08\x96\x01"; // some arbitrary protobuf-ish bytes
        let mut buf = Vec::new();
        encode_stream(payload, &mut buf).unwrap();
        let (consumed, decoded) = decode_stream(&buf).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(decoded, payload);
    }

    #[test]
    fn stream_round_trip_at_varint_boundary() {
        // 128-byte payload exercises the 1→2 byte varint transition.
        let payload = vec![0xAA; 128];
        let mut buf = Vec::new();
        encode_stream(&payload, &mut buf).unwrap();
        let (consumed, decoded) = decode_stream(&buf).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(decoded, payload.as_slice());
    }

    #[test]
    fn stream_decode_yields_borrowed_slice() {
        let payload = b"hello";
        let mut buf = Vec::new();
        encode_stream(payload, &mut buf).unwrap();
        let (_, decoded) = decode_stream(&buf).unwrap();
        let buf_start = buf.as_ptr() as usize;
        let buf_end = buf_start.saturating_add(buf.len());
        let decoded_start = decoded.as_ptr() as usize;
        assert!(
            decoded_start >= buf_start && decoded_start.saturating_add(decoded.len()) <= buf_end,
            "decoded slice not borrowed from input"
        );
    }

    #[test]
    fn stream_decode_invalid_magic() {
        let buf = [0xAB, 0x05, 1, 2, 3, 4, 5];
        let err = decode_stream(&buf).unwrap_err();
        assert!(matches!(err, Error::InvalidMagic(0xAB)));
    }

    #[test]
    fn stream_decode_partial_returns_incomplete() {
        // Magic + varint says 5 bytes, but we only give 2 of the 5.
        let buf = [MAGIC, 0x05, 0xDE, 0xAD];
        let err = decode_stream(&buf).unwrap_err();
        match err {
            Error::Incomplete { need, have } => {
                assert_eq!(need, 7);
                assert_eq!(have, 4);
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[test]
    fn stream_decode_extra_bytes_left_in_buffer() {
        // Two frames concatenated; first decode consumes only the first.
        let mut buf = Vec::new();
        encode_stream(b"abc", &mut buf).unwrap();
        encode_stream(b"defgh", &mut buf).unwrap();
        let (consumed1, p1) = decode_stream(&buf).unwrap();
        assert_eq!(p1, b"abc");
        let (consumed2, p2) = decode_stream(&buf[consumed1..]).unwrap();
        assert_eq!(p2, b"defgh");
        assert_eq!(consumed1.saturating_add(consumed2), buf.len());
    }

    // ---------- mesh ----------

    #[test]
    fn mesh_round_trip() {
        let payload = b"protobuf-bytes-go-here";
        let mut buf = Vec::new();
        encode_mesh(payload, &mut buf).unwrap();
        assert_eq!(&buf[..3], &MESH_HEADER);
        let decoded = decode_mesh(&buf).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn mesh_decode_short_buffer() {
        let err = decode_mesh(&[MAGIC, 0x01]).unwrap_err();
        match err {
            Error::Incomplete { need, have } => {
                assert_eq!(need, 3);
                assert_eq!(have, 2);
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[test]
    fn mesh_decode_wrong_header_byte() {
        let err = decode_mesh(&[MAGIC, 0x02, MAGIC, 0xDE]).unwrap_err();
        assert!(matches!(err, Error::InvalidMagic(_)));
    }

    #[test]
    fn mesh_payload_can_be_empty() {
        let mut buf = Vec::new();
        encode_mesh(&[], &mut buf).unwrap();
        assert_eq!(buf, MESH_HEADER);
        assert!(decode_mesh(&buf).unwrap().is_empty());
    }

    // ---------- peek ----------

    #[test]
    fn peek_distinguishes_v1_from_legacy_xml() {
        assert_eq!(peek(&[MAGIC, 0x05]).unwrap(), FrameKind::V1);
        assert_eq!(peek(b"<?xml version").unwrap(), FrameKind::LegacyXml);
        assert_eq!(peek(b"<event").unwrap(), FrameKind::LegacyXml);
        assert!(matches!(peek(&[]), Err(Error::Incomplete { .. })));
    }
}
