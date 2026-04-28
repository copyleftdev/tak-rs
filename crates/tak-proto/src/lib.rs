//! Vendored TAK Protocol v1 protobuf types.
//!
//! Generated from upstream `.proto` files at
//! `.scratch/takserver-java/src/takserver-protobuf/src/main/proto/`.
//! See `UPSTREAM.md` for the source SHA.
//!
//! # Layout
//!
//! prost generates one file per protobuf package; we mirror the package
//! hierarchy so cross-package references resolve correctly:
//!
//! - [`atakmap::commoncommo::protobuf::v1`] — also re-exported as [`v1`]
//! - [`gov::tak::cop::proto::v1`] — also re-exported as [`payload`]
//! - [`com::atakmap`] — also re-exported as [`fig`]
//!
//! # Example
//! ```
//! # #![allow(clippy::unwrap_used)]
//! use tak_proto::v1::TakMessage;
//! use prost::Message as _;
//!
//! let msg = TakMessage::default();
//! let bytes = msg.encode_to_vec();
//! let decoded = TakMessage::decode(&bytes[..]).unwrap();
//! assert_eq!(decoded, msg);
//! ```
#![allow(clippy::all, clippy::pedantic, missing_docs, unreachable_pub)]

pub mod atakmap {
    pub mod commoncommo {
        pub mod protobuf {
            pub mod v1 {
                include!(concat!(
                    env!("OUT_DIR"),
                    "/atakmap.commoncommo.protobuf.v1.rs"
                ));
            }
        }
    }
}

pub mod gov {
    pub mod tak {
        pub mod cop {
            pub mod proto {
                pub mod v1 {
                    include!(concat!(env!("OUT_DIR"), "/gov.tak.cop.proto.v1.rs"));
                }
            }
        }
    }
}

pub mod com {
    pub mod atakmap {
        include!(concat!(env!("OUT_DIR"), "/com.atakmap.rs"));
    }
}

// Convenience re-exports — the canonical names used elsewhere in tak-rs.
pub use atakmap::commoncommo::protobuf::v1;
pub use com::atakmap as fig;
pub use gov::tak::cop::proto::v1 as payload;

// Re-export prost::Message for downstream encode/decode without an extra import.
pub use prost::Message;
