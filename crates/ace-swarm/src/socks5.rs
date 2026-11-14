//! Minimal SOCKS5 client (RFC 1928 `CONNECT` and `UDP ASSOCIATE`, RFC 1929 username/password).
//!
//! Only the negotiation is implemented here: once `CONNECT` succeeds the caller owns a plain
//! [`TcpStream`] whose bytes go straight through the proxy, so the per-chunk path is the same
//! as a direct connection. Every exchange is bounded by a caller-supplied timeout, every reply
//! is read into fixed-size stack buffers, and no error or `Debug` output carries credentials.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const VERSION: u8 = 0x05;
const AUTH_VERSION: u8 = 0x01;
const METHOD_NONE: u8 = 0x00;
const METHOD_USER_PASS: u8 = 0x02;
const METHOD_UNACCEPTABLE: u8 = 0xff;
pub(crate) const CMD_CONNECT: u8 = 0x01;
pub(crate) const CMD_UDP_ASSOCIATE: u8 = 0x03;
const ATYP_V4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_V6: u8 = 0x04;
/// RFC 1928 reply code for "command not supported".
pub const REPLY_COMMAND_NOT_SUPPORTED: u8 = 0x07;

/// Longest request: VER CMD RSV ATYP LEN DOMAIN(255) PORT(2).
const MAX_REQUEST: usize = 4 + 1 + 255 + 2;
/// Longest credential sub-negotiation: VER ULEN USER(255) PLEN PASS(255).
const MAX_AUTH: usize = 1 + 1 + 255 + 1 + 255;

/// Where the proxy should connect (or, for UDP, where a datagram is addressed).
/// A domain is resolved by the proxy, so the local resolver never sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAddr<'a> {
    Ip(SocketAddr),
    Domain(&'a str, u16),
}

/// Username/password for RFC 1929. `Debug` never prints either value.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials(<redacted>)")
    }
}

/// Why a SOCKS5 exchange failed. Carries no address, host name or credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Socks5Error {
    /// Could not reach the proxy itself.
    ProxyConnect,
    /// Reading or writing the negotiation failed.
    Io(io::ErrorKind),
    /// The whole negotiation exceeded its deadline.
    Timeout,
    /// The proxy answered with a version other than 5 (or 1 for the auth step).
    BadVersion,
    /// The proxy accepted none of the offered authentication methods.
    NoAcceptableMethod,
    /// The proxy rejected the username/password.
    AuthRejected,
    /// The proxy refused the request with this RFC 1928 reply code.
    Reply(u8),
    /// A username, password or domain is empty or longer than 255 bytes.
    InvalidField,
    /// The reply was structurally invalid.
    Malformed,
}

impl fmt::Display for Socks5Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProxyConnect => f.write_str("proxy unreachable"),
            Self::Io(kind) => write!(f, "proxy i/o error ({kind:?})"),
            Self::Timeout => f.write_str("proxy handshake timed out"),
            Self::BadVersion => f.write_str("proxy is not SOCKS5"),
            Self::NoAcceptableMethod => f.write_str("proxy accepts no offered auth method"),
            Self::AuthRejected => f.write_str("proxy rejected credentials"),
            Self::Reply(code) => write!(f, "proxy refused request (reply {code})"),
            Self::InvalidField => f.write_str("invalid proxy credential or target length"),
            Self::Malformed => f.write_str("malformed proxy reply"),
        }
    }
}

impl std::error::Error for Socks5Error {}

impl From<Socks5Error> for io::Error {
    fn from(error: Socks5Error) -> Self {
        let kind = match error {
            Socks5Error::Io(kind) => kind,
            Socks5Error::Timeout => io::ErrorKind::TimedOut,
            Socks5Error::ProxyConnect | Socks5Error::Reply(_) => io::ErrorKind::ConnectionRefused,
            _ => io::ErrorKind::InvalidData,
        };
        io::Error::new(kind, error)
    }
}

fn io_err(error: io::Error) -> Socks5Error {
    Socks5Error::Io(error.kind())
}

