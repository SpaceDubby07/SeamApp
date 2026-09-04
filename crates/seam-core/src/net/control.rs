//! Control channel: connect, handshake, and the read/write loop over plain
//! TCP (Tier 6 of the build guide).
//!
//! No TLS yet — that's M8. Pairing and cert pinning aren't implemented, so
//! this accepts any peer that speaks the right protocol version. Do not
//! point this at an untrusted network.

use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::protocol::{
    CONTROL_MAX_FRAME, ControlMessage, Handshake, OsKind, PROTOCOL_VERSION, ProtocolError,
    control_codec, decode_frame, encode_frame,
};
use crate::topology::NodeId;

/// A connected, handshaked control channel to one peer.
///
/// # Socket options
/// `TCP_NODELAY` is set on connect/accept (Tier 6.4) — Nagle's algorithm
/// would otherwise happily hold a 7-byte mouse event for ~40ms waiting for
/// more data, which is the single most common cause of a KVM tool feeling
/// laggy.
pub struct ControlChannel {
    framed: Framed<TcpStream, LengthDelimitedCodec>,
    /// The peer's node id, learned during the handshake.
    pub peer_node_id: NodeId,
    /// The peer's user-facing display name, learned during the handshake.
    pub peer_display_name: String,
}

impl ControlChannel {
    /// Connects to `addr` and performs the handshake as the initiating
    /// side.
    ///
    /// # Errors
    /// Returns an error on connection failure, if the peer rejects the
    /// handshake, or on a protocol version mismatch.
    pub async fn connect(
        addr: impl ToSocketAddrs,
        local_node_id: NodeId,
        display_name: &str,
        os: OsKind,
    ) -> Result<Self, ProtocolError> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let mut framed = Framed::new(stream, control_codec());

