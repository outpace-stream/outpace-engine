//! Optional outbound SOCKS5 proxy for the engine's own peer, tracker and catalog traffic
//! (`OUTPACE_PROXY`, `OUTPACE_PROXY_MODE`).
//!
//! Off by default. When off nothing is installed and every dial takes its original direct
//! path. When active:
//! - outbound TCP peer connections and catalog/search HTTP requests use SOCKS5 `CONNECT`
//!   (host names are sent to the proxy and resolved there);
//! - UDP tracker announces use SOCKS5 `UDP ASSOCIATE`; if the proxy refuses that command, UDP
//!   trackers are disabled for the rest of the process rather than sent directly;
//! - DHT lookups and announces are disabled (never sent directly);
//! - the engine advertises no listen port and does not self-announce, since peers cannot reach
//!   it through the proxy.
//!
//! `fallback` starts direct and engages the proxy engine-wide, for the rest of the process,
//! the first time a live start finds no peers.

use crate::socks5::{self, Credentials, Socks5Error, TargetAddr};
use ace_peer::session::PeerSession;
use ace_tracker::codec::{
    build_announce_request, build_connect_request, parse_announce_response, parse_connect_response,
    AnnounceEvent, TransferState,
};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::net::{TcpStream, UdpSocket};

/// Ceiling for one SOCKS5 negotiation (proxy connect + auth + request) on non-peer paths.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Ceiling for one proxied UDP tracker exchange, matching the direct client's receive timeout.
const UDP_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(8);
/// Datagrams from unexpected sources tolerated per reply before giving up.
const MAX_STRAY_DATAGRAMS: usize = 8;

/// `OUTPACE_PROXY_MODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyMode {
    #[default]
    Off,
    On,
    Fallback,
}

impl ProxyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Fallback => "fallback",
        }
    }
}

impl std::str::FromStr for ProxyMode {
    type Err = InvalidProxy;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "on" => Ok(Self::On),
            "fallback" => Ok(Self::Fallback),
            _ => Err(InvalidProxy(
                "invalid OUTPACE_PROXY_MODE: expected off, on or fallback",
            )),
        }
    }
}

/// A configuration error. The message never echoes the URL, which may hold a password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidProxy(pub &'static str);

impl fmt::Display for InvalidProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for InvalidProxy {}

/// A parsed `socks5://[user:pass@]host:port`. `Debug`/`Display` omit the credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyEndpoint {
    host: String,
    port: u16,
    credentials: Option<Credentials>,
}

impl fmt::Debug for ProxyEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyEndpoint")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("auth", &self.credentials.is_some())
            .finish()
    }
}

impl fmt::Display for ProxyEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "socks5://[{}]:{}", self.host, self.port)
        } else {
            write!(f, "socks5://{}:{}", self.host, self.port)
        }
    }
}

impl ProxyEndpoint {
    /// Parse `socks5://` or `socks5h://` (both resolve target names at the proxy). Credentials
    /// are optional and percent-decoded; each must be 1..=255 bytes.
    pub fn parse(raw: &str) -> Result<Self, InvalidProxy> {
        const BAD: InvalidProxy =
            InvalidProxy("invalid OUTPACE_PROXY: expected socks5://[user:pass@]host:port");
        let (scheme, rest) = raw.split_once("://").ok_or(BAD)?;
        if !scheme.eq_ignore_ascii_case("socks5") && !scheme.eq_ignore_ascii_case("socks5h") {
            return Err(BAD);
        }
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.contains(['/', '?', '#']) {
            return Err(BAD);
        }
        let (userinfo, hostport) = match rest.rsplit_once('@') {
            Some((userinfo, hostport)) => (Some(userinfo), hostport),
            None => (None, rest),
        };
        let credentials = match userinfo {
            None => None,
            Some(userinfo) => {
                let (user, pass) = userinfo.split_once(':').ok_or(BAD)?;
                let username = percent_decode(user).ok_or(BAD)?;
                let password = percent_decode(pass).ok_or(BAD)?;
                if username.is_empty()
                    || username.len() > 255
                    || password.is_empty()
                    || password.len() > 255
                {
                    return Err(BAD);
                }
                Some(Credentials { username, password })
            }
        };
        let (host, port) = if let Some(bracketed) = hostport.strip_prefix('[') {
            let (host, port) = bracketed.split_once("]:").ok_or(BAD)?;
            host.parse::<Ipv6Addr>().map_err(|_| BAD)?;
            (host, port)
        } else {
            hostport.rsplit_once(':').ok_or(BAD)?
        };
        if host.is_empty() || host.len() > 255 || host.contains(':') && !hostport.starts_with('[') {
            return Err(BAD);
        }
        let port: u16 = port.parse().map_err(|_| BAD)?;
        if port == 0 {
            return Err(BAD);
        }
        Ok(Self {
            host: host.to_owned(),
            port,
            credentials,
        })
    }

