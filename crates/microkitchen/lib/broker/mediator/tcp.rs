//! `CONNECT`: hold the reply while deciding, then splice with half-close.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, copy_bidirectional};
use tokio::net::TcpStream;

use super::socks5::{Address, CodecError, encode_reply, reply};
use super::{UNSPECIFIED, resolve};
use crate::broker::protocol::Transport;
use crate::broker::registry::{Broker, SandboxEntry};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

pub(super) async fn connect(
    broker: Arc<Broker>,
    entry: Arc<SandboxEntry>,
    mut client: TcpStream,
    address: Address,
) -> Result<(), CodecError> {
    let (target, claimed) = match address {
        Address::Ip(target) => (target, None),
        Address::Domain(name, port) => match resolve(&name, port).await {
            Some(target) => (target, Some(name)),
            None => {
                client
                    .write_all(&encode_reply(reply::HOST_UNREACHABLE, UNSPECIFIED))
                    .await?;
                return Ok(());
            }
        },
    };

    // The client sends nothing before our reply, so EOF while the decision is
    // pending means the flow is gone and its approval must not be shown.
    let cancel = entry.cancel.child_token();
    let allowed = tokio::select! {
        allowed = broker.admit(&entry, Transport::Tcp, target.ip(), target.port(), claimed, &cancel) => allowed,
        () = client_gone(&client) => {
            cancel.cancel();
            return Ok(());
        }
    };
    if !allowed {
        client
            .write_all(&encode_reply(reply::NOT_ALLOWED, UNSPECIFIED))
            .await?;
        return Ok(());
    }

    let mut upstream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target)).await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            client
                .write_all(&encode_reply(reply_for(&error), UNSPECIFIED))
                .await?;
            return Ok(());
        }
        Err(_) => {
            client
                .write_all(&encode_reply(reply::HOST_UNREACHABLE, UNSPECIFIED))
                .await?;
            return Ok(());
        }
    };
    let bound = upstream.local_addr().unwrap_or(UNSPECIFIED);
    client
        .write_all(&encode_reply(reply::SUCCEEDED, bound))
        .await?;

    // copy_bidirectional shuts down each write side when the other side
    // reaches EOF, so one direction finishing does not truncate the other.
    tokio::select! {
        _ = copy_bidirectional(&mut client, &mut upstream) => {}
        () = entry.cancel.cancelled() => {}
    }
    Ok(())
}

async fn client_gone(stream: &TcpStream) {
    let mut byte = [0u8; 1];
    match stream.peek(&mut byte).await {
        Ok(0) | Err(_) => {}
        // Early data from a misbehaving client: keep waiting for the decision.
        Ok(_) => std::future::pending().await,
    }
}

fn reply_for(error: &io::Error) -> u8 {
    match error.kind() {
        io::ErrorKind::ConnectionRefused => reply::CONNECTION_REFUSED,
        io::ErrorKind::NetworkUnreachable => reply::NETWORK_UNREACHABLE,
        io::ErrorKind::HostUnreachable => reply::HOST_UNREACHABLE,
        _ => reply::GENERAL_FAILURE,
    }
}