/// Method-selection greeting: offer no-auth, plus username/password when we have credentials.
pub(crate) fn encode_greeting(with_credentials: bool) -> &'static [u8] {
    if with_credentials {
        &[VERSION, 2, METHOD_NONE, METHOD_USER_PASS]
    } else {
        &[VERSION, 1, METHOD_NONE]
    }
}

/// The method the proxy chose. Username/password is only acceptable when we offered it.
pub(crate) fn parse_method_choice(
    reply: [u8; 2],
    offered_credentials: bool,
) -> Result<u8, Socks5Error> {
    if reply[0] != VERSION {
        return Err(Socks5Error::BadVersion);
    }
    match reply[1] {
        METHOD_NONE => Ok(METHOD_NONE),
        METHOD_USER_PASS if offered_credentials => Ok(METHOD_USER_PASS),
        METHOD_UNACCEPTABLE => Err(Socks5Error::NoAcceptableMethod),
        _ => Err(Socks5Error::Malformed),
    }
}

/// RFC 1929 sub-negotiation. Both fields must be 1..=255 bytes.
pub(crate) fn encode_auth(credentials: &Credentials, out: &mut Vec<u8>) -> Result<(), Socks5Error> {
    let user = credentials.username.as_bytes();
    let pass = credentials.password.as_bytes();
    if user.is_empty() || user.len() > 255 || pass.is_empty() || pass.len() > 255 {
        return Err(Socks5Error::InvalidField);
    }
    out.push(AUTH_VERSION);
    out.push(user.len() as u8);
    out.extend_from_slice(user);
    out.push(pass.len() as u8);
    out.extend_from_slice(pass);
    Ok(())
}

pub(crate) fn parse_auth_reply(reply: [u8; 2]) -> Result<(), Socks5Error> {
    if reply[0] != AUTH_VERSION {
        return Err(Socks5Error::BadVersion);
    }
    if reply[1] != 0 {
        return Err(Socks5Error::AuthRejected);
    }
    Ok(())
}

/// Append `ATYP ADDR PORT` for `target`.
pub(crate) fn encode_addr(target: TargetAddr<'_>, out: &mut Vec<u8>) -> Result<(), Socks5Error> {
    match target {
        TargetAddr::Ip(SocketAddr::V4(addr)) => {
            out.push(ATYP_V4);
            out.extend_from_slice(&addr.ip().octets());
            out.extend_from_slice(&addr.port().to_be_bytes());
        }
        TargetAddr::Ip(SocketAddr::V6(addr)) => {
            out.push(ATYP_V6);
            out.extend_from_slice(&addr.ip().octets());
            out.extend_from_slice(&addr.port().to_be_bytes());
        }
        TargetAddr::Domain(host, port) => {
            let host = host.as_bytes();
            if host.is_empty() || host.len() > 255 {
                return Err(Socks5Error::InvalidField);
            }
            out.push(ATYP_DOMAIN);
            out.push(host.len() as u8);
            out.extend_from_slice(host);
            out.extend_from_slice(&port.to_be_bytes());
        }
    }
    Ok(())
}

/// `VER CMD RSV ATYP DST.ADDR DST.PORT`.
pub(crate) fn encode_request(
    cmd: u8,
    target: TargetAddr<'_>,
    out: &mut Vec<u8>,
) -> Result<(), Socks5Error> {
    out.extend_from_slice(&[VERSION, cmd, 0]);
    encode_addr(target, out)
}

/// Validate the fixed reply head `VER REP RSV ATYP` and return how many address bytes follow
/// (excluding the two port bytes). A domain length needs one more byte, signalled by `None`.
pub(crate) fn parse_reply_head(head: [u8; 4]) -> Result<Option<usize>, Socks5Error> {
    if head[0] != VERSION {
        return Err(Socks5Error::BadVersion);
    }
    if head[1] != 0 {
        return Err(Socks5Error::Reply(head[1]));
    }
    if head[2] != 0 {
        return Err(Socks5Error::Malformed);
    }
    match head[3] {
        ATYP_V4 => Ok(Some(4)),
        ATYP_V6 => Ok(Some(16)),
        ATYP_DOMAIN => Ok(None),
        _ => Err(Socks5Error::Malformed),
    }
}

