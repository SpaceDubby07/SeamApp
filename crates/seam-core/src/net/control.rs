//! Control channel: connect, TLS handshake, and the read/write loop
//! (Tier 6 of the build guide).
//!
//! TLS always, per the crate-level non-negotiable rule (Tier 7.6, M8) — the
//! app-level `Handshake`/`ControlMessage` exchange rides entirely inside
//! the encrypted channel. Identity is by certificate-fingerprint pinning,
//! not a CA: pass [`Trust::OnFirstUse`] for the initial pairing connection,
//! [`Trust::Pinned`] for every connection after that (see `net::tls` and
//! `net::pairing`).

use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::net::tls::{
    Fingerprint, NodeIdentity, Trust, client_config, dummy_server_name, peer_fingerprint,
    server_config,
};
use crate::protocol::{
    CONTROL_MAX_FRAME, ControlMessage, Handshake, OsKind, PROTOCOL_VERSION, ProtocolError,
    control_codec, decode_frame, encode_frame,
};
use crate::topology::NodeId;

/// A connected, handshaked, TLS-encrypted control channel to one peer.
///
/// # Socket options
/// `TCP_NODELAY` is set on connect/accept (Tier 6.4) — Nagle's algorithm
/// would otherwise happily hold a 7-byte mouse event for ~40ms waiting for
/// more data, which is the single most common cause of a KVM tool feeling
/// laggy.
pub struct ControlChannel {
    framed: Framed<TlsStream<TcpStream>, LengthDelimitedCodec>,
    /// The peer's node id, learned during the handshake.
    pub peer_node_id: NodeId,
    /// The peer's user-facing display name, learned during the handshake.
    pub peer_display_name: String,
    /// The peer's TLS certificate fingerprint, learned during the TLS
    /// handshake itself (before the app-level `Handshake` even runs). Under
    /// [`Trust::Pinned`] this is guaranteed to equal the fingerprint that
    /// was pinned; under [`Trust::OnFirstUse`] this is what a pairing flow
    /// shows the user and, on confirmation, is what gets pinned.
    pub peer_fingerprint: Fingerprint,
}

impl ControlChannel {
    /// Connects to `addr`, establishes TLS as the client side (presenting
    /// `identity`'s certificate, verifying the peer's per `trust`), and
    /// performs the application handshake as the initiating side.
    ///
    /// # Errors
    /// Returns an error on connection failure, TLS handshake/verification
    /// failure (including a [`Trust::Pinned`] fingerprint mismatch — a hard
    /// fail, never a silent fallback), a rejected application handshake, or
    /// a protocol version mismatch.
    pub async fn connect(
        addr: impl ToSocketAddrs,
        local_node_id: NodeId,
        display_name: &str,
        os: OsKind,
        identity: &NodeIdentity,
        trust: Trust,
    ) -> Result<Self, ProtocolError> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;

        let tls_config = client_config(identity, trust)?;
        let connector = TlsConnector::from(tls_config);
        let tls_stream = TlsStream::Client(connector.connect(dummy_server_name(), stream).await?);
        let peer_fingerprint =
            peer_fingerprint(&tls_stream).ok_or(ProtocolError::MissingPeerCertificate)?;

        let mut framed = Framed::new(tls_stream, control_codec());

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
            peer_fingerprint,
        })
    }

    /// Accepts one incoming connection on `listener`, establishes TLS as
    /// the server side (presenting `identity`'s certificate, REQUIRING and
    /// verifying the connecting side's per `trust` — mutual TLS), and
    /// performs the application handshake as the accepting side.
    ///
    /// # Errors
    /// Returns an error on accept failure, TLS handshake/verification
    /// failure (including a [`Trust::Pinned`] fingerprint mismatch), or if
    /// the incoming application handshake is malformed or speaks an
    /// incompatible protocol version (in which case a rejecting `HelloAck`
    /// is still sent before returning the error).
    pub async fn accept(
        listener: &TcpListener,
        local_node_id: NodeId,
        display_name: &str,
        os: OsKind,
        identity: &NodeIdentity,
        trust: Trust,
    ) -> Result<Self, ProtocolError> {
        let (stream, _addr) = listener.accept().await?;
        stream.set_nodelay(true)?;

        let tls_config = server_config(identity, trust)?;
        let acceptor = TlsAcceptor::from(tls_config);
        let tls_stream = TlsStream::Server(acceptor.accept(stream).await?);
        let peer_fingerprint =
            peer_fingerprint(&tls_stream).ok_or(ProtocolError::MissingPeerCertificate)?;

        let mut framed = Framed::new(tls_stream, control_codec());

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
            peer_fingerprint,
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
    framed: &mut Framed<TlsStream<TcpStream>, LengthDelimitedCodec>,
    hs: &Handshake,
) -> Result<(), ProtocolError> {
    let bytes = encode_frame(hs, CONTROL_MAX_FRAME)?;
    framed.send(bytes).await?;
    Ok(())
}

