//! `UDP ASSOCIATE` (design §7.3).
//!
//! The control connection is the association's lifetime. The relay accepts
//! datagrams from its first sender only, drops fragments, decides once per
//! destination (queueing a few datagrams meanwhile), and accepts replies only
//! from destinations it was allowed to send to.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::resolve;
use super::socks5::{self, Address, CodecError, encode_reply, reply};
use crate::broker::protocol::Transport;
use crate::broker::registry::{Broker, SandboxEntry};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Datagrams held per destination while its approval is pending.
pub const PENDING_QUEUE_LIMIT: usize = 16;

const MAX_DATAGRAM: usize = 65_535;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

enum Verdict {
    Pending(Vec<Vec<u8>>),
    Allowed,
    Denied,
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

pub(super) async fn associate(
    broker: Arc<Broker>,
    entry: Arc<SandboxEntry>,
    mut control: TcpStream,
) -> Result<(), CodecError> {
    // A concrete loopback address, never a wildcard.
    let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    control
        .write_all(&encode_reply(reply::SUCCEEDED, relay.local_addr()?))
        .await?;

    let cancel = entry.cancel.child_token();
    tokio::select! {
        () = wait_for_close(&mut control) => {}
        result = relay_loop(&broker, &entry, &relay, &cancel) => {
            if let Err(error) = result {
                tracing::debug!(%error, "UDP association failed");
            }
        }
        () = entry.cancel.cancelled() => {}
    }
    // Ends pending approvals of this association; sockets drop with this frame.
    cancel.cancel();
    Ok(())
}

async fn wait_for_close(control: &mut TcpStream) {
    let mut buffer = [0u8; 256];
    loop {
        match control.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(_) => continue,
        }
    }
}

async fn relay_loop(
    broker: &Arc<Broker>,
    entry: &Arc<SandboxEntry>,
    relay: &UdpSocket,
    cancel: &CancellationToken,
) -> io::Result<()> {
    let upstream_v4 = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    let upstream_v6 = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).await.ok();
    let (decided_tx, mut decided) = mpsc::unbounded_channel::<(SocketAddr, bool)>();

    let mut client: Option<SocketAddr> = None;
    let mut verdicts: HashMap<SocketAddr, Verdict> = HashMap::new();
    let mut from_client = vec![0u8; MAX_DATAGRAM];
    let mut from_v4 = vec![0u8; MAX_DATAGRAM];
    let mut from_v6 = vec![0u8; MAX_DATAGRAM];

    loop {
        tokio::select! {
            received = relay.recv_from(&mut from_client) => {
                let (len, source) = received?;
                // The relay is reachable by anything on loopback: the first sender owns it.
                if *client.get_or_insert(source) != source {
                    continue;
                }
                let (address, payload) = match socks5::parse_datagram(&from_client[..len]) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        tracing::debug!(sandbox = %entry.name, %error, "dropping datagram");
                        continue;
                    }
                };
                let payload = payload.to_vec();
                let (target, claimed) = match address {
                    Address::Ip(target) => (target, None),
                    Address::Domain(name, port) => match resolve(&name, port).await {
                        Some(target) => (target, Some(name)),
                        None => continue,
                    },
                };
                let target = SocketAddr::new(target.ip().to_canonical(), target.port());
                match verdicts.get_mut(&target) {
                    Some(Verdict::Allowed) => send(&upstream_v4, upstream_v6.as_ref(), &payload, target).await,
                    Some(Verdict::Denied) => {}
                    Some(Verdict::Pending(queue)) => {
                        if queue.len() < PENDING_QUEUE_LIMIT {
                            queue.push(payload);
                        }
                    }
                    None => {
                        verdicts.insert(target, Verdict::Pending(vec![payload]));
                        let (broker, entry, cancel, decided_tx) = (broker.clone(), entry.clone(), cancel.clone(), decided_tx.clone());
                        tokio::spawn(async move {
                            let allowed = broker.admit(&entry, Transport::Udp, target.ip(), target.port(), claimed, &cancel).await;
                            let _ = decided_tx.send((target, allowed));
                        });
                    }
                }
            }
            Some((target, allowed)) = decided.recv() => {
                let verdict = if allowed { Verdict::Allowed } else { Verdict::Denied };
                if let Some(Verdict::Pending(queued)) = verdicts.insert(target, verdict)
                    && allowed
                {
                    for payload in queued {
                        send(&upstream_v4, upstream_v6.as_ref(), &payload, target).await;
                    }
                }
            }
            received = upstream_v4.recv_from(&mut from_v4) => {
                let (len, from) = received?;
                forward_reply(relay, client, &verdicts, from, &from_v4[..len]).await;
            }
            received = recv_optional(upstream_v6.as_ref(), &mut from_v6) => {
                if let Ok((len, from)) = received {
                    let from = SocketAddr::new(from.ip().to_canonical(), from.port());
                    forward_reply(relay, client, &verdicts, from, &from_v6[..len]).await;
                }
            }
        }
    }
}

async fn recv_optional(
    socket: Option<&UdpSocket>,
    buffer: &mut [u8],
) -> io::Result<(usize, SocketAddr)> {
    match socket {
        Some(socket) => socket.recv_from(buffer).await,
        None => std::future::pending().await,
    }
}

async fn send(v4: &UdpSocket, v6: Option<&UdpSocket>, payload: &[u8], target: SocketAddr) {
    let socket = if target.is_ipv4() { Some(v4) } else { v6 };
    if let Some(socket) = socket {
        let _ = socket.send_to(payload, target).await;
    }
}

/// Replies are accepted only from exactly where an allowed datagram went:
/// an unrestricted relay would be an open reflector.
async fn forward_reply(
    relay: &UdpSocket,
    client: Option<SocketAddr>,
    verdicts: &HashMap<SocketAddr, Verdict>,
    from: SocketAddr,
    payload: &[u8],
) {
    let Some(client) = client else {
        return;
    };
    if !matches!(verdicts.get(&from), Some(Verdict::Allowed)) {
        return;
    }
    let _ = relay
        .send_to(&socks5::encode_datagram(from, payload), client)
        .await;
}
