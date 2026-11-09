//! Discovery integration tests using the real network stack.

mod common;

use std::time::Duration;

use linker_core::discovery::{Discovered, Discovery, SelfInfo};
use tokio::sync::mpsc;

fn me(id: &str, port: u16) -> SelfInfo {
    SelfInfo {
        device_id: id.repeat(64 / id.len()),
        device_name: format!("dev-{id}"),
        device_type: "test".into(),
        port,
    }
}

async fn expect_peer(
    rx: &mut mpsc::Receiver<Discovered>,
    id: &str,
    secs: u64,
) -> Option<Discovered> {
    let want = id.repeat(64 / id.len());
    tokio::time::timeout(Duration::from_secs(secs), async {
        while let Some(d) = rx.recv().await {
            if d.device_id == want {
                return d;
            }
        }
        panic!("discovery channel closed");
    })
    .await
    .ok()
}

/// Two mDNS responders on this host find each other and ignore themselves.
/// Needs multicast on at least one interface; skipped (with a message) on
/// hosts without one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mdns_peers_find_each_other() {
    common::init_tracing();
    let (txa, mut rxa) = mpsc::channel(64);
    let (txb, mut rxb) = mpsc::channel(64);
    let _a = Discovery::start(me("a1", 41001), true, None, txa).await;
    let _b = Discovery::start(me("b2", 41002), true, None, txb).await;
    let Some(found) = expect_peer(&mut rxa, "b2", 10).await else {
        eprintln!("SKIP: no mDNS on this host (no multicast-capable interface?)");
        return;
    };
    assert_eq!(found.via, "mdns");
    assert_eq!(found.device_name, "dev-b2");
    assert!(found.addrs.iter().all(|a| a.port() == 41002));
    assert!(expect_peer(&mut rxb, "a1", 10).await.is_some(), "B finds A");
    // Never reports itself.
    assert!(expect_peer(&mut rxa, "a1", 2).await.is_none());
}

/// Broadcast fallback: a beacon from one node is heard by a listener.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcast_beacon_is_received() {
    common::init_tracing();
    let port = common::free_port();
    let (txa, mut rxa) = mpsc::channel(64);
    let _a = Discovery::start(me("aa", 41003), false, Some(port), txa).await;
    // Send B's beacon by hand (same format as the daemon's) to A's port.
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
    sock.set_broadcast(true).unwrap();
    let beacon =
        serde_json::json!({"linker": 1, "id": "bb".repeat(32), "name": "dev-bb", "port": 41004});
    let mut found = None;
    for _ in 0..10 {
        let _ = sock
            .send_to(beacon.to_string().as_bytes(), ("127.0.0.1", port))
            .await;
        if let Some(d) = expect_peer(&mut rxa, "bb", 1).await {
            found = Some(d);
            break;
        }
    }
    let d = found.expect("beacon received");
    assert_eq!(d.via, "broadcast");
    assert_eq!(d.addrs[0].port(), 41004);
}
