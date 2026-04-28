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

pub mod framing;

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

    /// XML parse failure (malformed, unexpected element, etc.).
    #[error("xml: {0}")]
    Xml(String),

    /// Framing-layer error (malformed varint, etc.).
    #[error("framing: {0}")]
    Framing(&'static str),

    /// XML attribute or text contained an entity reference (`&amp;`, `&#x...;`, etc.)
    /// that would require allocating an owned string. The borrowed-mode decoder
    /// rejects these to preserve invariant H2; CoT in practice never uses entities.
    #[error("xml: entity decoding not supported in borrowed mode")]
    EntityNotSupported,

    /// Required event attribute missing.
    #[error("xml: required event attribute `{0}` missing")]
    MissingEventAttr(&'static str),

    /// Required point attribute missing.
    #[error("xml: required point attribute `{0}` missing")]
    MissingPointAttr(&'static str),

    /// Attribute or text value contained an XML special character (`<`, `>`,
    /// `&`, `"`, `'`) that would require entity encoding to round-trip safely.
    /// CoT in production never emits these, so we hard-fail rather than
    /// silently allocating to escape.
    #[error("xml: value contains XML-special character `{0}` — entity escaping not supported")]
    SpecialCharInValue(char),

    /// Underlying protobuf decode failure.
    #[error("protobuf decode: {0}")]
    Proto(#[from] prost::DecodeError),

    /// Underlying I/O failure during encode.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<core::str::Utf8Error> for Error {
    fn from(err: core::str::Utf8Error) -> Self {
        Self::Xml(err.to_string())
    }
}

/// Convenience result type used across the codec.
pub type Result<T> = core::result::Result<T, Error>;

pub mod proto;
pub mod xml;