/// Decode a bound address. A domain-form bound address is not usable as a relay endpoint.
fn bound_addr(atyp_len: usize, addr: &[u8], port: [u8; 2]) -> Option<SocketAddr> {
    let port = u16::from_be_bytes(port);
    match atyp_len {
        4 => Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3])),
            port,
        )),
        16 => {
            let octets: [u8; 16] = addr.try_into().ok()?;
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        _ => None,
    }
}

/// Prefix `payload` with the RFC 1928 UDP request header addressed to `target` (no fragmentation).
pub fn encode_udp_datagram(
    target: TargetAddr<'_>,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), Socks5Error> {
    out.extend_from_slice(&[0, 0, 0]);
    encode_addr(target, out)?;
    out.extend_from_slice(payload);
    Ok(())
}

/// Split a relayed datagram into its source address and payload. Fragments and domain-form
/// sources are rejected: a tracker reply always comes from the numeric address we sent to.
pub fn decode_udp_datagram(datagram: &[u8]) -> Result<(SocketAddr, &[u8]), Socks5Error> {
    if datagram.len() < 4 || datagram[0] != 0 || datagram[1] != 0 {
        return Err(Socks5Error::Malformed);
    }
    if datagram[2] != 0 {
        return Err(Socks5Error::Malformed);
    }
    let len = match datagram[3] {
        ATYP_V4 => 4,
        ATYP_V6 => 16,
        _ => return Err(Socks5Error::Malformed),
    };
    let end = 4 + len + 2;
    if datagram.len() < end {
        return Err(Socks5Error::Malformed);
    }
    let source = bound_addr(
        len,
        &datagram[4..4 + len],
        [datagram[end - 2], datagram[end - 1]],
    )
    .ok_or(Socks5Error::Malformed)?;
    Ok((source, &datagram[end..]))
}

/// Run greeting, optional authentication, and one request on `stream`; return the bound
/// address from the reply (if numeric).
async fn negotiate(
    stream: &mut TcpStream,
    credentials: Option<&Credentials>,
    cmd: u8,
    target: TargetAddr<'_>,
) -> Result<Option<SocketAddr>, Socks5Error> {
    stream
        .write_all(encode_greeting(credentials.is_some()))
        .await
        .map_err(io_err)?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await.map_err(io_err)?;
    if parse_method_choice(choice, credentials.is_some())? == METHOD_USER_PASS {
        let credentials = credentials.ok_or(Socks5Error::Malformed)?;
        let mut auth = Vec::with_capacity(MAX_AUTH);
        encode_auth(credentials, &mut auth)?;
        stream.write_all(&auth).await.map_err(io_err)?;
        let mut reply = [0u8; 2];
        stream.read_exact(&mut reply).await.map_err(io_err)?;
        parse_auth_reply(reply)?;
    }

    let mut request = Vec::with_capacity(MAX_REQUEST);
    encode_request(cmd, target, &mut request)?;
    stream.write_all(&request).await.map_err(io_err)?;

    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await.map_err(io_err)?;
    let addr_len = match parse_reply_head(head)? {
        Some(len) => len,
        None => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(io_err)?;
            usize::from(len[0])
        }
    };
    let mut addr = [0u8; 255];
    stream
        .read_exact(&mut addr[..addr_len])
        .await
        .map_err(io_err)?;
    let mut port = [0u8; 2];
    stream.read_exact(&mut port).await.map_err(io_err)?;
    let numeric = matches!(head[3], ATYP_V4 | ATYP_V6);
    Ok(numeric
        .then(|| bound_addr(addr_len, &addr[..addr_len], port))
        .flatten())
}

async fn open_proxy(host: &str, port: u16) -> Result<TcpStream, Socks5Error> {
    TcpStream::connect((host, port))
        .await
        .map_err(|_| Socks5Error::ProxyConnect)
}

