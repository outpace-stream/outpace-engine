//! `client_list` on `GET /streams` and `GET /streams/{network}/{id}/status` (#473).

use super::*;
use axum::extract::connect_info::ConnectInfo;
use std::net::SocketAddr;

fn from(path: &str, peer: Option<&str>) -> Request<Body> {
    let mut request = Request::get(path).body(Body::empty()).unwrap();
    if let Some(peer) = peer {
        let peer: SocketAddr = peer.parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
    }
    request
}

async fn json_at(app: &Router, path: &str) -> serde_json::Value {
    let response = app.clone().oneshot(from(path, None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    serde_json::from_str(&body_text(response).await).unwrap()
}

/// The `clients` count must equal the list length below the cap.
fn addresses(entry: &serde_json::Value) -> Vec<serde_json::Value> {
    let list = entry["client_list"].as_array().unwrap();
    assert_eq!(entry["clients"], list.len() as u64, "{entry}");
    for client in list {
        assert!(client["since"].as_u64().unwrap() > 0, "{client}");
    }
    list.iter()
        .map(|client| client["address"].clone())
        .collect()
}

#[tokio::test]
async fn direct_ts_clients_are_listed_by_peer_address_and_removed_on_disconnect() {
    let st = fixture_state(0);
    let app = router(st.clone());
    let first = app
        .clone()
        .oneshot(from(
            "/streams/fix/chan.ts",
            Some("[::ffff:192.168.1.20]:50000"),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let second = app
        .clone()
        .oneshot(from("/streams/fix/chan", Some("127.0.0.1:50001")))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    // A router served without connect info still lists the client, without an address.
    let third = app
        .clone()
        .oneshot(from("/streams/fix/chan.ts", None))
        .await
        .unwrap();
    assert_eq!(third.status(), StatusCode::OK);

    let expected = vec![json!("192.168.1.20"), json!("127.0.0.1"), json!(null)];
    let list = json_at(&app, "/streams").await;
    assert_eq!(addresses(&list["streams"][0]), expected);
    let status = json_at(&app, "/streams/fix/chan/status").await;
    assert_eq!(addresses(&status), expected);

    drop(first);
    let status = json_at(&app, "/streams/fix/chan/status").await;
    assert_eq!(
        addresses(&status),
        vec![json!("127.0.0.1"), json!(null)],
        "a dropped body removes exactly its own entry"
    );
    drop((second, third));
    let list = json_at(&app, "/streams").await;
    assert_eq!(
        addresses(&list["streams"][0]),
        Vec::<serde_json::Value>::new()
    );
    st.manager.shutdown().await;
}

#[tokio::test]
async fn client_list_is_capped_while_clients_stays_exact() {
    let st = fixture_state(0);
    let app = router(st.clone());
    let total = crate::session::CLIENT_LIST_CAP + 2;
    let mut bodies = Vec::with_capacity(total);
    for port in 0..total {
        let peer = format!("10.0.0.1:{}", 40000 + port);
        let response = app
            .clone()
            .oneshot(from("/streams/fix/chan.ts", Some(&peer)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        bodies.push(response);
    }
    for value in [
        json_at(&app, "/streams").await["streams"][0].clone(),
        json_at(&app, "/streams/fix/chan/status").await,
    ] {
        assert_eq!(value["clients"], total as u64);
        let list = value["client_list"].as_array().unwrap();
        assert_eq!(list.len(), crate::session::CLIENT_LIST_CAP);
        assert!(list.iter().all(|client| client["address"] == "10.0.0.1"));
    }
    drop(bodies);
    st.manager.shutdown().await;
}

#[tokio::test]
async fn hls_lease_lists_its_opener_once_and_drops_it_on_expiry() {
    let id = "0123456789abcdef0123456789abcdef01234567";
    let state = ace_hls_state(Duration::from_millis(300), 8);
    let manager = state.manager.clone();
    let app = router(state);
    let start = app
        .clone()
        .oneshot(from(
            &format!("/ace/manifest.m3u8?infohash={id}"),
            Some("192.168.1.30:41000"),
        ))
        .await
        .unwrap();
    assert_eq!(start.status(), StatusCode::FOUND);
    let playlist_path = start.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .strip_prefix("http://127.0.0.1:6878")
        .unwrap()
        .to_owned();

    // Playlist and segment fetches, from any address, add no entries.
    let playlist = app
        .clone()
        .oneshot(from(&playlist_path, Some("192.168.1.99:41001")))
        .await
        .unwrap();
    assert_eq!(playlist.status(), StatusCode::OK);
    let expected = vec![json!("192.168.1.30")];
    let list = json_at(&app, "/streams").await;
    assert_eq!(addresses(&list["streams"][0]), expected);
    let status = json_at(&app, &format!("/streams/hlsfix/{id}/status")).await;
    assert_eq!(addresses(&status), expected);

    // Expiry releases the lease's pin, and with it the entry.
    let mut released = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if manager
            .get("hlsfix", id)
            .await
            .is_none_or(|session| session.subscriber_count() == 0)
        {
            released = true;
            break;
        }
    }
    assert!(released, "an expired lease must release its client entry");
    let list = json_at(&app, "/streams").await;
    for stream in list["streams"].as_array().unwrap() {
        assert_eq!(addresses(stream), Vec::<serde_json::Value>::new());
    }
    manager.shutdown().await;
}

#[tokio::test]
async fn compat_playback_lists_the_address_of_the_request_that_streams() {
    let id = "0123456789abcdef0123456789abcdef01234567";
    let st = ace_compat_state(0);
    let app = router(st.clone());
    let direct = app
        .clone()
        .oneshot(from(
            &format!("/ace/getstream?infohash={id}"),
            Some("192.168.1.40:42000"),
        ))
        .await
        .unwrap();
    assert_eq!(direct.status(), StatusCode::OK);
    // A JSON mint opens no subscription; the later playback request is the client.
    let minted = app
        .clone()
        .oneshot(from(
            &format!("/ace/getstream?format=json&infohash={id}"),
            Some("192.168.1.41:42001"),
        ))
        .await
        .unwrap();
    let minted: serde_json::Value = serde_json::from_str(&body_text(minted).await).unwrap();
    let playback_path = minted["response"]["playback_url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://127.0.0.1:6878")
        .unwrap()
        .to_owned();
    let playback = app
        .clone()
        .oneshot(from(&playback_path, Some("192.168.1.42:42002")))
        .await
        .unwrap();
    assert_eq!(playback.status(), StatusCode::OK);

    let list = json_at(&app, "/streams").await;
    assert_eq!(
        addresses(&list["streams"][0]),
        vec![json!("192.168.1.40"), json!("192.168.1.42")]
    );
    drop((direct, playback));
    let list = json_at(&app, "/streams").await;
    assert_eq!(
        addresses(&list["streams"][0]),
        Vec::<serde_json::Value>::new()
    );
    st.manager.shutdown().await;
}

/// The production serve path (`into_make_service_with_connect_info`) supplies the TCP peer.
#[tokio::test]
async fn served_router_records_the_tcp_peer_address() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let st = fixture_state(0);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(st.clone());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /streams/fix/chan.ts HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut head = [0_u8; 12];
    stream.read_exact(&mut head).await.unwrap();
    assert_eq!(&head, b"HTTP/1.1 200");
    let session = st.manager.get("fix", "chan").await.unwrap();
    let snapshot = session.client_snapshot();
    assert_eq!(snapshot.count, 1);
    assert_eq!(
        snapshot.clients[0].address,
        Some("127.0.0.1".parse().unwrap())
    );
    drop(stream);
    server.abort();
    st.manager.shutdown().await;
}
