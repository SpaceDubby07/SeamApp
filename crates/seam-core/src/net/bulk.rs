//! Bulk channel: a second plain-TCP connection alongside the control
//! channel, used for anything too large for the control channel's 1 MB
//! frame cap (Tier 6.1) — clipboard images (M7) and, later, file chunks
//! (M10).
//!
//! No handshake of its own: it rides on the peer identity the control
//! channel already established. In v1 (exactly one peer) the accepting side
//! simply takes the first incoming connection on the bulk listener as
//! belonging to that peer — TLS + cert pinning, which is what actually
//! authenticates a channel, lands in M8 and will cover this one too.
//!
//! `TCP_NODELAY` is deliberately left ON (the default) here — the opposite
//! of the control channel — since bulk traffic cares about throughput, not
//! per-message latency (Tier 6.4). The other half of Tier 6.4's advice, a
//! larger send buffer to help saturate gigabit, is deferred to M10: it
//! matters for multi-hundred-MB file transfers, not the at-most-10-MB
//! clipboard images M7 sends, and setting it portably on an already-accepted
//! `TcpStream` needs a socket2 dependency this milestone doesn't otherwise
//! need.

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::protocol::{BULK_MAX_FRAME, BulkMessage, ProtocolError, bulk_codec, decode_frame};

/// A connected bulk channel to one peer. See the module docs for what
/// makes this different from [`crate::net::control::ControlChannel`].
pub struct BulkChannel {
    framed: Framed<TcpStream, LengthDelimitedCodec>,
}

impl BulkChannel {
    /// Connects to `addr` — the peer's bulk port (Tier 6.5: control port +
    /// 1 by convention).
    ///
    /// # Errors
    /// Returns an error if the connection can't be established.
    pub async fn connect(addr: impl ToSocketAddrs) -> Result<Self, ProtocolError> {
        let stream = TcpStream::connect(addr).await?;
        Ok(Self {
            framed: Framed::new(stream, bulk_codec()),
        })
    }

    /// Accepts one incoming connection on `listener`.
    ///
    /// # Errors
    /// Returns an error if the accept fails.
    pub async fn accept(listener: &TcpListener) -> Result<Self, ProtocolError> {
        let (stream, _addr) = listener.accept().await?;
        Ok(Self {
            framed: Framed::new(stream, bulk_codec()),
        })
    }

    /// Sends one bulk message.
    ///
    /// # Errors
    /// Returns an error if the message can't be encoded (e.g. it exceeds
    /// [`BULK_MAX_FRAME`]) or the socket write fails.
    pub async fn send(&mut self, msg: &BulkMessage) -> Result<(), ProtocolError> {
        let bytes = crate::protocol::encode_frame(msg, BULK_MAX_FRAME)?;
        self.framed.send(bytes).await?;
        Ok(())
    }

    /// Receives the next bulk message, or `None` if the peer closed the
    /// connection cleanly.
    ///
    /// # Errors
    /// Returns an error if a frame can't be decoded or the socket read
    /// fails.
    pub async fn recv(&mut self) -> Result<Option<BulkMessage>, ProtocolError> {
        match self.framed.next().await {
            Some(Ok(bytes)) => Ok(Some(decode_frame(&bytes)?)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BulkChannel;
    use crate::protocol::{BulkMessage, CONTROL_MAX_FRAME, ControlMessage, ProtocolError};
    use tokio::net::TcpListener;

    async fn loopback_pair() -> (BulkChannel, BulkChannel) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        let server =
            tokio::spawn(async move { BulkChannel::accept(&listener).await.expect("accept") });
        let client = BulkChannel::connect(addr).await.expect("connect");
        let server = server.await.expect("server task");
        (client, server)
    }

    #[tokio::test]
    async fn round_trips_a_clipboard_blob() {
        let (mut client, mut server) = loopback_pair().await;
        let msg = BulkMessage::ClipboardBlob {
            seq: 1,
            mime: "image/png".to_string(),
            data: vec![1, 2, 3, 4, 5],
        };
        client.send(&msg).await.expect("send");
        let received = server.recv().await.expect("recv").expect("not closed");
        assert_eq!(received, msg);
    }

    #[tokio::test]
    async fn recv_returns_none_after_clean_close() {
        let (client, mut server) = loopback_pair().await;
        drop(client);
        let result = server.recv().await.expect("recv should not error");
        assert!(result.is_none());
    }

    /// The whole reason clipboard images and file chunks travel on this
    /// channel instead of the control channel: a payload well over
    /// `CONTROL_MAX_FRAME` (1 MB) still fits comfortably under
    /// `BULK_MAX_FRAME` (16 MB).
    #[tokio::test]
    async fn carries_frames_too_large_for_the_control_channel() {
        let (mut client, mut server) = loopback_pair().await;
        let oversized_for_control = vec![0u8; CONTROL_MAX_FRAME + 1024];

        // Sanity check: the control channel's own framing really does
        // reject this size.
        assert!(matches!(
            crate::protocol::encode_frame(
                &ControlMessage::Goodbye {
                    reason: "x".repeat(CONTROL_MAX_FRAME)
                },
                CONTROL_MAX_FRAME
            ),
            Err(ProtocolError::FrameTooLarge(_))
        ));

        let msg = BulkMessage::ClipboardBlob {
            seq: 1,
            mime: "image/png".to_string(),
            data: oversized_for_control.clone(),
        };
        client
            .send(&msg)
            .await
            .expect("send oversized-for-control frame");
        let received = server.recv().await.expect("recv").expect("not closed");
        assert_eq!(received, msg);
    }
}