    pub fn has_credentials(&self) -> bool {
        self.credentials.is_some()
    }

    /// Negotiate `CONNECT` to `target`, bounded by `timeout`.
    pub async fn connect(
        &self,
        target: TargetAddr<'_>,
        timeout: Duration,
    ) -> Result<TcpStream, Socks5Error> {
        socks5::connect(
            &self.host,
            self.port,
            self.credentials.as_ref(),
            target,
            timeout,
        )
        .await
    }

    async fn udp_associate(&self) -> Result<(TcpStream, SocketAddr), Socks5Error> {
        socks5::udp_associate(
            &self.host,
            self.port,
            self.credentials.as_ref(),
            HANDSHAKE_TIMEOUT,
        )
        .await
    }
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = raw.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Operator proxy settings. `Debug` reports only whether an endpoint is set.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProxySettings {
    pub mode: ProxyMode,
    pub endpoint: Option<ProxyEndpoint>,
}

impl fmt::Debug for ProxySettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxySettings")
            .field("mode", &self.mode)
            .field("endpoint_set", &self.endpoint.is_some())
            .finish()
    }
}

impl ProxySettings {
    /// Build from `OUTPACE_PROXY` and `OUTPACE_PROXY_MODE` values. The mode defaults to `off`.
    pub fn from_values(url: Option<&str>, mode: Option<&str>) -> Result<Self, InvalidProxy> {
        let endpoint = url
            .filter(|url| !url.is_empty())
            .map(ProxyEndpoint::parse)
            .transpose()?;
        let mode = mode
            .filter(|mode| !mode.is_empty())
            .map(str::parse)
            .transpose()?
            .unwrap_or_default();
        let settings = Self { mode, endpoint };
        settings.validate()?;
        Ok(settings)
    }

    pub fn validate(&self) -> Result<(), InvalidProxy> {
        if self.mode != ProxyMode::Off && self.endpoint.is_none() {
            return Err(InvalidProxy(
                "OUTPACE_PROXY_MODE=on or fallback requires OUTPACE_PROXY",
            ));
        }
        Ok(())
    }

    /// `on`: the proxy is used from the start, so inbound serving can never be reached.
    pub fn always_proxied(&self) -> bool {
        self.mode == ProxyMode::On
    }
}

struct Installed {
    endpoint: ProxyEndpoint,
    mode: ProxyMode,
    engaged: AtomicBool,
    udp_unsupported: AtomicBool,
    dht_skip_logged: AtomicBool,
    announce_skip_logged: AtomicBool,
    failure_logged: AtomicBool,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// Install the process-wide proxy once, before any network activity. `off` installs nothing,
/// so the direct path stays exactly as it was.
pub fn install(settings: &ProxySettings) {
    let Some(endpoint) = settings.endpoint.clone() else {
        return;
    };
    if settings.mode == ProxyMode::Off {
        crate::alog!("[proxy] OUTPACE_PROXY is set but OUTPACE_PROXY_MODE=off; not used");
        return;
    }
    let mode = settings.mode;
    let auth = endpoint.has_credentials();
    let shown = endpoint.to_string();
    let installed = Installed {
        endpoint,
        mode,
        engaged: AtomicBool::new(false),
        udp_unsupported: AtomicBool::new(false),
        dht_skip_logged: AtomicBool::new(false),
        announce_skip_logged: AtomicBool::new(false),
        failure_logged: AtomicBool::new(false),
    };
    if INSTALLED.set(installed).is_err() {
        crate::alog!("[proxy] already installed; ignoring a second configuration");
        return;
    }
    crate::alog!(
        "[proxy] mode={} endpoint={shown} auth={}",
        mode.as_str(),
        if auth { "yes" } else { "no" }
    );
    match mode {
        ProxyMode::On => crate::alog!(
            "[proxy] peers and catalog via CONNECT, UDP trackers via UDP ASSOCIATE; DHT, \
             inbound peers, port mapping and self-announce are off"
        ),
        ProxyMode::Fallback => crate::alog!(
            "[proxy] direct until a live start finds no peers, then proxied engine-wide"
        ),
        ProxyMode::Off => {}
    }
}

fn installed() -> Option<&'static Installed> {
    INSTALLED.get()
}

