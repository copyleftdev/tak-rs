//! Cursor-on-Target codec — XML and TAK Protocol v1 protobuf framing.
//!
//! Wire framing per `docs/architecture.md` §3.1:
//! - Mesh framing: `0xBF 0x01 0xBF <payload>` (UDP)
//! - Stream framing: `0xBF <varint length> <payload>` (TCP/TLS)
//! - Legacy v0: raw CoT XML, no header
//!
//! Hot-path invariants H2/H3 from `docs/invariants.md`: decoders **borrow**
//! from input; fan-out is `Bytes::clone`, not `Vec::clone`.
//!
//! # Example
//! ```
//! use tak_cot::framing::{MAGIC, MESH_HEADER, MULTICAST_GROUP, MULTICAST_PORT};
//! assert_eq!(MAGIC, 0xBF);
//! assert_eq!(MESH_HEADER, [0xBF, 0x01, 0xBF]);
//! assert_eq!((MULTICAST_GROUP, MULTICAST_PORT), ("239.2.3.1", 6969));
//! ```
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented
    )
)]
#![warn(missing_docs, missing_debug_implementations)]

pub mod framing {
    //! Wire framing constants for the TAK Protocol v1 spec.

    /// Magic byte that prefixes every framed TAK Protocol v1 message.
    pub const MAGIC: u8 = 0xBF;

    /// Fixed 3-byte header for mesh framing (single UDP datagram).
    pub const MESH_HEADER: [u8; 3] = [MAGIC, 0x01, MAGIC];

    /// Default UDP multicast group for SA mesh.
    pub const MULTICAST_GROUP: &str = "239.2.3.1";

    /// Default UDP port for the mesh.
    pub const MULTICAST_PORT: u16 = 6969;

    /// Wire-protocol version byte values.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u8)]
    pub enum ProtoVersion {
        /// Raw CoT XML, no header (legacy).
        Xml = 0x00,
        /// TAK Protocol v1, protobuf payload.
        V1  = 0x01,
    }
}

/// Errors returned by codec operations. Library code uses `thiserror` enums
/// per invariant D2; downstream binaries can convert via `?` into `anyhow::Error`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Framing prefix did not start with the magic byte.
    #[error("framing: expected magic byte 0xBF, found {0:#04x}")]
    InvalidMagic(u8),

    /// Need more bytes than available.
    #[error("framing: incomplete; need {need} bytes, have {have}")]
    Incomplete {
        /// Bytes the decoder needed.
        need: usize,
        /// Bytes available in the buffer.
        have: usize,
    },

    /// Underlying protobuf decode failure.
    #[error("protobuf decode: {0}")]
    Proto(#[from] prost::DecodeError),
}

/// Convenience result type used across the codec.
pub type Result<T> = core::result::Result<T, Error>;
