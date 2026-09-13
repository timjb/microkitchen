//! SOCKS5 flow mediation (design §7).

pub mod grammar;
pub mod socks5;
mod tcp;
mod udp;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use self::socks5::{
    AUTH_VERSION, CMD_CONNECT, CMD_UDP_ASSOCIATE, CodecError, METHOD_NONE_ACCEPTABLE,
    METHOD_USER_PASS, VERSION, encode_reply, reply,
};
use super::protocol::Transport;
use super::registry::{Broker, SandboxEntry};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const UNSPECIFIED: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Accept proxy connections for one sandbox until it is retired.
pub async fn serve(broker: Arc<Broker>, entry: Arc<SandboxEntry>, listener: TcpListener) {
    loop {
        let stream = tokio::select! {
            _ = entry.cancel.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::warn!(%error, "proxy accept failed");
                    continue;
                }
            },
        };
        let (broker, entry) = (broker.clone(), entry.clone());
        tokio::spawn(async move {
            if let Err(error) = handle(broker, entry, stream).await {
                tracing::debug!(%error, "proxy connection ended with an error");
            }
        });
    }
}

async fn handle(
    broker: Arc<Broker>,
    entry: Arc<SandboxEntry>,
    mut stream: TcpStream,
) -> Result<(), CodecError> {
    let methods = socks5::read_greeting(&mut stream).await?;
    if !methods.contains(&METHOD_USER_PASS) {
        stream.write_all(&[VERSION, METHOD_NONE_ACCEPTABLE]).await?;
        return Ok(());
    }
    stream.write_all(&[VERSION, METHOD_USER_PASS]).await?;

    let (username, password) = socks5::read_credentials(&mut stream).await?;
    if !entry.authenticate(&username, &password) {
        tracing::warn!(
            sandbox = %entry.name,
            "proxy authentication failed: the credentials do not belong to this endpoint's sandbox"
        );
        stream.write_all(&[AUTH_VERSION, 1]).await?;
        return Ok(());
    }
    stream.write_all(&[AUTH_VERSION, 0]).await?;

    let request = match socks5::read_request(&mut stream).await {
        Ok(request) => request,
        Err(CodecError::Domain(error)) => {
            broker.deny_malformed(&entry, Transport::Tcp, &error.to_string());
            stream
                .write_all(&encode_reply(reply::NOT_ALLOWED, UNSPECIFIED))
                .await?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    match request.command {
        CMD_CONNECT => tcp::connect(broker, entry, stream, request.address).await,
        CMD_UDP_ASSOCIATE => udp::associate(broker, entry, stream).await,
        _ => {
            stream
                .write_all(&encode_reply(reply::COMMAND_NOT_SUPPORTED, UNSPECIFIED))
                .await?;
            Ok(())
        }
    }
}

/// Resolve a SOCKS domain request ourselves; the name becomes the only candidate.
async fn resolve(name: &str, port: u16) -> Option<SocketAddr> {
    tokio::net::lookup_host((name, port)).await.ok()?.next()
}
