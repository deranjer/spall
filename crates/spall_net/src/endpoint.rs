//! QUIC endpoints and the development authentication exchange.
//!
//! [`NetServer::accept`] and [`connect`] each perform the same three checks
//! before returning a [`Connection`]:
//!
//! 1. TLS: the client pins the server certificate fingerprint. A mismatch
//!    fails the QUIC handshake and surfaces as [`TransportError::Connect`].
//! 2. Join token: constant-time compared. A mismatch is
//!    [`AuthReject::BadToken`].
//! 3. Handshake compatibility: [`spall_protocol::check_compatible`]. A mismatch
//!    is [`AuthReject::Incompatible`] naming the field.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use tokio::sync::Mutex;

use spall_protocol::{Handshake, SessionRegistry, SlotId, check_compatible};

use crate::config::TransportConfig;
use crate::conn::{Connection, Role};
use crate::framing::{read_framed, write_framed};
use crate::message::{
    AuthReject, ClientHello, MAX_AUTH_MESSAGE, ServerAccept, ServerAuthReply, decode_auth,
    encode_auth,
};
use crate::tls::{DevIdentity, Fingerprint, JoinToken, client_config};
use crate::{Result, TransportError};

/// A listening Spall server endpoint.
pub struct NetServer {
    endpoint: quinn::Endpoint,
    token: JoinToken,
    server_handshake: Handshake,
    cfg: TransportConfig,
    sessions: Mutex<SessionRegistry>,
    next_slot: AtomicU32,
}

impl NetServer {
    /// Binds `addr` with `identity`'s certificate. `server_handshake` is the
    /// compatibility baseline every client is checked against; its `session`
    /// field is replaced per connection.
    pub async fn bind(
        addr: SocketAddr,
        identity: &DevIdentity,
        token: JoinToken,
        server_handshake: Handshake,
        cfg: TransportConfig,
    ) -> Result<Self> {
        let server_config = identity.server_config(&cfg)?;
        let endpoint = quinn::Endpoint::server(server_config, addr)?;
        Ok(Self {
            endpoint,
            token,
            server_handshake,
            cfg,
            sessions: Mutex::new(SessionRegistry::new()),
            next_slot: AtomicU32::new(0),
        })
    }

    /// The bound address (with the OS-assigned port resolved).
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Accepts one QUIC connection and runs server-side authentication. On an
    /// auth failure the peer is told why (a framed [`ServerAuthReply`]) before
    /// the error returns.
    pub async fn accept(&self) -> Result<Connection> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| TransportError::Connect("endpoint closed".into()))?;
        let quic = incoming
            .await
            .map_err(|e| TransportError::Connect(format!("quic handshake: {e}")))?;

        let deadline = self.cfg.handshake_timeout;
        tokio::time::timeout(deadline, self.authenticate(quic))
            .await
            .map_err(|_| TransportError::Timeout(deadline))?
    }

    async fn authenticate(&self, quic: quinn::Connection) -> Result<Connection> {
        let (mut send, mut recv) = quic
            .accept_bi()
            .await
            .map_err(|e| TransportError::Connect(format!("control stream: {e}")))?;

        let hello_bytes = read_framed(&mut recv, MAX_AUTH_MESSAGE)
            .await?
            .ok_or_else(|| TransportError::Auth(AuthReject::Malformed("empty hello".into())))?;
        let hello: ClientHello = match decode_auth(&hello_bytes) {
            Ok(h) => h,
            Err(reject) => return Err(reject_client(&mut send, reject).await),
        };

        if !self.token.verify(&hello.token) {
            return Err(reject_client(&mut send, AuthReject::BadToken).await);
        }

        if let Err(incompat) = check_compatible(&hello.handshake, &self.server_handshake) {
            return Err(
                reject_client(&mut send, AuthReject::Incompatible(incompat.to_string())).await,
            );
        }

        let slot = SlotId(self.next_slot.fetch_add(1, Ordering::Relaxed));
        let session = self
            .sessions
            .lock()
            .await
            .open(slot)
            .map_err(|_| TransportError::Auth(AuthReject::Busy))?;

        let mut server_handshake = self.server_handshake.clone();
        server_handshake.session = session;
        write_reply(
            &mut send,
            &ServerAuthReply::Accepted(ServerAccept {
                session,
                server_handshake,
            }),
        )
        .await?;

        Connection::from_parts(quic, send, recv, self.cfg, session, Role::Server)
    }

    /// Stops accepting and closes the endpoint.
    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"server shutdown");
    }

    /// Waits for all connections to finish closing.
    pub async fn wait_idle(&self) {
        self.endpoint.wait_idle().await;
    }
}