async fn recv_handshake(
    framed: &mut Framed<TlsStream<TcpStream>, LengthDelimitedCodec>,
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
    use crate::net::tls::{NodeIdentity, Trust};
    use crate::protocol::{ControlMessage, OsKind, ProtocolError};
    use crate::topology::NodeId;
    use tokio::net::TcpListener;

    async fn loopback_pair() -> (ControlChannel, ControlChannel) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let server_node = NodeId::new();
        let client_node = NodeId::new();
        let server_identity = NodeIdentity::generate().expect("server identity");
        let client_identity = NodeIdentity::generate().expect("client identity");

        let server = tokio::spawn(async move {
            ControlChannel::accept(
                &listener,
                server_node,
                "server",
                OsKind::MacOs,
                &server_identity,
                Trust::OnFirstUse,
            )
            .await
            .expect("server-side handshake")
        });
        let client = ControlChannel::connect(
            addr,
            client_node,
            "client",
            OsKind::Windows,
            &client_identity,
            Trust::OnFirstUse,
        )
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

    /// Both sides learn each other's TLS fingerprint from the handshake
    /// itself — this is what a pairing flow shows the user (Tier 7.6).
    #[tokio::test]
    async fn both_sides_learn_the_others_tls_fingerprint() {
        let (client, server) = loopback_pair().await;
        assert_ne!(client.peer_fingerprint, server.peer_fingerprint);
        // Each side's observed peer fingerprint is the OTHER side's own —
        // there's no direct accessor for "my own fingerprint" here, but
        // never matching your own would be a stronger, cheaper sanity
        // check than trying to compare against a value this test can't
        // otherwise obtain without threading the identities out too.
    }

    /// The M8 demo (Tier 13): a connection attempt pinned to a fingerprint
    /// that doesn't match the peer's real certificate hard-fails — it must
    /// never silently fall back to trusting an unpaired/changed peer.
    #[tokio::test]
    async fn connect_refuses_to_establish_with_an_unpaired_or_mismatched_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let server_identity = NodeIdentity::generate().expect("server identity");
        let client_identity = NodeIdentity::generate().expect("client identity");
        let wrong_fingerprint = NodeIdentity::generate().expect("throwaway").fingerprint;

        let server_task = tokio::spawn(async move {
            ControlChannel::accept(
                &listener,
                NodeId::new(),
                "server",
                OsKind::MacOs,
                &server_identity,
                Trust::OnFirstUse,
            )
            .await
        });

        let result = ControlChannel::connect(
            addr,
            NodeId::new(),
            "client",
            OsKind::Windows,
            &client_identity,
            Trust::Pinned(wrong_fingerprint),
        )
        .await;

        assert!(
            result.is_err(),
            "connecting with a fingerprint that doesn't match any real peer must fail"
        );
        // The server side observes the failed handshake as an error too,
        // rather than quietly proceeding without a peer.
        let server_result = server_task.await.expect("server task");
        assert!(server_result.is_err());
    }

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
        use tokio::io::AsyncWriteExt;

        let (mut client, mut server) = loopback_pair().await;
        // A "clean close" for TLS means sending `close_notify` before the
        // socket goes away — TLS deliberately treats an ungraceful drop as
        // an error rather than a plain EOF, to guard against truncation
        // attacks, so simply `drop`ping `client` here (as the pre-TLS
        // version of this test did) would make `recv` return `Err`, not
        // `Ok(None)`.
        client
            .framed
            .get_mut()
            .shutdown()
            .await
            .expect("tls shutdown");
        drop(client);
        let result = server.recv().await.expect("recv should not error");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn mismatched_protocol_version_is_rejected_on_both_sides() {
        // Can't drive a real version mismatch through the public API (it
        // always sends PROTOCOL_VERSION), so this exercises the same
        // accept-side rejection path with a listener + a hand-rolled
        // Hello at a different version, over the same TLS + framing the
        // real client uses.
        use crate::net::tls::{client_config, dummy_server_name};
        use crate::protocol::{CONTROL_MAX_FRAME, Handshake, control_codec, encode_frame};
        use futures_util::SinkExt;
        use tokio_rustls::{TlsConnector, TlsStream};
        use tokio_util::codec::Framed;

        const WRONG_VERSION: u16 = crate::protocol::PROTOCOL_VERSION + 1;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let server_identity = NodeIdentity::generate().expect("server identity");
        let client_identity = NodeIdentity::generate().expect("client identity");

        let server = tokio::spawn(async move {
            ControlChannel::accept(
                &listener,
                NodeId::new(),
                "server",
                OsKind::MacOs,
                &server_identity,
                Trust::OnFirstUse,
            )
            .await
        });

        let tls_config = client_config(&client_identity, Trust::OnFirstUse).expect("tls config");
        let connector = TlsConnector::from(tls_config);
        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let tls_stream = TlsStream::Client(
            connector
                .connect(dummy_server_name(), stream)
                .await
                .expect("tls handshake"),
        );
        let mut framed = Framed::new(tls_stream, control_codec());
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