fn active_installed() -> Option<&'static Installed> {
    installed().filter(|i| i.mode == ProxyMode::On || i.engaged.load(Ordering::Acquire))
}

/// The proxy new outbound connections use right now, or `None` for direct.
pub fn active() -> Option<&'static ProxyEndpoint> {
    active_installed().map(|i| &i.endpoint)
}

/// Configured mode (`off` when nothing is installed).
pub fn mode() -> ProxyMode {
    installed().map_or(ProxyMode::Off, |i| i.mode)
}

/// Path that new outbound connections take: `"direct"` or `"proxy"`.
pub fn path() -> &'static str {
    if active().is_some() {
        "proxy"
    } else {
        "direct"
    }
}

/// Whether UDP trackers still run (directly, or through a relay the proxy granted).
pub fn udp_trackers_enabled() -> bool {
    active_installed().is_none_or(|i| !i.udp_unsupported.load(Ordering::Acquire))
}

/// In `fallback` mode, switch every later connection to the proxy. Returns `true` only for
/// the call that performed the switch, so the caller can retry its start once.
pub fn engage_fallback() -> bool {
    let Some(i) = installed().filter(|i| i.mode == ProxyMode::Fallback) else {
        return false;
    };
    let switched = !i.engaged.swap(true, Ordering::AcqRel);
    if switched {
        crate::alog!(
            "[proxy] fallback engaged: a live start found no peers directly; new connections \
             now use the proxy, DHT and self-announce are off"
        );
    }
    switched
}

/// DHT has no proxied transport; report whether to skip it (logging the first skip).
pub(crate) fn skip_dht() -> bool {
    let Some(i) = active_installed() else {
        return false;
    };
    if !i.dht_skip_logged.swap(true, Ordering::AcqRel) {
        crate::alog!("[proxy] DHT disabled while the proxy is active");
    }
    true
}

/// Peers cannot reach us through the proxy, so do not announce a listen port.
pub fn skip_self_announce() -> bool {
    let Some(i) = active_installed() else {
        return false;
    };
    if !i.announce_skip_logged.swap(true, Ordering::AcqRel) {
        crate::alog!("[proxy] seeder self-announce disabled while the proxy is active");
    }
    true
}

fn note_result(i: &Installed, result: &Result<TcpStream, Socks5Error>) {
    match result {
        Ok(_) => {
            i.failure_logged.store(false, Ordering::Release);
        }
        // Proxy-level faults affect every connection; log once per outage. Per-target refusals
        // (reply codes, timeouts to dead peers) are normal swarm churn and stay quiet.
        Err(
            error @ (Socks5Error::ProxyConnect
            | Socks5Error::AuthRejected
            | Socks5Error::NoAcceptableMethod
            | Socks5Error::BadVersion),
        ) => {
            if !i.failure_logged.swap(true, Ordering::AcqRel) {
                crate::alog!("[proxy] proxy unusable: {error}");
            }
        }
        Err(_) => {}
    }
}

async fn proxied(
    i: &Installed,
    target: TargetAddr<'_>,
    timeout: Duration,
) -> Result<TcpStream, Socks5Error> {
    let result = i.endpoint.connect(target, timeout).await;
    note_result(i, &result);
    result
}

/// Dial a peer: directly (the unchanged `ace_peer` path) or through the active proxy. A
/// proxied stream is a plain `TcpStream` after the handshake, so the session is identical.
pub async fn connect_peer(endpoint: SocketAddrV4) -> ace_peer::Result<PeerSession<TcpStream>> {
    let Some(i) = active_installed() else {
        return ace_peer::session::connect(&endpoint.to_string()).await;
    };
    let stream = proxied(
        i,
        TargetAddr::Ip(SocketAddr::V4(endpoint)),
        ace_peer::session::DEFAULT_PEER_TIMEOUT,
    )
    .await
    .map_err(|error| ace_peer::PeerError::Io(error.into()))?;
    Ok(PeerSession::new(stream))
}