/// Connects to a Spall server (or a [`crate::proxy::UdpProxy`] in front of it).
///
/// `target` is where packets go; `expected` is the pinned server certificate
/// fingerprint; `token` and `client_handshake` are the credentials.
pub async fn connect(
    target: SocketAddr,
    expected: Fingerprint,
    token: JoinToken,
    client_handshake: Handshake,
    cfg: TransportConfig,
) -> Result<Connection> {
    let mut endpoint = quinn::Endpoint::client(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    endpoint.set_default_client_config(client_config(expected, &cfg)?);

    let started = Instant::now();
    let quic = endpoint
        .connect(target, "localhost")
        .map_err(|e| TransportError::Connect(e.to_string()))?
        .await
        .map_err(|e| TransportError::Connect(format!("quic handshake: {e}")))?;

    let remaining = cfg
        .handshake_timeout
        .checked_sub(started.elapsed())
        .unwrap_or_default();
    tokio::time::timeout(remaining.max(cfg.handshake_timeout / 4), async move {
        let (mut send, mut recv) = quic
            .open_bi()
            .await
            .map_err(|e| TransportError::Connect(format!("control stream: {e}")))?;

        let hello = ClientHello {
            token,
            handshake: client_handshake,
        };
        let bytes = encode_auth(&hello)?;
        write_framed(&mut send, &bytes, MAX_AUTH_MESSAGE).await?;

        let reply_bytes = read_framed(&mut recv, MAX_AUTH_MESSAGE)
            .await?
            .ok_or_else(|| {
                TransportError::Auth(AuthReject::Malformed("server closed before reply".into()))
            })?;
        let reply: ServerAuthReply = decode_auth(&reply_bytes)?;
        match reply {
            ServerAuthReply::Accepted(accept) => {
                Connection::from_parts(quic, send, recv, cfg, accept.session, Role::Client)
            }
            ServerAuthReply::Rejected(reject) => Err(TransportError::Auth(reject)),
        }
    })
    .await
    .map_err(|_| TransportError::Timeout(cfg.handshake_timeout))?
}

async fn write_reply(send: &mut quinn::SendStream, reply: &ServerAuthReply) -> Result<()> {
    let bytes = encode_auth(reply)?;
    write_framed(send, &bytes, MAX_AUTH_MESSAGE).await?;
    Ok(())
}

/// Sends a rejection, finishes the stream, and waits (bounded) for the client
/// to have received it before the connection is torn down. Returns the
/// corresponding [`TransportError`] so callers can `return Err(reject_client(..).await)`.
async fn reject_client(send: &mut quinn::SendStream, reject: AuthReject) -> TransportError {
    let _ = write_reply(send, &ServerAuthReply::Rejected(reject.clone())).await;
    let _ = send.finish();
    // `stopped()` resolves once the peer has acknowledged the data + FIN or has
    // closed; a short cap keeps a misbehaving peer from stalling accept().
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), send.stopped()).await;
    TransportError::Auth(reject)
}

impl std::fmt::Debug for NetServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetServer")
            .field("local_addr", &self.endpoint.local_addr().ok())
            .finish()
    }
}
