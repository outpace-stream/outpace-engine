//! Register a live broadcast transport with the AceStream catalog, as engine 3.2.11 does when
//! asked for a broadcast's content id (protocol note 76).
//!
//! 1. `GET /checktorrent` with the SHA-1 of the wrapped transport. A signed reply carrying an
//!    `<id>` means the transport is already registered.
//! 2. Otherwise `POST /puttorrent` with the raw wrapped transport as the body. The signed reply's
//!    `<id>` is the catalog content id.
//!
//! Requests are signed like `/gettorrent` (sorted `k=v` pairs joined by `#`, then the request
//! key); `puttorrent` also signs `#` plus the body. Replies are authenticated as in note 62.
//! The catalog assigns the id: it is not derived from the transport.

use crate::engine_profile;
use crate::resolve::{
    authenticated_catalog_document, catalog_field, catalog_response_text, hex20,
    http_response_body, ResolveError, CATALOG_RESPONSE_LIMIT, CATALOG_SIGNING_SECRET,
};
use ace_wire::infohash::{transport_file_hash, try_infohash_of_transport};
use ace_wire::transport::decode_transport;
use base64ct::{Base64, Encoding};
use rand::RngExt;
use sha1::{Digest, Sha1};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The catalog pool engine 3.2.11 used for `checktorrent`/`puttorrent`, plain HTTP on port 80.
pub const REGISTRATION_HOSTS: &[&str] = &[
    "s1.torrentstream.net",
    "s2.torrentstream.net",
    "s3.torrentstream.net",
    "s1.torrentstream.org",
    "s2.torrentstream.org",
    "s3.torrentstream.org",
    "s1.torrentstream.info",
    "s2.torrentstream.info",
    "s3.torrentstream.info",
];
const REGISTRATION_PORT: u16 = 80;
const HOST_TIMEOUT: Duration = Duration::from_secs(15);

/// The client version and platform the requests claim and sign.
#[derive(Clone, Copy)]
struct Client<'a> {
    version_name: &'a str,
    version_code: i64,
    platform: &'a str,
}

fn current_client() -> Client<'static> {
    let (version_name, version_code) = engine_profile::version();
    Client {
        version_name,
        version_code,
        platform: engine_profile::platform_name(),
    }
}

/// Register `transport` (wrapped `AceStreamTransport` bytes of a live broadcast) and return
/// its catalog content id, trying each catalog host in turn.
pub async fn register_live_transport(transport: &[u8]) -> Result<[u8; 20], ResolveError> {
    let hosts: Vec<(&str, u16)> = REGISTRATION_HOSTS
        .iter()
        .map(|host| (*host, REGISTRATION_PORT))
        .collect();
    register_with_hosts(transport, &hosts).await
}

async fn register_with_hosts(
    transport: &[u8],
    hosts: &[(&str, u16)],
) -> Result<[u8; 20], ResolveError> {
    let decoded =
        decode_transport(transport).map_err(|_| ResolveError::Transport("bad transport"))?;
    if !decoded.is_live {
        return Err(ResolveError::Transport(
            "only live transports are registered",
        ));
    }
    let infohash = try_infohash_of_transport(transport)
        .map_err(|_| ResolveError::Transport("bad transport infohash"))?;
    let mut last = ResolveError::CatalogNetwork("catalog registration failed");
    for &(host, port) in hosts {
        match tokio::time::timeout(HOST_TIMEOUT, register_at(host, port, transport, infohash)).await
        {
            Ok(Ok(id)) => return Ok(id),
            Ok(Err(e)) => last = e,
            Err(_) => last = ResolveError::CatalogNetwork("catalog timeout"),
        }
    }
    Err(last)
}

async fn register_at(
    host: &str,
    port: u16,
    transport: &[u8],
    infohash: [u8; 20],
) -> Result<[u8; 20], ResolveError> {
    let checksum = hex::encode(transport_file_hash(transport));
    let check = check_params(&checksum);
    if let Some(id) = call(host, port, "/checktorrent", &check, None, infohash).await? {
        return Ok(id);
    }
    call(
        host,
        port,
        "/puttorrent",
        &put_params(&infohash),
        Some(transport),
        infohash,
    )
    .await?
    .ok_or(ResolveError::Catalog("catalog registration returned no id"))
}

