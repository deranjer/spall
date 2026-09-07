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

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::Semaphore;

use spall_protocol::{Handshake, NegotiatedLimits, Record, SessionId, SlotId, check_compatible};

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
    sessions: Arc<Mutex<Vec<(u32, bool)>>>,
    preauth: Arc<Semaphore>,
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
        let server_handshake = handshake_with_local_limits(server_handshake, cfg)?;
        let server_config = identity.server_config(&cfg)?;
        let endpoint = quinn::Endpoint::server(server_config, addr)?;
        Ok(Self {
            endpoint,
            token,
            server_handshake,
            cfg,
            sessions: Arc::new(Mutex::new(vec![(0, false); cfg.max_connections as usize])),
            preauth: Arc::new(Semaphore::new(cfg.max_pending_authentications as usize)),
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
        let _preauth =
            self.preauth.clone().try_acquire_owned().map_err(|_| {
                TransportError::Connect("preauthentication budget exhausted".into())
            })?;
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| TransportError::Connect("endpoint closed".into()))?;
        let started = Instant::now();
        let quic = tokio::time::timeout(
            remaining_handshake_time(started, self.cfg.handshake_timeout)?,
            incoming,
        )
        .await
        .map_err(|_| TransportError::Timeout(self.cfg.handshake_timeout))?
        .map_err(|e| TransportError::Connect(format!("quic handshake: {e}")))?;

        let result = tokio::time::timeout(
            remaining_handshake_time(started, self.cfg.handshake_timeout)?,
            self.authenticate(quic.clone()),
        )
        .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                quic.close(1u32.into(), b"authentication timeout");
                Err(TransportError::Timeout(self.cfg.handshake_timeout))
            }
        }
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

        let lease = SessionLease::acquire(self.sessions.clone())?;
        let session = lease.session;

        let mut server_handshake = self.server_handshake.clone();
        server_handshake.session = session;
        server_handshake.limits = hello.handshake.limits;
        write_reply(
            &mut send,
            &ServerAuthReply::Accepted(ServerAccept {
                session,
                server_handshake,
            }),
        )
        .await?;

        let mut conn = Connection::from_parts(
            quic,
            send,
            recv,
            effective_config(self.cfg, hello.handshake.limits)?,
            session,
            Role::Server,
        )?;
        conn.session_lease = Some(lease);
        Ok(conn)
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
    let client_handshake = handshake_with_local_limits(client_handshake, cfg)?;
    let mut endpoint = quinn::Endpoint::client(client_bind_addr(target))?;
    endpoint.set_default_client_config(client_config(expected, &cfg)?);

    let started = Instant::now();
    let connecting = endpoint
        .connect(target, "localhost")
        .map_err(|e| TransportError::Connect(e.to_string()))?;
    let quic = tokio::time::timeout(cfg.handshake_timeout, connecting)
        .await
        .map_err(|_| TransportError::Timeout(cfg.handshake_timeout))?
        .map_err(|e| TransportError::Connect(format!("quic handshake: {e}")))?;

    let remaining = cfg
        .handshake_timeout
        .checked_sub(started.elapsed())
        .unwrap_or_default();
    tokio::time::timeout(remaining, async move {
        let (mut send, mut recv) = quic
            .open_bi()
            .await
            .map_err(|e| TransportError::Connect(format!("control stream: {e}")))?;

        let hello = ClientHello {
            token,
            handshake: client_handshake.clone(),
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
                accept
                    .server_handshake
                    .validate()
                    .map_err(|e| TransportError::Auth(AuthReject::Malformed(e.to_string())))?;
                if accept.session != accept.server_handshake.session
                    || accept.session.generation() == 0
                    || accept.server_handshake.limits != client_handshake.limits
                {
                    return Err(TransportError::Auth(AuthReject::Malformed(
                        "server returned inconsistent session or negotiated limits".into(),
                    )));
                }
                check_compatible(&client_handshake, &accept.server_handshake)
                    .map_err(|e| TransportError::Auth(AuthReject::Incompatible(e.to_string())))?;
                Connection::from_parts(
                    quic,
                    send,
                    recv,
                    effective_config(cfg, accept.server_handshake.limits)?,
                    accept.session,
                    Role::Client,
                )
            }
            ServerAuthReply::Rejected(reject) => Err(TransportError::Auth(reject)),
        }
    })
    .await
    .map_err(|_| TransportError::Timeout(cfg.handshake_timeout))?
}

fn client_bind_addr(target: SocketAddr) -> SocketAddr {
    match target {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
    }
}

fn remaining_handshake_time(
    started: Instant,
    total: std::time::Duration,
) -> Result<std::time::Duration> {
    total
        .checked_sub(started.elapsed())
        .ok_or(TransportError::Timeout(total))
}