/// Open a TCP connection to `target` through the proxy at `proxy_host:proxy_port`.
/// The proxy connect and the whole negotiation share one `timeout`.
pub async fn connect(
    proxy_host: &str,
    proxy_port: u16,
    credentials: Option<&Credentials>,
    target: TargetAddr<'_>,
    timeout: Duration,
) -> Result<TcpStream, Socks5Error> {
    tokio::time::timeout(timeout, async {
        let mut stream = open_proxy(proxy_host, proxy_port).await?;
        negotiate(&mut stream, credentials, CMD_CONNECT, target).await?;
        Ok(stream)
    })
    .await
    .map_err(|_| Socks5Error::Timeout)?
}

/// Ask the proxy for a UDP relay. Returns the control connection, which must stay open for
/// as long as the relay is used, and the relay address. A wildcard relay address means "the
/// proxy's own address", per common practice.
pub async fn udp_associate(
    proxy_host: &str,
    proxy_port: u16,
    credentials: Option<&Credentials>,
    timeout: Duration,
) -> Result<(TcpStream, SocketAddr), Socks5Error> {
    tokio::time::timeout(timeout, async {
        let mut stream = open_proxy(proxy_host, proxy_port).await?;
        let any = TargetAddr::Ip(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
        let bound = negotiate(&mut stream, credentials, CMD_UDP_ASSOCIATE, any)
            .await?
            .ok_or(Socks5Error::Malformed)?;
        if bound.port() == 0 {
            return Err(Socks5Error::Malformed);
        }
        let relay = if bound.ip().is_unspecified() {
            SocketAddr::new(stream.peer_addr().map_err(io_err)?.ip(), bound.port())
        } else {
            bound
        };
        Ok((stream, relay))
    })
    .await
    .map_err(|_| Socks5Error::Timeout)?
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::net::{TcpListener, UdpSocket};

    fn creds() -> Credentials {
        Credentials {
            username: "user".into(),
            password: "secret-pass".into(),
        }
    }

    #[test]
    fn greeting_offers_userpass_only_with_credentials() {
        assert_eq!(encode_greeting(false), &[5, 1, 0]);
        assert_eq!(encode_greeting(true), &[5, 2, 0, 2]);
    }

    #[test]
    fn method_choice_rejects_wrong_version_unacceptable_and_unoffered_auth() {
        assert_eq!(parse_method_choice([5, 0], false), Ok(0));
        assert_eq!(parse_method_choice([5, 2], true), Ok(2));
        assert_eq!(
            parse_method_choice([5, 2], false),
            Err(Socks5Error::Malformed)
        );
        assert_eq!(
            parse_method_choice([5, 0xff], true),
            Err(Socks5Error::NoAcceptableMethod)
        );
        assert_eq!(
            parse_method_choice([4, 0], false),
            Err(Socks5Error::BadVersion)
        );
    }

    #[test]
    fn auth_encoding_and_reply() {
        let mut out = Vec::new();
        encode_auth(&creds(), &mut out).unwrap();
        assert_eq!(out, b"\x01\x04user\x0bsecret-pass");
        assert_eq!(parse_auth_reply([1, 0]), Ok(()));
        assert_eq!(parse_auth_reply([1, 1]), Err(Socks5Error::AuthRejected));
        assert_eq!(parse_auth_reply([5, 0]), Err(Socks5Error::BadVersion));
        for (user, pass) in [("", "p"), ("u", ""), (&*"u".repeat(256), "p")] {
            let bad = Credentials {
                username: user.into(),
                password: pass.into(),
            };
            assert_eq!(
                encode_auth(&bad, &mut Vec::new()),
                Err(Socks5Error::InvalidField)
            );
        }
    }

    #[test]
    fn request_encodes_v4_v6_and_domain_targets() {
        let mut out = Vec::new();
        encode_request(
            CMD_CONNECT,
            TargetAddr::Ip("1.2.3.4:8621".parse().unwrap()),
            &mut out,
        )
        .unwrap();
        assert_eq!(out, [5, 1, 0, 1, 1, 2, 3, 4, 0x21, 0xad]);

        out.clear();
        encode_request(
            CMD_CONNECT,
            TargetAddr::Ip("[::1]:80".parse().unwrap()),
            &mut out,
        )
        .unwrap();
        assert_eq!(&out[..4], [5, 1, 0, 4]);
        assert_eq!(out[4..20], Ipv6Addr::LOCALHOST.octets());
        assert_eq!(&out[20..], [0, 80]);

        out.clear();
        encode_request(
            CMD_UDP_ASSOCIATE,
            TargetAddr::Domain("example.invalid", 443),
            &mut out,
        )
        .unwrap();
        let mut expected = vec![5, 3, 0, 3, 15];
        expected.extend_from_slice(b"example.invalid");
        expected.extend_from_slice(&[1, 0xbb]);
        assert_eq!(out, expected);

        assert_eq!(
            encode_request(CMD_CONNECT, TargetAddr::Domain("", 1), &mut Vec::new()),
            Err(Socks5Error::InvalidField)
        );
        assert_eq!(
            encode_request(
                CMD_CONNECT,
                TargetAddr::Domain(&"a".repeat(256), 1),
                &mut Vec::new()
            ),
            Err(Socks5Error::InvalidField)
        );
    }

    #[test]
    fn reply_head_decodes_lengths_and_errors() {
        assert_eq!(parse_reply_head([5, 0, 0, 1]), Ok(Some(4)));
        assert_eq!(parse_reply_head([5, 0, 0, 4]), Ok(Some(16)));
        assert_eq!(parse_reply_head([5, 0, 0, 3]), Ok(None));
        assert_eq!(parse_reply_head([5, 5, 0, 1]), Err(Socks5Error::Reply(5)));
        assert_eq!(parse_reply_head([4, 0, 0, 1]), Err(Socks5Error::BadVersion));
        assert_eq!(parse_reply_head([5, 0, 1, 1]), Err(Socks5Error::Malformed));
        assert_eq!(parse_reply_head([5, 0, 0, 9]), Err(Socks5Error::Malformed));
    }

    #[test]
    fn udp_datagram_roundtrip_and_rejections() {
        let target: SocketAddr = "10.0.0.1:6969".parse().unwrap();
        let mut out = Vec::new();
        encode_udp_datagram(TargetAddr::Ip(target), b"hello", &mut out).unwrap();
        assert_eq!(&out[..10], [0, 0, 0, 1, 10, 0, 0, 1, 0x1b, 0x39]);
        let (source, payload) = decode_udp_datagram(&out).unwrap();
        assert_eq!((source, payload), (target, &b"hello"[..]));

        let v6: SocketAddr = "[2001:db8::1]:1".parse().unwrap();
        out.clear();
        encode_udp_datagram(TargetAddr::Ip(v6), b"", &mut out).unwrap();
        assert_eq!(decode_udp_datagram(&out).unwrap(), (v6, &b""[..]));

        let mut fragment = vec![0, 0, 1, 1, 10, 0, 0, 1, 0, 1];
        assert_eq!(decode_udp_datagram(&fragment), Err(Socks5Error::Malformed));
        fragment[2] = 0;
        assert!(decode_udp_datagram(&fragment).is_ok());
        assert_eq!(
            decode_udp_datagram(&fragment[..9]),
            Err(Socks5Error::Malformed)
        );
        assert_eq!(
            decode_udp_datagram(&[0, 0, 0, 3, 1, b'a', 0, 1]),
            Err(Socks5Error::Malformed)
        );
        assert_eq!(
            decode_udp_datagram(&[1, 0, 0, 1]),
            Err(Socks5Error::Malformed)
        );
    }

    #[test]
    fn debug_and_errors_never_print_credentials() {
        let debug = format!("{:?}", creds());
        assert!(!debug.contains("user") && !debug.contains("secret"));
        for error in [
            Socks5Error::AuthRejected,
            Socks5Error::Reply(2),
            Socks5Error::InvalidField,
            Socks5Error::Io(io::ErrorKind::BrokenPipe),
        ] {
            let text = format!("{error} {error:?} {}", io::Error::from(error));
            assert!(!text.contains("secret"));
        }
    }

    /// What the fake proxy saw and how it should answer.
    #[derive(Clone, Copy)]
    pub(crate) struct FakeProxy {
        pub require_auth: bool,
        pub reply: u8,
        /// For UDP ASSOCIATE: the relay socket address to report.
        pub relay: Option<SocketAddr>,
    }

    /// Serve exactly one SOCKS5 client on `listener`. On `CONNECT` success, relay bytes to the
    /// requested numeric target; on `UDP ASSOCIATE` success, report `relay` and hold the control
    /// connection open until the client closes it. Returns the request's raw `ATYP ADDR PORT`.
    pub(crate) async fn serve_one(listener: TcpListener, proxy: FakeProxy) -> Vec<u8> {
        let (mut client, _) = listener.accept().await.unwrap();
        let mut head = [0u8; 2];
        client.read_exact(&mut head).await.unwrap();
        let mut methods = vec![0u8; head[1] as usize];
        client.read_exact(&mut methods).await.unwrap();
        if proxy.require_auth {
            if !methods.contains(&METHOD_USER_PASS) {
                client.write_all(&[5, 0xff]).await.unwrap();
                return Vec::new();
            }
            client.write_all(&[5, 2]).await.unwrap();
            let mut ver_len = [0u8; 2];
            client.read_exact(&mut ver_len).await.unwrap();
            let mut user = vec![0u8; ver_len[1] as usize];
            client.read_exact(&mut user).await.unwrap();
            let mut plen = [0u8; 1];
            client.read_exact(&mut plen).await.unwrap();
            let mut pass = vec![0u8; plen[0] as usize];
            client.read_exact(&mut pass).await.unwrap();
            let ok = user == b"user" && pass == b"secret-pass";
            client.write_all(&[1, u8::from(!ok)]).await.unwrap();
            if !ok {
                return Vec::new();
            }
        } else {
            client.write_all(&[5, 0]).await.unwrap();
        }
        let mut req = [0u8; 4];
        client.read_exact(&mut req).await.unwrap();
        let mut addr = vec![req[3]];
        let len = match req[3] {
            1 => 4,
            4 => 16,
            _ => {
                let mut l = [0u8; 1];
                client.read_exact(&mut l).await.unwrap();
                addr.push(l[0]);
                l[0] as usize
            }
        };
        let mut rest = vec![0u8; len + 2];
        client.read_exact(&mut rest).await.unwrap();
        addr.extend_from_slice(&rest);
        if proxy.reply != 0 {
            client
                .write_all(&[5, proxy.reply, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            return addr;
        }
        match req[1] {
            CMD_CONNECT => {
                let target = match req[3] {
                    1 => SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::new(rest[0], rest[1], rest[2], rest[3])),
                        u16::from_be_bytes([rest[4], rest[5]]),
                    ),
                    // Domain targets: answer for "localhost" with loopback only.
                    _ => {
                        let port = u16::from_be_bytes([rest[len], rest[len + 1]]);
                        assert_eq!(&rest[..len], b"localhost");
                        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
                    }
                };
                let mut upstream = TcpStream::connect(target).await.unwrap();
                // Domain-form bound address exercises the variable-length reply path.
                client
                    .write_all(&[5, 0, 0, 3, 4, b'b', b'n', b'd', b'x', 0, 1])
                    .await
                    .unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            }
            CMD_UDP_ASSOCIATE => {
                let relay = proxy.relay.expect("relay address");
                let mut reply = vec![5, 0, 0, 1];
                match relay {
                    SocketAddr::V4(v4) => reply.extend_from_slice(&v4.ip().octets()),
                    SocketAddr::V6(_) => unreachable!(),
                }
                reply.extend_from_slice(&relay.port().to_be_bytes());
                client.write_all(&reply).await.unwrap();
                let mut sink = [0u8; 1];
                let _ = client.read(&mut sink).await;
            }
            _ => unreachable!(),
        }
        addr
    }

    async fn echo_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = socket.split();
            let _ = tokio::io::copy(&mut r, &mut w).await;
        });
        addr
    }

    async fn proxy(config: FakeProxy) -> (SocketAddr, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (addr, tokio::spawn(serve_one(listener, config)))
    }

    const NO_AUTH: FakeProxy = FakeProxy {
        require_auth: false,
        reply: 0,
        relay: None,
    };

    #[tokio::test]
    async fn connect_through_fake_proxy_relays_bytes_to_numeric_target() {
        let echo = echo_server().await;
        let (addr, seen) = proxy(NO_AUTH).await;
        let mut stream = connect(
            "127.0.0.1",
            addr.port(),
            None,
            TargetAddr::Ip(echo),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        drop(stream);
        let mut expected = vec![1, 127, 0, 0, 1];
        expected.extend_from_slice(&echo.port().to_be_bytes());
        assert_eq!(seen.await.unwrap(), expected);
    }

    #[tokio::test]
    async fn connect_with_credentials_sends_domain_for_remote_resolution() {
        let echo = echo_server().await;
        let (addr, seen) = proxy(FakeProxy {
            require_auth: true,
            ..NO_AUTH
        })
        .await;
        let mut stream = connect(
            "127.0.0.1",
            addr.port(),
            Some(&creds()),
            TargetAddr::Domain("localhost", echo.port()),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        stream.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        stream.read_exact(&mut buf).await.unwrap();
        drop(stream);
        let seen = seen.await.unwrap();
        assert_eq!(&seen[..2], [3, 9]);
        assert_eq!(&seen[2..11], b"localhost");
    }

    #[tokio::test]
    async fn wrong_credentials_and_refusals_surface_typed_errors() {
        let (addr, _seen) = proxy(FakeProxy {
            require_auth: true,
            ..NO_AUTH
        })
        .await;
        let wrong = Credentials {
            username: "user".into(),
            password: "nope".into(),
        };
        let target = TargetAddr::Ip("127.0.0.1:9".parse().unwrap());
        let err = connect(
            "127.0.0.1",
            addr.port(),
            Some(&wrong),
            target,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert_eq!(err, Socks5Error::AuthRejected);

        let (addr, _seen) = proxy(FakeProxy {
            require_auth: true,
            ..NO_AUTH
        })
        .await;
        let err = connect(
            "127.0.0.1",
            addr.port(),
            None,
            target,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert_eq!(err, Socks5Error::NoAcceptableMethod);

        let (addr, _seen) = proxy(FakeProxy {
            reply: 5,
            ..NO_AUTH
        })
        .await;
        let err = connect(
            "127.0.0.1",
            addr.port(),
            None,
            target,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert_eq!(err, Socks5Error::Reply(5));
    }

    #[tokio::test]
    async fn silent_proxy_is_bounded_by_the_handshake_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _hold = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(socket);
        });
        let started = std::time::Instant::now();
        let err = connect(
            "127.0.0.1",
            addr.port(),
            None,
            TargetAddr::Ip("127.0.0.1:9".parse().unwrap()),
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        assert_eq!(err, Socks5Error::Timeout);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn udp_associate_reports_relay_and_wildcard_maps_to_proxy_ip() {
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay.local_addr().unwrap();
        let (addr, _seen) = proxy(FakeProxy {
            relay: Some(relay_addr),
            ..NO_AUTH
        })
        .await;
        let (_control, got) = udp_associate("127.0.0.1", addr.port(), None, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(got, relay_addr);

        let wildcard: SocketAddr = format!("0.0.0.0:{}", relay_addr.port()).parse().unwrap();
        let (addr, _seen) = proxy(FakeProxy {
            relay: Some(wildcard),
            ..NO_AUTH
        })
        .await;
        let (_control, got) = udp_associate("127.0.0.1", addr.port(), None, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(got, relay_addr);

        let (addr, _seen) = proxy(FakeProxy {
            reply: REPLY_COMMAND_NOT_SUPPORTED,
            ..NO_AUTH
        })
        .await;
        let err = udp_associate("127.0.0.1", addr.port(), None, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(err, Socks5Error::Reply(REPLY_COMMAND_NOT_SUPPORTED));
    }
}
