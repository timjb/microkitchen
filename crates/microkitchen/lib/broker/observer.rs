//! The name observer (design §5): each sandbox's resolver.
//!
//! Queries are forwarded verbatim to the upstream resolvers and responses
//! returned unmodified. The observer never blocks, never filters and never
//! synthesizes: an upstream failure is passed on (or, on timeout, the client
//! times out too). It only records which names mapped to which addresses.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hickory_proto::op::Message;
use hickory_proto::rr::rdata::HTTPS;
use hickory_proto::rr::rdata::svcb::{IpHint, SvcParamValue};
use hickory_proto::rr::{Name, RData};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio_util::sync::CancellationToken;

use super::mediator::grammar::validate_hostname;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(3);

const MAX_DNS_MESSAGE: usize = 65_535;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Called with every response forwarded to the sandbox.
pub type Recorder = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// One learned binding: address, names, TTL in seconds.
pub type Learned = (IpAddr, Vec<String>, u32);

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Bindings in a DNS response: every A/AAAA address is bound to the query
/// name and every name in the CNAME chain; `HTTPS`/`SVCB` IP hints are bound
/// to the query, owner and target names. Anything unparseable yields nothing.
pub fn extract_bindings(response: &[u8]) -> Vec<Learned> {
    let Ok(message) = Message::from_vec(response) else {
        return Vec::new();
    };
    let query = message.queries.first().and_then(|q| normalize(&q.name));

    // The chain, in order: the query name, the CNAME targets it leads through,
    // then any other owner names in the answer.
    let cnames: Vec<(String, String)> = message
        .answers
        .iter()
        .filter_map(|record| match &record.data {
            RData::CNAME(target) => Some((normalize(&record.name)?, normalize(&target.0)?)),
            _ => None,
        })
        .collect();
    let mut chain: Vec<String> = Vec::new();
    let push = |chain: &mut Vec<String>, name: Option<String>| {
        if let Some(name) = name
            && !chain.contains(&name)
        {
            chain.push(name);
        }
    };
    push(&mut chain, query.clone());
    let mut current = query.clone();
    while let Some(target) = current
        .as_ref()
        .and_then(|name| cnames.iter().find(|(owner, _)| owner == name))
        .map(|(_, target)| target.clone())
    {
        if chain.contains(&target) {
            break;
        }
        chain.push(target.clone());
        current = Some(target);
    }
    for record in &message.answers {
        match &record.data {
            RData::CNAME(target) => {
                push(&mut chain, normalize(&record.name));
                push(&mut chain, normalize(&target.0));
            }
            RData::A(_) | RData::AAAA(_) => push(&mut chain, normalize(&record.name)),
            _ => {}
        }
    }

    let mut learned = Vec::new();
    for record in &message.answers {
        match &record.data {
            RData::A(a) => learned.push((IpAddr::V4(a.0), chain.clone(), record.ttl)),
            RData::AAAA(aaaa) => learned.push((IpAddr::V6(aaaa.0), chain.clone(), record.ttl)),
            RData::HTTPS(HTTPS(svcb)) | RData::SVCB(svcb) => {
                let mut names: Vec<String> = Vec::new();
                for name in [
                    query.clone(),
                    normalize(&record.name),
                    normalize(&svcb.target_name),
                ]
                .into_iter()
                .flatten()
                {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
                for (_, value) in &svcb.svc_params {
                    match value {
                        SvcParamValue::Ipv4Hint(IpHint(hints)) => {
                            learned.extend(
                                hints
                                    .iter()
                                    .map(|a| (IpAddr::V4(a.0), names.clone(), record.ttl)),
                            );
                        }
                        SvcParamValue::Ipv6Hint(IpHint(hints)) => {
                            learned.extend(
                                hints
                                    .iter()
                                    .map(|a| (IpAddr::V6(a.0), names.clone(), record.ttl)),
                            );
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    learned
}

/// Upstream resolvers: `configured` (`ip` or `ip:port`) or else the host's
/// `/etc/resolv.conf`.
pub fn upstreams(configured: &[String]) -> Result<Vec<SocketAddr>> {
    let upstreams: Vec<SocketAddr> = if configured.is_empty() {
        let text =
            std::fs::read_to_string("/etc/resolv.conf").context("reading /etc/resolv.conf")?;
        parse_resolv_conf(&text)
    } else {
        configured
            .iter()
            .map(|s| {
                s.parse::<SocketAddr>()
                    .or_else(|_| s.parse::<IpAddr>().map(|ip| SocketAddr::new(ip, 53)))
                    .with_context(|| format!("invalid upstream resolver {s:?}"))
            })
            .collect::<Result<_>>()?
    };
    if upstreams.is_empty() {
        bail!("no upstream DNS resolver: set broker.upstream_dns in ~/.microkitchen/config.toml");
    }
    Ok(upstreams)
}

pub fn parse_resolv_conf(text: &str) -> Vec<SocketAddr> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .filter_map(|rest| {
            let address = rest.trim().split('%').next()?;
            address.parse::<IpAddr>().ok()
        })
        .map(|ip| SocketAddr::new(ip, 53))
        .collect()
}

/// Serve DNS over UDP until cancelled.
pub async fn serve_udp(
    socket: UdpSocket,
    upstreams: Arc<[SocketAddr]>,
    record: Recorder,
    cancel: CancellationToken,
) {
    let socket = Arc::new(socket);
    let mut buffer = vec![0u8; MAX_DNS_MESSAGE];
    loop {
        let (len, client) = tokio::select! {
            _ = cancel.cancelled() => return,
            received = socket.recv_from(&mut buffer) => match received {
                Ok(received) => received,
                Err(_) => continue,
            },
        };
        let query = buffer[..len].to_vec();
        let (socket, upstreams, record) = (socket.clone(), upstreams.clone(), record.clone());
        tokio::spawn(async move {
            if let Some(response) = forward_udp(&query, &upstreams).await {
                record(&response);
                let _ = socket.send_to(&response, client).await;
            }
        });
    }
}

/// Serve DNS over TCP until cancelled.
pub async fn serve_tcp(
    listener: TcpListener,
    upstreams: Arc<[SocketAddr]>,
    record: Recorder,
    cancel: CancellationToken,
) {
    loop {
        let stream = tokio::select! {
            _ = cancel.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => continue,
            },
        };
        let (upstreams, record, cancel) = (upstreams.clone(), record.clone(), cancel.clone());
        tokio::spawn(async move { serve_tcp_client(stream, upstreams, record, cancel).await });
    }
}

async fn serve_tcp_client(
    mut stream: TcpStream,
    upstreams: Arc<[SocketAddr]>,
    record: Recorder,
    cancel: CancellationToken,
) {
    loop {
        let query = tokio::select! {
            _ = cancel.cancelled() => return,
            frame = read_frame(&mut stream) => match frame {
                Ok(Some(query)) => query,
                _ => return,
            },
        };
        let Some(response) = forward_tcp(&query, &upstreams).await else {
            return;
        };
        record(&response);
        if write_frame(&mut stream, &response).await.is_err() {
            return;
        }
    }
}

async fn forward_udp(query: &[u8], upstreams: &[SocketAddr]) -> Option<Vec<u8>> {
    if query.len() < 2 {
        return None;
    }
    for upstream in upstreams {
        let bind: SocketAddr = if upstream.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let Ok(socket) = UdpSocket::bind(bind).await else {
            continue;
        };
        if socket.send_to(query, upstream).await.is_err() {
            continue;
        }
        let deadline = tokio::time::Instant::now() + UPSTREAM_TIMEOUT;
        let mut buffer = vec![0u8; MAX_DNS_MESSAGE];
        loop {
            match tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await {
                Ok(Ok((len, from)))
                    if from == *upstream && len >= 2 && buffer[..2] == query[..2] =>
                {
                    return Some(buffer[..len].to_vec());
                }
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
    }
    None
}

async fn forward_tcp(query: &[u8], upstreams: &[SocketAddr]) -> Option<Vec<u8>> {
    for upstream in upstreams {
        let Ok(Ok(mut stream)) =
            tokio::time::timeout(UPSTREAM_TIMEOUT, TcpStream::connect(upstream)).await
        else {
            continue;
        };
        if write_frame(&mut stream, query).await.is_err() {
            continue;
        }
        if let Ok(Ok(Some(response))) =
            tokio::time::timeout(UPSTREAM_TIMEOUT, read_frame(&mut stream)).await
        {
            return Some(response);
        }
    }
    None
}

async fn read_frame(stream: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
    let len = match stream.read_u16().await {
        Ok(len) => len,
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut message = vec![0; usize::from(len)];
    stream.read_exact(&mut message).await?;
    Ok(Some(message))
}

async fn write_frame(stream: &mut TcpStream, message: &[u8]) -> io::Result<()> {
    let len = u16::try_from(message.len()).map_err(|_| io::Error::other("DNS message too long"))?;
    stream.write_u16(len).await?;
    stream.write_all(message).await
}

fn normalize(name: &Name) -> Option<String> {
    let text = name.to_ascii();
    let text = text.trim_end_matches('.').to_ascii_lowercase();
    validate_hostname(text.as_bytes()).ok()?;
    Some(text)
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Mutex;

    use hickory_proto::op::{MessageType, OpCode, Query};
    use hickory_proto::rr::rdata::svcb::{SVCB, SvcParamKey};
    use hickory_proto::rr::rdata::{A, AAAA, CNAME};
    use hickory_proto::rr::{Record, RecordType};

    use super::*;

    fn name(s: &str) -> Name {
        Name::from_str(s).unwrap()
    }

    fn response(query: &str, record_type: RecordType, answers: Vec<Record>) -> Vec<u8> {
        let mut message = Message::new(0x4242, MessageType::Response, OpCode::Query);
        message.add_query(Query::query(name(query), record_type));
        for answer in answers {
            message.add_answer(answer);
        }
        message.to_vec().unwrap()
    }

    #[test]
    fn binds_the_whole_cname_chain() {
        let bytes = response(
            "api.example.com.",
            RecordType::A,
            vec![
                Record::from_rdata(
                    name("api.example.com."),
                    30,
                    RData::CNAME(CNAME(name("example.map.cdn.net."))),
                ),
                Record::from_rdata(
                    name("example.map.cdn.net."),
                    30,
                    RData::A(A(Ipv4Addr::new(192, 0, 2, 1))),
                ),
                Record::from_rdata(
                    name("example.map.cdn.net."),
                    30,
                    RData::AAAA(AAAA("2001:db8::1".parse().unwrap())),
                ),
            ],
        );
        let learned = extract_bindings(&bytes);
        let chain = vec![
            "api.example.com".to_string(),
            "example.map.cdn.net".to_string(),
        ];
        assert_eq!(
            learned,
            vec![
                ("192.0.2.1".parse().unwrap(), chain.clone(), 30),
                ("2001:db8::1".parse().unwrap(), chain, 30),
            ]
        );
    }

    #[test]
    fn binds_https_hints() {
        let svcb = SVCB::new(
            1,
            name("svc.example.net."),
            vec![
                (
                    SvcParamKey::Ipv4Hint,
                    SvcParamValue::Ipv4Hint(IpHint(vec![A(Ipv4Addr::new(198, 51, 100, 9))])),
                ),
                (
                    SvcParamKey::Ipv6Hint,
                    SvcParamValue::Ipv6Hint(IpHint(vec![AAAA("2001:db8::9".parse().unwrap())])),
                ),
            ],
        );
        let bytes = response(
            "quic.example.com.",
            RecordType::HTTPS,
            vec![Record::from_rdata(
                name("quic.example.com."),
                60,
                RData::HTTPS(HTTPS(svcb)),
            )],
        );
        let names = vec![
            "quic.example.com".to_string(),
            "svc.example.net".to_string(),
        ];
        assert_eq!(
            extract_bindings(&bytes),
            vec![
                ("198.51.100.9".parse().unwrap(), names.clone(), 60),
                ("2001:db8::9".parse().unwrap(), names, 60),
            ]
        );
    }

    #[test]
    fn ignores_garbage() {
        assert!(extract_bindings(b"").is_empty());
        assert!(extract_bindings(&[0xff; 40]).is_empty());
    }

    #[test]
    fn reads_resolv_conf() {
        let text = "# comment\nnameserver 10.0.0.2\nnameserver fe80::1%eth0\nsearch lan\nnameserver bogus\n";
        assert_eq!(
            parse_resolv_conf(text),
            vec![
                "10.0.0.2:53".parse().unwrap(),
                "[fe80::1]:53".parse().unwrap()
            ]
        );
        assert_eq!(
            upstreams(&["1.1.1.1".into(), "127.0.0.1:5353".into()])
                .unwrap()
                .len(),
            2
        );
        assert!(upstreams(&["nope".into()]).is_err());
    }

    #[tokio::test]
    async fn forwards_unmodified_and_records() {
        // A fake upstream that answers every query with a fixed response.
        let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let answer = response(
            "a.test.",
            RecordType::A,
            vec![Record::from_rdata(
                name("a.test."),
                5,
                RData::A(A(Ipv4Addr::new(203, 0, 113, 1))),
            )],
        );
        let reply = answer.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            while let Ok((_, from)) = upstream.recv_from(&mut buf).await {
                let _ = upstream.send_to(&reply, from).await;
            }
        });

        let resolver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let resolver_addr = resolver.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Learned>>> = Arc::default();
        let sink = seen.clone();
        let record: Recorder =
            Arc::new(move |bytes| sink.lock().unwrap().extend(extract_bindings(bytes)));
        let cancel = CancellationToken::new();
        tokio::spawn(serve_udp(
            resolver,
            Arc::from(vec![upstream_addr]),
            record,
            cancel.clone(),
        ));

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(&answer[..], resolver_addr).await.unwrap(); // any query with id 0x4242
        let mut buf = [0u8; 512];
        let (len, _) = tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            &buf[..len],
            &answer[..],
            "responses are returned unmodified"
        );
        assert_eq!(seen.lock().unwrap()[0].1, vec!["a.test".to_string()]);
        cancel.cancel();
    }
}