fn handshake_with_local_limits(
    mut handshake: Handshake,
    cfg: TransportConfig,
) -> Result<Handshake> {
    if cfg.max_connections == 0
        || cfg.max_connections > 4096
        || cfg.max_pending_authentications == 0
        || cfg.max_pending_authentications > 4096
        || cfg.handshake_timeout.is_zero()
        || cfg.heartbeat_interval.is_zero()
        || cfg.idle_timeout.is_zero()
    {
        return Err(TransportError::Connect(
            "invalid transport counts or timers".into(),
        ));
    }
    let limits = effective_limits(cfg.limits, handshake.limits)?;
    handshake.limits = NegotiatedLimits {
        max_control_record: limits.max_control_record as u32,
        max_bulk_part: limits.max_bulk_part as u32,
        max_assembled_transfer: limits.max_assembled_transfer as u64,
        max_datagram_payload: limits.max_datagram_payload as u32,
    };
    handshake
        .validate()
        .map_err(|e| TransportError::Connect(e.to_string()))?;
    Ok(handshake)
}

fn effective_config(mut cfg: TransportConfig, peer: NegotiatedLimits) -> Result<TransportConfig> {
    cfg.limits = effective_limits(cfg.limits, peer)?;
    Ok(cfg)
}

fn effective_limits(
    local: crate::config::TransportLimits,
    peer: NegotiatedLimits,
) -> Result<crate::config::TransportLimits> {
    if local.max_control_record == 0
        || local.max_bulk_part == 0
        || local.max_assembled_transfer == 0
        || local.max_datagram_payload == 0
        || local.max_bulk_streams == 0
        || local.max_bulk_streams > 4
        || peer.max_control_record == 0
        || peer.max_bulk_part == 0
        || peer.max_assembled_transfer == 0
        || peer.max_datagram_payload == 0
    {
        return Err(TransportError::Connect(
            "transport limits must be non-zero".into(),
        ));
    }
    let peer_control = usize::try_from(peer.max_control_record)
        .map_err(|_| TransportError::Connect("peer control limit does not fit usize".into()))?;
    let peer_bulk = usize::try_from(peer.max_bulk_part)
        .map_err(|_| TransportError::Connect("peer bulk limit does not fit usize".into()))?;
    let peer_assembled = usize::try_from(peer.max_assembled_transfer)
        .map_err(|_| TransportError::Connect("peer assembled limit does not fit usize".into()))?;
    let peer_datagram = usize::try_from(peer.max_datagram_payload)
        .map_err(|_| TransportError::Connect("peer datagram limit does not fit usize".into()))?;
    Ok(crate::config::TransportLimits {
        max_control_record: local.max_control_record.min(peer_control),
        max_bulk_part: local.max_bulk_part.min(peer_bulk),
        max_assembled_transfer: local.max_assembled_transfer.min(peer_assembled),
        max_datagram_payload: local.max_datagram_payload.min(peer_datagram),
        max_bulk_streams: local.max_bulk_streams,
    })
}

/// Bounded reusable slot whose generation advances on reuse. Retained for the
/// server Connection lifetime; dropping it also releases failed authentications.
pub(crate) struct SessionLease {
    slots: Arc<Mutex<Vec<(u32, bool)>>>,
    session: SessionId,
}

impl SessionLease {
    fn acquire(slots: Arc<Mutex<Vec<(u32, bool)>>>) -> Result<Self> {
        let session = {
            let mut pool = slots.lock().unwrap_or_else(|e| e.into_inner());
            let (slot, state) = pool
                .iter_mut()
                .enumerate()
                .find(|(_, (generation, used))| !*used && *generation < u32::MAX)
                .ok_or(TransportError::Auth(AuthReject::Busy))?;
            state.0 += 1;
            state.1 = true;
            SessionId::from_parts(SlotId(slot as u32), state.0)
        };
        Ok(Self { slots, session })
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())[self.session.slot().0 as usize].1 =
            false;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_bind_uses_unspecified_address_in_target_family() {
        assert_eq!(
            client_bind_addr("192.0.2.5:4000".parse().unwrap()),
            "0.0.0.0:0".parse().unwrap()
        );
        assert_eq!(
            client_bind_addr("[::1]:4000".parse().unwrap()),
            "[::]:0".parse().unwrap()
        );
    }

    #[test]
    fn session_slots_are_bounded_and_reused_with_a_new_generation() {
        let slots = Arc::new(Mutex::new(vec![(0, false)]));
        let first = SessionLease::acquire(slots.clone()).unwrap();
        let id = first.session;
        assert!(SessionLease::acquire(slots.clone()).is_err());
        drop(first);
        let second = SessionLease::acquire(slots.clone()).unwrap();
        assert_eq!(second.session.slot(), id.slot());
        assert_eq!(second.session.generation(), id.generation() + 1);
        drop(second);
        slots.lock().unwrap()[0].0 = u32::MAX;
        assert!(SessionLease::acquire(slots).is_err());
    }
}