        send_handshake(
            &mut framed,
            &Handshake::Hello {
                protocol_version: PROTOCOL_VERSION,
                node_id: local_node_id,
                display_name: display_name.to_string(),
                os,
                app_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
        .await?;

        let (peer_node_id, peer_display_name) = match recv_handshake(&mut framed).await? {
            Handshake::HelloAck {
                protocol_version,
                node_id,
                display_name,
                accepted,
                reason,
                ..
            } => {
                if !accepted {
                    return Err(ProtocolError::Rejected(
                        reason.unwrap_or_else(|| "no reason given".to_string()),
                    ));
                }
                if protocol_version != PROTOCOL_VERSION {
                    return Err(ProtocolError::VersionMismatch {
                        peer: protocol_version,
                        ours: PROTOCOL_VERSION,
                    });
                }
                (node_id, display_name)
            }
            Handshake::Hello { .. } => return Err(ProtocolError::UnexpectedHandshakeMessage),
        };

        Ok(Self {
            framed,
            peer_node_id,
            peer_display_name,
        })
    }

    /// Accepts one incoming connection on `listener` and performs the
    /// handshake as the accepting side.
    ///
    /// # Errors
    /// Returns an error on accept failure, or if the incoming handshake is
    /// malformed or speaks an incompatible protocol version (in which case
    /// a rejecting `HelloAck` is still sent before returning the error).
    pub async fn accept(
        listener: &TcpListener,
        local_node_id: NodeId,
        display_name: &str,
        os: OsKind,
    ) -> Result<Self, ProtocolError> {
        let (stream, _addr) = listener.accept().await?;
        stream.set_nodelay(true)?;
        let mut framed = Framed::new(stream, control_codec());

        let (peer_node_id, peer_display_name) = match recv_handshake(&mut framed).await? {
            Handshake::Hello {
                protocol_version,
                node_id,
                display_name: peer_name,
                ..
            } => {
                let accepted = protocol_version == PROTOCOL_VERSION;
                send_handshake(
                    &mut framed,
                    &Handshake::HelloAck {
                        protocol_version: PROTOCOL_VERSION,
                        node_id: local_node_id,
                        display_name: display_name.to_string(),
                        os,
                        accepted,
                        reason: (!accepted).then(|| {
                            format!("protocol version mismatch: we speak v{PROTOCOL_VERSION}")
                        }),
                    },
                )
                .await?;
                if !accepted {
                    return Err(ProtocolError::VersionMismatch {
                        peer: protocol_version,
                        ours: PROTOCOL_VERSION,
                    });
                }
                (node_id, peer_name)
            }
            Handshake::HelloAck { .. } => return Err(ProtocolError::UnexpectedHandshakeMessage),
        };

        Ok(Self {
            framed,
            peer_node_id,
            peer_display_name,
        })
    }

    /// Sends one control message.
    ///
    /// # Errors
    /// Returns an error if the message can't be encoded or the socket
    /// write fails.
    pub async fn send(&mut self, msg: &ControlMessage) -> Result<(), ProtocolError> {
        let bytes = encode_frame(msg, CONTROL_MAX_FRAME)?;
        self.framed.send(bytes).await?;
        Ok(())
    }

    /// Receives the next control message, or `None` if the peer closed the
    /// connection cleanly.
    ///
    /// # Errors
    /// Returns an error if a frame can't be decoded or the socket read
    /// fails.
    pub async fn recv(&mut self) -> Result<Option<ControlMessage>, ProtocolError> {
        match self.framed.next().await {
            Some(Ok(bytes)) => Ok(Some(decode_frame(&bytes)?)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }
}

async fn send_handshake(
    framed: &mut Framed<TcpStream, LengthDelimitedCodec>,
    hs: &Handshake,
) -> Result<(), ProtocolError> {
    let bytes = encode_frame(hs, CONTROL_MAX_FRAME)?;
    framed.send(bytes).await?;
    Ok(())
}

async fn recv_handshake(
    framed: &mut Framed<TcpStream, LengthDelimitedCodec>,
) -> Result<Handshake, ProtocolError> {
    match framed.next().await {
        Some(Ok(bytes)) => decode_frame(&bytes),
        Some(Err(e)) => Err(e.into()),
        None => Err(ProtocolError::ConnectionClosed),
    }
}

/// The current time as microseconds since the Unix epoch, for `Ping`'s
/// `sent_at_micros` field.
#[must_use]
pub fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::{ControlChannel, now_micros};
    use crate::protocol::{ControlMessage, OsKind, ProtocolError};
    use crate::topology::NodeId;
    use tokio::net::TcpListener;

    async fn loopback_pair() -> (ControlChannel, ControlChannel) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let server_node = NodeId::new();
        let client_node = NodeId::new();

        let server = tokio::spawn(async move {
            ControlChannel::accept(&listener, server_node, "server", OsKind::MacOs)
                .await
                .expect("server-side handshake")
        });
        let client = ControlChannel::connect(addr, client_node, "client", OsKind::Windows)
            .await
            .expect("client-side handshake");
        let server = server.await.expect("server task");
        (client, server)
    }

    #[tokio::test]
    async fn handshake_exchanges_node_ids_and_names() {
        let (client, server) = loopback_pair().await;
        assert_eq!(client.peer_display_name, "server");
        assert_eq!(server.peer_display_name, "client");
        assert_ne!(client.peer_node_id, server.peer_node_id);
    }

    /// The M3 demo: two instances on one machine exchange handshakes and
    /// heartbeats (Tier 13).
    #[tokio::test]
    async fn exchanges_ping_pong_heartbeat() {
        let (mut client, mut server) = loopback_pair().await;

        let sent_at = now_micros();
        client
            .send(&ControlMessage::Ping {
                seq: 1,
                sent_at_micros: sent_at,
            })
            .await
            .expect("send ping");

        let received = server.recv().await.expect("recv").expect("not closed");
        let ControlMessage::Ping {
            seq,
            sent_at_micros,
        } = received
        else {
            panic!("expected Ping, got {received:?}");
        };
        assert_eq!(seq, 1);

        server
            .send(&ControlMessage::Pong {
                seq,
                sent_at_micros,
            })
            .await
            .expect("send pong");

        let pong = client.recv().await.expect("recv").expect("not closed");
        let ControlMessage::Pong {
            seq: pong_seq,
            sent_at_micros: echoed,
        } = pong
        else {
            panic!("expected Pong, got {pong:?}");
        };
        assert_eq!(pong_seq, 1);
        assert_eq!(echoed, sent_at);

        let round_trip_micros = now_micros().saturating_sub(echoed);
        // Loopback on the same machine: this should be microseconds to low
        // milliseconds, never anywhere close to a full second.
        assert!(round_trip_micros < 1_000_000, "{round_trip_micros}us");
    }

    #[tokio::test]
    async fn recv_returns_none_after_clean_close() {
        let (client, mut server) = loopback_pair().await;
        drop(client);
        let result = server.recv().await.expect("recv should not error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn mismatched_protocol_version_is_rejected_on_both_sides() {
        // Can't drive a real version mismatch through the public API (it
        // always sends PROTOCOL_VERSION), so this exercises the same
        // accept-side rejection path with a listener + a hand-rolled
        // Hello at a different version, over the same framing the real
        // client uses.
        use crate::protocol::{CONTROL_MAX_FRAME, Handshake, control_codec, encode_frame};
        use futures_util::SinkExt;
        use tokio::net::TcpStream;
        use tokio_util::codec::Framed;

        const WRONG_VERSION: u16 = crate::protocol::PROTOCOL_VERSION + 1;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        let server = tokio::spawn(async move {
            ControlChannel::accept(&listener, NodeId::new(), "server", OsKind::MacOs).await
        });

        let stream = TcpStream::connect(addr).await.expect("connect");
        let mut framed = Framed::new(stream, control_codec());
        let bad_hello = Handshake::Hello {
            protocol_version: WRONG_VERSION,
            node_id: NodeId::new(),
            display_name: "old-client".to_string(),
            os: OsKind::Windows,
            app_version: "0.0.0".to_string(),
        };
        framed
            .send(encode_frame(&bad_hello, CONTROL_MAX_FRAME).expect("encode"))
            .await
            .expect("send bad hello");

        let result = server.await.expect("server task");
        assert!(matches!(result, Err(ProtocolError::VersionMismatch { .. })));
    }
}