/// Dial `host:port` for an HTTP request. Through the proxy a host name is resolved remotely.
pub(crate) async fn connect_host(host: &str, port: u16) -> std::io::Result<TcpStream> {
    let Some(i) = active_installed() else {
        return TcpStream::connect((host, port)).await;
    };
    let target = match host.parse::<IpAddr>() {
        Ok(ip) => TargetAddr::Ip(SocketAddr::new(ip, port)),
        Err(_) => TargetAddr::Domain(host, port),
    };
    Ok(proxied(i, target, HANDSHAKE_TIMEOUT).await?)
}

/// Announce to a UDP tracker through the active proxy's UDP relay, advertising port 0.
/// Returns `None` when no proxy is active (the caller announces directly). After the proxy
/// refuses `UDP ASSOCIATE` once, every later call returns an empty list without traffic.
pub(crate) async fn tracker_announce(
    tracker: SocketAddrV4,
    infohash: &[u8; 20],
    peer_id: &[u8; 20],
    num_want: i32,
    transfer: TransferState,
    event: AnnounceEvent,
) -> Option<Vec<SocketAddrV4>> {
    let i = active_installed()?;
    if i.udp_unsupported.load(Ordering::Acquire) {
        return Some(Vec::new());
    }
    let result = relay_announce(
        &i.endpoint,
        tracker,
        infohash,
        peer_id,
        num_want,
        transfer,
        event,
    )
    .await;
    match result {
        Ok(peers) => Some(peers),
        Err(RelayError::Socks(Socks5Error::Reply(socks5::REPLY_COMMAND_NOT_SUPPORTED))) => {
            if !i.udp_unsupported.swap(true, Ordering::AcqRel) {
                crate::alog!("[proxy] proxy does not support UDP ASSOCIATE; UDP trackers disabled");
            }
            Some(Vec::new())
        }
        Err(RelayError::Socks(error)) => {
            if !i.failure_logged.swap(true, Ordering::AcqRel) {
                crate::alog!("[proxy] UDP relay unavailable: {error}");
            }
            Some(Vec::new())
        }
        Err(RelayError::Tracker) => Some(Vec::new()),
    }
}

#[derive(Debug)]
enum RelayError {
    Socks(Socks5Error),
    Tracker,
}

impl From<Socks5Error> for RelayError {
    fn from(error: Socks5Error) -> Self {
        Self::Socks(error)
    }
}

/// One BEP-15 connect + announce through a fresh SOCKS5 UDP association.
async fn relay_announce(
    proxy: &ProxyEndpoint,
    tracker: SocketAddrV4,
    infohash: &[u8; 20],
    peer_id: &[u8; 20],
    num_want: i32,
    transfer: TransferState,
    event: AnnounceEvent,
) -> Result<Vec<SocketAddrV4>, RelayError> {
    // The association lives as long as this control connection.
    let (_control, relay) = proxy.udp_associate().await?;
    tokio::time::timeout(UDP_EXCHANGE_TIMEOUT, async {
        let bind = match relay {
            SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        };
        let sock = UdpSocket::bind(bind)
            .await
            .map_err(|_| RelayError::Tracker)?;
        sock.connect(relay).await.map_err(|_| RelayError::Tracker)?;
        let target = TargetAddr::Ip(SocketAddr::V4(tracker));
        let mut out = Vec::with_capacity(10 + 98);
        let mut buf = [0u8; 4096];

        let ctxid: u32 = rand::random();
        socks5::encode_udp_datagram(target, &build_connect_request(ctxid), &mut out)?;
        sock.send(&out).await.map_err(|_| RelayError::Tracker)?;
        let reply = recv_from_tracker(&sock, tracker, &mut buf).await?;
        let connection_id =
            parse_connect_response(reply, ctxid).map_err(|_| RelayError::Tracker)?;

        let atxid: u32 = rand::random();
        let request = build_announce_request(
            connection_id,
            atxid,
            infohash,
            peer_id,
            0,
            num_want,
            &transfer,
            event,
        );
        out.clear();
        socks5::encode_udp_datagram(target, &request, &mut out)?;
        sock.send(&out).await.map_err(|_| RelayError::Tracker)?;
        let reply = recv_from_tracker(&sock, tracker, &mut buf).await?;
        let (_interval, peers) =
            parse_announce_response(reply, atxid).map_err(|_| RelayError::Tracker)?;
        Ok(peers)
    })
    .await
    .map_err(|_| RelayError::Tracker)?
}

