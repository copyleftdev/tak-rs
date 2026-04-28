//! `AtakMockClient` — a single-connection ATAK simulator over the
//! plain-TCP firehose.
//!
//! ATAK speaks two-stage:
//!
//! 1. Connection-initiation XML preamble (Protocol v0).
//! 2. Negotiate up to TAK Protocol v1 (`0xBF <varint> <protobuf>`).
//!
//! The firehose on port 8088 in `tak-rs` skips the preamble and
//! expects Protocol v1 framing immediately. Our mock honors that —
//! it is a v1-streaming-only client, sufficient for the wire
//! conformance scenarios in this crate. The mTLS-on-8089 flow with
//! a real ATAK device is covered by `docs/conformance.md`, not
//! this mock.
//!
//! # What it can do
//!
//! - Connect over plain TCP to the firehose.
//! - Send a framed `TakMessage` (`send_frame`).
//! - Drain framed messages the server fans back at it
//!   (`recv_frame_with_timeout`).
//! - Disconnect cleanly.
//!
//! # What it cannot do (yet)
//!
//! - mTLS handshake. (Deferred — needs cert plumbing; see runbook.)
//! - Connection-init XML preamble. (Defended by the v1-streaming
//!   port, which `tak-rs` listens on by default.)
//! - Mission API REST flows. (Out of scope — covered by separate
//!   axum integration tests.)

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tak_cot::framing;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Errors emitted by [`AtakMockClient`].
#[derive(Debug, thiserror::Error)]
pub enum MockClientError {
    /// Underlying TCP I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Server's response framing was malformed.
    #[error("frame decode: {0}")]
    Decode(#[from] tak_cot::Error),
    /// `recv_frame_with_timeout` waited past its deadline without a
    /// complete frame on the wire.
    #[error("timeout waiting for inbound frame")]
    Timeout,
    /// Peer closed the socket before a full frame arrived.
    #[error("peer closed socket mid-frame")]
    PeerEof,
}

/// One mock-ATAK connection.
#[derive(Debug)]
pub struct AtakMockClient {
    sock: TcpStream,
    /// Read-side buffer; persisted across [`Self::recv_frame_with_timeout`]
    /// calls so a half-arrived frame survives until the rest lands.
    rx_buf: BytesMut,
}

impl AtakMockClient {
    /// Connect to a tak-rs firehose listener.
    ///
    /// # Errors
    ///
    /// - [`MockClientError::Io`] if the TCP connect fails.
    pub async fn connect(addr: std::net::SocketAddr) -> Result<Self, MockClientError> {
        let sock = TcpStream::connect(addr).await?;
        sock.set_nodelay(true)?;
        Ok(Self {
            sock,
            rx_buf: BytesMut::with_capacity(8192),
        })
    }

    /// Send a single Protocol-v1 framed payload (`framed_bytes`
    /// must already start with `0xBF` and contain a varint length
    /// followed by the encoded `TakMessage`).
    ///
    /// # Errors
    ///
    /// - [`MockClientError::Io`] on TCP write failure.
    pub async fn send_frame(&mut self, framed_bytes: &[u8]) -> Result<(), MockClientError> {
        self.sock.write_all(framed_bytes).await?;
        self.sock.flush().await?;
        Ok(())
    }

    /// Read until one full framed Protocol-v1 frame is buffered;
    /// returns it as `Bytes` (zero-copy split off the rx buffer).
    ///
    /// # Errors
    ///
    /// - [`MockClientError::Timeout`] if no frame completes inside
    ///   `deadline`.
    /// - [`MockClientError::PeerEof`] if the server closes the
    ///   socket mid-frame.
    pub async fn recv_frame_with_timeout(
        &mut self,
        deadline: Duration,
    ) -> Result<Bytes, MockClientError> {
        let start = tokio::time::Instant::now();

        loop {
            // Try to extract a frame from what's already buffered.
            if let Ok((total, _)) = framing::decode_stream(&self.rx_buf[..]) {
                let frame = self.rx_buf.split_to(total).freeze();
                return Ok(frame);
            }

            // No complete frame yet — read more.
            let remaining = deadline
                .checked_sub(start.elapsed())
                .ok_or(MockClientError::Timeout)?;
            let read_fut = self.sock.read_buf(&mut self.rx_buf);
            let n = match tokio::time::timeout(remaining, read_fut).await {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(MockClientError::Io(e)),
                Err(_) => return Err(MockClientError::Timeout),
            };
            if n == 0 {
                return Err(MockClientError::PeerEof);
            }
        }
    }
}
