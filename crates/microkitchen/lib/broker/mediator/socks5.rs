//! SOCKS5 wire format (RFC 1928, RFC 1929), as used by microsandbox's client.
//!
//! Domain names go through the strict grammar and are rejected, never
//! repaired. UDP datagrams with a non-zero fragment field are rejected.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::grammar::{GrammarError, validate_hostname};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

pub const VERSION: u8 = 5;
pub const AUTH_VERSION: u8 = 1;
pub const METHOD_USER_PASS: u8 = 2;
pub const METHOD_NONE_ACCEPTABLE: u8 = 0xff;
pub const CMD_CONNECT: u8 = 1;
pub const CMD_UDP_ASSOCIATE: u8 = 3;
pub const ATYP_IPV4: u8 = 1;
pub const ATYP_DOMAIN: u8 = 3;
pub const ATYP_IPV6: u8 = 4;

/// Reply codes.
pub mod reply {
    pub const SUCCEEDED: u8 = 0;
    pub const GENERAL_FAILURE: u8 = 1;
    pub const NOT_ALLOWED: u8 = 2;
    pub const NETWORK_UNREACHABLE: u8 = 3;
    pub const HOST_UNREACHABLE: u8 = 4;
    pub const CONNECTION_REFUSED: u8 = 5;
    pub const COMMAND_NOT_SUPPORTED: u8 = 7;
}

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    Ip(SocketAddr),
    /// Lowercased, grammar-checked name.
    Domain(String, u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub command: u8,
    pub address: Address,
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("unsupported protocol version {0}")]
    Version(u8),

    #[error("reserved field is not zero")]
    Reserved,

    #[error("unsupported address type {0}")]
    AddressType(u8),

    #[error("invalid domain name: {0}")]
    Domain(#[from] GrammarError),

    #[error("fragmented datagram")]
    Fragmented,

    #[error("truncated datagram")]
    Truncated,
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// The client's method list.
pub async fn read_greeting<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, CodecError> {
    let version = reader.read_u8().await?;
    if version != VERSION {
        return Err(CodecError::Version(version));
    }
    let count = reader.read_u8().await?;
    let mut methods = vec![0; usize::from(count)];
    reader.read_exact(&mut methods).await?;
    Ok(methods)
}

/// RFC 1929 username and password.
pub async fn read_credentials<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<(Vec<u8>, Vec<u8>), CodecError> {
    let version = reader.read_u8().await?;
    if version != AUTH_VERSION {
        return Err(CodecError::Version(version));
    }
    let username_len = reader.read_u8().await?;
    let mut username = vec![0; usize::from(username_len)];
    reader.read_exact(&mut username).await?;
    let password_len = reader.read_u8().await?;
    let mut password = vec![0; usize::from(password_len)];
    reader.read_exact(&mut password).await?;
    Ok((username, password))
}

pub async fn read_request<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Request, CodecError> {
    let version = reader.read_u8().await?;
    if version != VERSION {
        return Err(CodecError::Version(version));
    }
    let command = reader.read_u8().await?;
    if reader.read_u8().await? != 0 {
        return Err(CodecError::Reserved);
    }
    let address_type = reader.read_u8().await?;
    let address = match address_type {
        ATYP_IPV4 => {
            let mut octets = [0; 4];
            reader.read_exact(&mut octets).await?;
            let port = reader.read_u16().await?;
            Address::Ip(SocketAddr::new(Ipv4Addr::from(octets).into(), port))
        }
        ATYP_IPV6 => {
            let mut octets = [0; 16];
            reader.read_exact(&mut octets).await?;
            let port = reader.read_u16().await?;
            Address::Ip(SocketAddr::new(Ipv6Addr::from(octets).into(), port))
        }
        ATYP_DOMAIN => {
            let len = reader.read_u8().await?;
            let mut name = vec![0; usize::from(len)];
            reader.read_exact(&mut name).await?;
            let port = reader.read_u16().await?;
            Address::Domain(validate_hostname(&name)?.to_ascii_lowercase(), port)
        }
        other => return Err(CodecError::AddressType(other)),
    };
    Ok(Request { command, address })
}

pub fn encode_reply(code: u8, bound: SocketAddr) -> Vec<u8> {
    let mut out = vec![VERSION, code, 0];
    encode_socket_addr(&mut out, bound);
    out
}

/// Split a client datagram into destination and payload.
pub fn parse_datagram(packet: &[u8]) -> Result<(Address, &[u8]), CodecError> {
    if packet.len() < 4 {
        return Err(CodecError::Truncated);
    }
    if packet[0] != 0 || packet[1] != 0 {
        return Err(CodecError::Reserved);
    }
    if packet[2] != 0 {
        return Err(CodecError::Fragmented);
    }
    let rest = &packet[4..];
    let take = |len: usize| rest.get(..len).ok_or(CodecError::Truncated);
    let port_at = |at: usize| -> Result<u16, CodecError> {
        let bytes = rest.get(at..at + 2).ok_or(CodecError::Truncated)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    };
    match packet[3] {
        ATYP_IPV4 => {
            let octets: [u8; 4] = take(4)?.try_into().expect("length checked");
            let port = port_at(4)?;
            Ok((
                Address::Ip(SocketAddr::new(Ipv4Addr::from(octets).into(), port)),
                &rest[6..],
            ))
        }
        ATYP_IPV6 => {
            let octets: [u8; 16] = take(16)?.try_into().expect("length checked");
            let port = port_at(16)?;
            Ok((
                Address::Ip(SocketAddr::new(Ipv6Addr::from(octets).into(), port)),
                &rest[18..],
            ))
        }
        ATYP_DOMAIN => {
            let len = usize::from(*rest.first().ok_or(CodecError::Truncated)?);
            let name = rest.get(1..1 + len).ok_or(CodecError::Truncated)?;
            let name = validate_hostname(name)?.to_ascii_lowercase();
            let port = port_at(1 + len)?;
            Ok((Address::Domain(name, port), &rest[1 + len + 2..]))
        }
        other => Err(CodecError::AddressType(other)),
    }
}

/// Wrap an upstream reply for the client.
pub fn encode_datagram(from: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0];
    encode_socket_addr(&mut out, from);
    out.extend_from_slice(payload);
    out
}