async fn recv_from_tracker<'a>(
    sock: &UdpSocket,
    tracker: SocketAddrV4,
    buf: &'a mut [u8; 4096],
) -> Result<&'a [u8], RelayError> {
    let mut len = None;
    for _ in 0..MAX_STRAY_DATAGRAMS {
        let n = sock.recv(buf).await.map_err(|_| RelayError::Tracker)?;
        if let Ok((source, _)) = socks5::decode_udp_datagram(&buf[..n]) {
            if source == SocketAddr::V4(tracker) {
                len = Some(n);
                break;
            }
        }
    }
    let n = len.ok_or(RelayError::Tracker)?;
    let (_, payload) = socks5::decode_udp_datagram(&buf[..n])?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socks5::tests::{serve_one, FakeProxy};
    use ace_tracker::codec::{build_announce_response, build_connect_response};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const PLACEHOLDER: [u8; 20] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef, 0x01, 0x23, 0x45, 0x67,
    ];

    #[test]
    fn parses_proxy_urls() {
        let plain = ProxyEndpoint::parse("socks5://127.0.0.1:1080").unwrap();
        assert_eq!((plain.host.as_str(), plain.port), ("127.0.0.1", 1080));
        assert!(!plain.has_credentials());

        let auth =
            ProxyEndpoint::parse("SOCKS5H://us%40er:p%3Ass@proxy.example.invalid:9050/").unwrap();
        assert_eq!(auth.host, "proxy.example.invalid");
        let creds = auth.credentials.as_ref().unwrap();
        assert_eq!(
            (creds.username.as_str(), creds.password.as_str()),
            ("us@er", "p:ss")
        );

        let v6 = ProxyEndpoint::parse("socks5://[::1]:1080").unwrap();
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.to_string(), "socks5://[::1]:1080");

        for bad in [
            "",
            "http://127.0.0.1:1080",
            "socks4://127.0.0.1:1080",
            "socks5://127.0.0.1",
            "socks5://127.0.0.1:0",
            "socks5://127.0.0.1:99999",
            "socks5://:1080",
            "socks5://user@127.0.0.1:1080",
            "socks5://user:@127.0.0.1:1080",
            "socks5://u:p%zz@127.0.0.1:1080",
            "socks5://127.0.0.1:1080/path",
            "socks5://127.0.0.1:1080?x=1",
            "socks5://::1:1080",
            "socks5://[nothost]:1080",
        ] {
            assert!(ProxyEndpoint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn errors_and_debug_never_echo_credentials() {
        let error = ProxyEndpoint::parse("socks5://user:hunter2@host").unwrap_err();
        assert!(!error.to_string().contains("hunter2"));
        let endpoint = ProxyEndpoint::parse("socks5://user:hunter2@host:1080").unwrap();
        let settings = ProxySettings {
            mode: ProxyMode::On,
            endpoint: Some(endpoint.clone()),
        };
        for text in [
            format!("{endpoint:?}"),
            endpoint.to_string(),
            format!("{settings:?}"),
        ] {
            assert!(
                !text.contains("hunter2") && !text.contains("user"),
                "{text}"
            );
        }
    }

    #[test]
    fn settings_require_an_endpoint_unless_off() {
        assert_eq!(
            ProxySettings::from_values(None, None).unwrap(),
            ProxySettings::default()
        );
        let set_but_off = ProxySettings::from_values(Some("socks5://h:1"), None).unwrap();
        assert_eq!(set_but_off.mode, ProxyMode::Off);
        assert!(ProxySettings::from_values(None, Some("on")).is_err());
        assert!(ProxySettings::from_values(None, Some("fallback")).is_err());
        assert!(ProxySettings::from_values(Some("socks5://h:1"), Some("maybe")).is_err());
        let on = ProxySettings::from_values(Some("socks5://h:1"), Some("on")).unwrap();
        assert!(on.always_proxied());
        let fallback = ProxySettings::from_values(Some("socks5://h:1"), Some("fallback")).unwrap();
        assert!(!fallback.always_proxied());
    }

    #[test]
    fn nothing_installed_means_direct() {
        // Tests never install a process-wide proxy, so every helper reports the direct path.
        assert!(active().is_none());
        assert_eq!(path(), "direct");
        assert!(udp_trackers_enabled());
        assert!(!skip_dht());
        assert!(!skip_self_announce());
        assert!(!engage_fallback());
    }

    #[tokio::test]
    async fn udp_tracker_announce_round_trips_through_a_fake_relay() {
        // Relay socket stands in for both the proxy's UDP relay and the tracker behind it.
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay.local_addr().unwrap();
        let tracker: SocketAddrV4 = "198.51.100.7:6969".parse().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_one(
            listener,
            FakeProxy {
                require_auth: true,
                reply: 0,
                relay: Some(relay_addr),
            },
        ));
        let peers: Vec<SocketAddrV4> = vec!["203.0.113.1:8621".parse().unwrap()];
        let served = peers.clone();
        let relay_task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let wrap = |payload: &[u8]| {
                let mut out = Vec::new();
                socks5::encode_udp_datagram(
                    TargetAddr::Ip(SocketAddr::V4(tracker)),
                    payload,
                    &mut out,
                )
                .unwrap();
                out
            };
            // Connect step: a stray datagram from another source must be ignored.
            let (n, client) = relay.recv_from(&mut buf).await.unwrap();
            let (dest, payload) = socks5::decode_udp_datagram(&buf[..n]).unwrap();
            assert_eq!(dest, SocketAddr::V4(tracker));
            let txid = u32::from_be_bytes(payload[12..16].try_into().unwrap());
            let mut stray = Vec::new();
            socks5::encode_udp_datagram(
                TargetAddr::Ip("192.0.2.1:1".parse().unwrap()),
                &build_connect_response(txid, 1),
                &mut stray,
            )
            .unwrap();
            relay.send_to(&stray, client).await.unwrap();
            relay
                .send_to(&wrap(&build_connect_response(txid, 42)), client)
                .await
                .unwrap();
            // Announce step: port must be 0 because the proxy cannot carry inbound peers.
            let (n, client) = relay.recv_from(&mut buf).await.unwrap();
            let (_, payload) = socks5::decode_udp_datagram(&buf[..n]).unwrap();
            let request = ace_tracker::codec::parse_announce_request(payload).unwrap();
            assert_eq!(request.connection_id, 42);
            assert_eq!(request.port, 0);
            assert_eq!(request.infohash, PLACEHOLDER);
            relay
                .send_to(
                    &wrap(&build_announce_response(request.txid, 60, 0, 1, &served)),
                    client,
                )
                .await
                .unwrap();
        });
        let endpoint =
            ProxyEndpoint::parse(&format!("socks5://user:secret-pass@127.0.0.1:{proxy_port}"))
                .unwrap();
        let got = relay_announce(
            &endpoint,
            tracker,
            &PLACEHOLDER,
            &[7u8; 20],
            50,
            TransferState {
                downloaded: 0,
                left: 0,
                uploaded: 0,
            },
            AnnounceEvent::Started,
        )
        .await
        .unwrap();
        assert_eq!(got, peers);
        relay_task.await.unwrap();
    }

    #[tokio::test]
    async fn udp_associate_refusal_is_reported_as_command_not_supported() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_one(
            listener,
            FakeProxy {
                require_auth: false,
                reply: socks5::REPLY_COMMAND_NOT_SUPPORTED,
                relay: None,
            },
        ));
        let endpoint = ProxyEndpoint::parse(&format!("socks5://127.0.0.1:{port}")).unwrap();
        let error = relay_announce(
            &endpoint,
            "198.51.100.7:6969".parse().unwrap(),
            &PLACEHOLDER,
            &[7u8; 20],
            50,
            TransferState {
                downloaded: 0,
                left: 0,
                uploaded: 0,
            },
            AnnounceEvent::Started,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            RelayError::Socks(Socks5Error::Reply(socks5::REPLY_COMMAND_NOT_SUPPORTED))
        ));
    }

    #[tokio::test]
    async fn proxied_peer_stream_is_a_plain_tcp_stream() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = target.accept().await.unwrap();
            socket.write_all(b"peer").await.unwrap();
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_one(
            listener,
            FakeProxy {
                require_auth: false,
                reply: 0,
                relay: None,
            },
        ));
        let endpoint = ProxyEndpoint::parse(&format!("socks5://127.0.0.1:{port}")).unwrap();
        let mut stream = endpoint
            .connect(TargetAddr::Ip(target_addr), Duration::from_secs(5))
            .await
            .unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"peer");
    }
}