/// `a`, `d` and `z` are the affiliate, developer and zone ids; the engine sends zero.
fn check_params(checksum: &str) -> Vec<(&'static str, String)> {
    vec![
        ("a", "0".into()),
        ("checksum", checksum.into()),
        ("d", "0".into()),
        ("z", "0".into()),
    ]
}

fn put_params(infohash: &[u8; 20]) -> Vec<(&'static str, String)> {
    vec![
        ("a", "0".into()),
        ("d", "0".into()),
        ("infohash", hex::encode(infohash)),
        // Observed as 1 for a live RSA-signed broadcast transport.
        ("protected", "1".into()),
        ("z", "0".into()),
    ]
}

/// The path and query for one signed catalog request: client fields and `params` in sorted
/// key order, then `_s`.
fn signed_path(
    path: &str,
    client: Client<'_>,
    request_random: u64,
    params: &[(&'static str, String)],
    body: Option<&[u8]>,
) -> String {
    let mut pairs: Vec<(&str, String)> = vec![
        ("_n", client.version_name.into()),
        ("_p", client.platform.into()),
        ("_r", request_random.to_string()),
        ("_v", client.version_code.to_string()),
    ];
    pairs.extend(params.iter().cloned());
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let signed = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("#");
    let mut hasher = Sha1::new();
    hasher.update(signed.as_bytes());
    if let Some(body) = body {
        hasher.update(b"#");
        hasher.update(body);
    }
    hasher.update(CATALOG_SIGNING_SECRET);
    let signature = hex::encode(hasher.finalize());

    let mut url =
        reqwest::Url::parse("http://catalog.example.invalid/").expect("constant catalog URL");
    url.set_path(path);
    {
        let mut query = url.query_pairs_mut();
        for (k, v) in &pairs {
            query.append_pair(k, v);
        }
        query.append_pair("_s", &signature);
    }
    format!("{}?{}", url.path(), url.query().expect("catalog query"))
}

async fn call(
    host: &str,
    port: u16,
    path: &str,
    params: &[(&'static str, String)],
    body: Option<&[u8]>,
    infohash: [u8; 20],
) -> Result<Option<[u8; 20]>, ResolveError> {
    let request_random = rand::rng().random_range(1..=i64::MAX as u64);
    let path_and_query = signed_path(path, current_client(), request_random, params, body);
    let host_header = if port == 80 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    let user_agent = match engine_profile::identity() {
        Some(identity) => identity.random_catalog_user_agent(),
        None => "outpace/0.1".to_string(),
    };
    let head = match body {
        Some(body) => {
            engine_profile::engine_post_head(&path_and_query, &host_header, &user_agent, body.len())
        }
        None => engine_profile::engine_request_head(&path_and_query, &host_header, &user_agent),
    };

    let mut stream = crate::proxy::connect_host(host, port)
        .await
        .map_err(|_| ResolveError::CatalogNetwork("catalog connect failed"))?;
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|_| ResolveError::CatalogNetwork("catalog write failed"))?;
    if let Some(body) = body {
        stream
            .write_all(body)
            .await
            .map_err(|_| ResolveError::CatalogNetwork("catalog write failed"))?;
    }
    let mut response = Vec::new();
    (&mut stream)
        .take(CATALOG_RESPONSE_LIMIT as u64 + 1)
        .read_to_end(&mut response)
        .await
        .map_err(|_| ResolveError::CatalogNetwork("catalog read failed"))?;
    if response.len() > CATALOG_RESPONSE_LIMIT {
        return Err(ResolveError::Catalog("catalog response too large"));
    }
    parse_registration(&http_response_body(&response)?, request_random, infohash)
}

/// The content id in an authenticated registration reply, `None` when the catalog has no
/// registration for the transport. A reply naming a different swarm is rejected.
fn parse_registration(
    body: &[u8],
    request_random: u64,
    infohash: [u8; 20],
) -> Result<Option<[u8; 20]>, ResolveError> {
    let xml = catalog_response_text(body)?;
    let document = authenticated_catalog_document(xml, request_random)?;
    let root = document.root_element();
    let optional = |tag| match catalog_field(root, tag) {
        Err(ResolveError::Catalog("empty catalog tag")) => Ok(None),
        other => other,
    };
    if let Some(value) = optional("infohash")? {
        let reported = Base64::decode_vec(value.trim())
            .map_err(|_| ResolveError::Catalog("bad catalog infohash"))?;
        if reported.as_slice() != infohash.as_slice() {
            return Err(ResolveError::Catalog(
                "catalog registration infohash mismatch",
            ));
        }
    }
    optional("id")?
        .map(|id| hex20(id.trim()).map_err(|_| ResolveError::Catalog("bad catalog content id")))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::CATALOG_RESPONSE_KEY;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    const ENGINE: Client<'static> = Client {
        version_name: "3.2.11",
        version_code: 3021100,
        platform: "linux",
    };
    const PLACEHOLDER: &str = "0123456789abcdef0123456789abcdef01234567";

    fn fixture() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/vectors/transport/synthetic-live-512k.bin"
        ))
        .unwrap()
    }

    fn query(path: &str) -> Vec<(String, String)> {
        let url = reqwest::Url::parse(&format!("http://h{path}")).unwrap();
        url.query_pairs().into_owned().collect()
    }

    fn signed(unsigned: &str) -> String {
        let mut hasher = Sha1::new();
        hasher.update(unsigned.as_bytes());
        hasher.update(CATALOG_RESPONSE_KEY);
        let sig = hex::encode(hasher.finalize());
        unsigned.replacen("<response", &format!("<response sig=\"{sig}\""), 1)
    }

    // Requests engine 3.2.11 sent for the synthetic fixture, captured offline against a mock
    // catalog (note 76). The nonce and signature are the engine's own.
    #[test]
    fn checktorrent_matches_engine_capture() {
        let transport = fixture();
        let checksum = hex::encode(transport_file_hash(&transport));
        let path = signed_path(
            "/checktorrent",
            ENGINE,
            7076687882475883716,
            &check_params(&checksum),
            None,
        );
        let q = query(&path);
        let keys: Vec<_> = q.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["_n", "_p", "_r", "_v", "a", "checksum", "d", "z", "_s"]
        );
        assert_eq!(q[8].1, "dedeceffc1902ac983948baa1fc8423fb0ba8c4c");
        assert!(path.starts_with("/checktorrent?_n=3.2.11&_p=linux&_r=7076687882475883716&"));
    }

    #[test]
    fn puttorrent_matches_engine_capture() {
        let transport = fixture();
        let infohash = try_infohash_of_transport(&transport).unwrap();
        let path = signed_path(
            "/puttorrent",
            ENGINE,
            2458234888495302710,
            &put_params(&infohash),
            Some(&transport),
        );
        let q = query(&path);
        let keys: Vec<_> = q.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "_n",
                "_p",
                "_r",
                "_v",
                "a",
                "d",
                "infohash",
                "protected",
                "z",
                "_s"
            ]
        );
        assert_eq!(q[9].1, "9e6723acd51b0cab316bd870799c1e81c71b609f");
    }

    #[test]
    fn parses_production_shaped_replies() {
        let ih = try_infohash_of_transport(&fixture()).unwrap();
        // Not registered: the catalog answers with an empty, self-closing root.
        let absent = signed("<?xml version=\"1.0\"?>\n<response r=\"5\"/>\n");
        assert_eq!(parse_registration(absent.as_bytes(), 5, ih), Ok(None));
        // Registered: id plus the Base64 swarm infohash.
        let b64 = Base64::encode_string(&ih);
        let present = signed(&format!(
            "<?xml version=\"1.0\"?>\n<response r=\"6\"><id>{PLACEHOLDER}</id><infohash>{b64}</infohash></response>\n"
        ));
        assert_eq!(
            parse_registration(present.as_bytes(), 6, ih),
            Ok(Some(hex20(PLACEHOLDER).unwrap()))
        );
        // Wrong nonce, wrong swarm, bad id and a forged signature all fail closed.
        assert!(parse_registration(present.as_bytes(), 7, ih).is_err());
        assert_eq!(
            parse_registration(present.as_bytes(), 6, [0; 20]),
            Err(ResolveError::Catalog(
                "catalog registration infohash mismatch"
            ))
        );
        let bad_id = signed("<?xml version=\"1.0\"?>\n<response r=\"8\"><id>xyz</id></response>");
        assert_eq!(
            parse_registration(bad_id.as_bytes(), 8, ih),
            Err(ResolveError::Catalog("bad catalog content id"))
        );
        let forged = present.replace(PLACEHOLDER, "1123456789abcdef0123456789abcdef01234567");
        assert!(parse_registration(forged.as_bytes(), 6, ih).is_err());
    }

    /// A one-host mock catalog: answers each request with `reply(path, body)` signed for the
    /// request's nonce, and records what it received.
    async fn mock_catalog(
        reply: impl Fn(&str, &[u8]) -> String + Send + Sync + 'static,
    ) -> (u16, Arc<Mutex<Vec<(String, Vec<u8>)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body) = loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .map_or(0, |v| v.parse::<usize>().unwrap());
                        while buf.len() < end + 4 + len {
                            let n = sock.read(&mut chunk).await.unwrap();
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        break (head, buf[end + 4..end + 4 + len].to_vec());
                    }
                };
                let path = head.split(' ').nth(1).unwrap().to_string();
                let r = query(&path).into_iter().find(|(k, _)| k == "_r").unwrap().1;
                let xml = signed(&reply(&path, &body).replace("NONCE", &r));
                log.lock().unwrap().push((path, body));
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}",
                    xml.len()
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
            }
        });
        (port, seen)
    }

    #[tokio::test]
    async fn registers_after_a_negative_check() {
        let transport = fixture();
        let (port, seen) = mock_catalog(|path, _| {
            if path.starts_with("/checktorrent") {
                "<?xml version=\"1.0\"?>\n<response r=\"NONCE\"/>\n".into()
            } else {
                format!("<?xml version=\"1.0\"?>\n<response r=\"NONCE\"><id>{PLACEHOLDER}</id></response>\n")
            }
        })
        .await;
        let id = register_with_hosts(&transport, &[("127.0.0.1", port)])
            .await
            .unwrap();
        assert_eq!(id, hex20(PLACEHOLDER).unwrap());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].0.starts_with("/checktorrent?"));
        assert!(seen[1].0.starts_with("/puttorrent?"));
        assert_eq!(seen[1].1, transport, "puttorrent uploads the raw transport");
    }

    #[tokio::test]
    async fn existing_registration_skips_the_upload() {
        let transport = fixture();
        let (port, seen) = mock_catalog(|_, _| {
            format!("<?xml version=\"1.0\"?>\n<response r=\"NONCE\"><id>{PLACEHOLDER}</id></response>\n")
        })
        .await;
        let id = register_with_hosts(&transport, &[("127.0.0.1", port)])
            .await
            .unwrap();
        assert_eq!(id, hex20(PLACEHOLDER).unwrap());
        assert_eq!(seen.lock().unwrap().len(), 1, "only checktorrent");
    }

    #[tokio::test]
    async fn falls_through_to_the_next_host() {
        let transport = fixture();
        // First host refuses connections; second answers.
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let (port, _) = mock_catalog(|_, _| {
            format!("<?xml version=\"1.0\"?>\n<response r=\"NONCE\"><id>{PLACEHOLDER}</id></response>\n")
        })
        .await;
        let id = register_with_hosts(&transport, &[("127.0.0.1", dead_port), ("127.0.0.1", port)])
            .await
            .unwrap();
        assert_eq!(id, hex20(PLACEHOLDER).unwrap());
    }

    // Live: registers ACE_TRANSPORT_FILE with the production catalog. Only use a transport you
    // have the right to publish; an already-registered one only performs checktorrent.
    //   ACE_TRANSPORT_FILE=/path/to/broadcast.acelive \
    //     cargo test -p ace-swarm live_catalog_registration -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_catalog_registration() {
        let path = std::env::var("ACE_TRANSPORT_FILE").expect("set ACE_TRANSPORT_FILE=path");
        let transport = std::fs::read(path).unwrap();
        let id = register_live_transport(&transport).await.unwrap();
        eprintln!("catalog content id: {}", hex::encode(id));
    }
}