fn encode_socket_addr(out: &mut Vec<u8>, address: SocketAddr) {
    match address.ip() {
        IpAddr::V4(v4) => {
            out.push(ATYP_IPV4);
            out.extend_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            out.push(ATYP_IPV6);
            out.extend_from_slice(&v6.octets());
        }
    }
    out.extend_from_slice(&address.port().to_be_bytes());
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn request(bytes: &[u8]) -> Result<Request, CodecError> {
        read_request(&mut &bytes[..]).await
    }

    #[tokio::test]
    async fn reads_the_handshake() {
        assert_eq!(
            read_greeting(&mut &[5u8, 2, 0, 2][..]).await.unwrap(),
            vec![0, 2]
        );
        assert!(matches!(
            read_greeting(&mut &[4u8, 1, 0][..]).await,
            Err(CodecError::Version(4))
        ));

        let creds = [1u8, 3, b'a', b'b', b'c', 2, b'p', b'w'];
        let (user, pass) = read_credentials(&mut &creds[..]).await.unwrap();
        assert_eq!(
            (user.as_slice(), pass.as_slice()),
            (&b"abc"[..], &b"pw"[..])
        );
    }

    #[tokio::test]
    async fn reads_requests() {
        let v4 = request(&[5, 1, 0, 1, 203, 0, 113, 7, 1, 187])
            .await
            .unwrap();
        assert_eq!(
            v4,
            Request {
                command: CMD_CONNECT,
                address: Address::Ip("203.0.113.7:443".parse().unwrap())
            }
        );

        let mut v6 = vec![5, 3, 0, 4];
        v6.extend_from_slice(&"2001:db8::1".parse::<Ipv6Addr>().unwrap().octets());
        v6.extend_from_slice(&53u16.to_be_bytes());
        assert_eq!(
            request(&v6).await.unwrap().address,
            Address::Ip("[2001:db8::1]:53".parse().unwrap())
        );

        let mut domain = vec![5, 1, 0, 3, 11];
        domain.extend_from_slice(b"Example.COM");
        domain.extend_from_slice(&80u16.to_be_bytes());
        assert_eq!(
            request(&domain).await.unwrap().address,
            Address::Domain("example.com".into(), 80)
        );
    }

    #[tokio::test]
    async fn rejects_bad_domains_without_repair() {
        for name in [
            &b"example.com\0evil"[..],
            b"example.com\n",
            b"b\xc3\xbccher.de",
            b"",
        ] {
            let mut bytes = vec![5, 1, 0, 3, name.len() as u8];
            bytes.extend_from_slice(name);
            bytes.extend_from_slice(&80u16.to_be_bytes());
            assert!(
                matches!(request(&bytes).await, Err(CodecError::Domain(_))),
                "{name:?}"
            );
        }
        assert!(matches!(
            request(&[5, 1, 1, 1, 1, 1, 1, 1, 0, 80]).await,
            Err(CodecError::Reserved)
        ));
        assert!(matches!(
            request(&[5, 1, 0, 9]).await,
            Err(CodecError::AddressType(9))
        ));
        assert!(matches!(
            request(&[5, 1, 0, 1, 1]).await,
            Err(CodecError::Io(_))
        ));
    }

    #[test]
    fn encodes_replies() {
        assert_eq!(
            encode_reply(reply::NOT_ALLOWED, "0.0.0.0:0".parse().unwrap()),
            vec![5, 2, 0, 1, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            encode_reply(0, "127.0.0.1:4242".parse().unwrap()),
            vec![5, 0, 0, 1, 127, 0, 0, 1, 0x10, 0x92]
        );
    }

    #[test]
    fn datagrams_round_trip() {
        for from in ["198.51.100.7:123", "[2001:db8::7]:443"] {
            let from: SocketAddr = from.parse().unwrap();
            let packet = encode_datagram(from, b"payload");
            let (address, payload) = parse_datagram(&packet).unwrap();
            assert_eq!((address, payload), (Address::Ip(from), &b"payload"[..]));
        }
        let mut domain = vec![0, 0, 0, 3, 8];
        domain.extend_from_slice(b"ntp.test");
        domain.extend_from_slice(&123u16.to_be_bytes());
        domain.extend_from_slice(b"x");
        assert_eq!(
            parse_datagram(&domain).unwrap(),
            (Address::Domain("ntp.test".into(), 123), &b"x"[..])
        );
    }

    #[test]
    fn rejects_bad_datagrams() {
        let mut fragmented = encode_datagram("198.51.100.7:123".parse().unwrap(), b"x");
        fragmented[2] = 1;
        assert!(matches!(
            parse_datagram(&fragmented),
            Err(CodecError::Fragmented)
        ));
        assert!(matches!(
            parse_datagram(&[0, 0, 0]),
            Err(CodecError::Truncated)
        ));
        assert!(matches!(
            parse_datagram(&[0, 0, 0, 1, 1, 2]),
            Err(CodecError::Truncated)
        ));
        assert!(matches!(
            parse_datagram(&[0, 1, 0, 1, 1, 2, 3, 4, 0, 1]),
            Err(CodecError::Reserved)
        ));
        assert!(matches!(
            parse_datagram(&[0, 0, 0, 3, 3, b'a', 0, b'b', 0, 1]),
            Err(CodecError::Domain(_))
        ));
    }
}
