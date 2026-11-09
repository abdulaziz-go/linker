//! Integration tests: two (or three) real daemons in one process, talking
//! QUIC over localhost with a mock platform backend.

mod common;

use std::time::Duration;

use common::*;
use linker_core::config::KvmRole;
use linker_core::identity::Identity;
use linker_core::status::LinkState;
use linker_platform::{
    now_us, CameraEvent, ClipboardChange, ClipboardContent, EncodedFrame, LocalInput, Notification,
};

const T: Duration = Duration::from_secs(15);

type PairRx = tokio::sync::mpsc::UnboundedReceiver<linker_core::pairing::PairEvent>;

/// Wait for the SAS event, skipping `Waiting`.
async fn next_sas(rx: &mut PairRx) -> Result<String, tokio::time::error::Elapsed> {
    tokio::time::timeout(T, async {
        loop {
            match rx.recv().await.expect("events") {
                linker_core::pairing::PairEvent::Sas { code, .. } => return code,
                linker_core::pairing::PairEvent::Waiting => continue,
                other => panic!("unexpected {other:?}"),
            }
        }
    })
    .await
}

struct Pair {
    a: Node,
    b: Node,
}

/// Two pre-paired nodes: A = KVM server + camera sink (Ubuntu-like),
/// B = KVM client + camera source (Mac-like).
fn paired_pair() -> Pair {
    let (ka, kb) = (
        Identity::generate_pem().unwrap(),
        Identity::generate_pem().unwrap(),
    );
    let (ida, idb) = (
        identity(&ka, "ubuntu").device_id,
        identity(&kb, "mac").device_id,
    );
    let (pa, pb) = (free_port(), free_port());
    let a = spawn(NodeSpec {
        name: "ubuntu",
        key_pem: ka,
        port: pa,
        peer_port: pb,
        paired_with: Some((idb, "mac".into())),
        kvm_role: KvmRole::Server,
        camera_role: "sink",
    });
    let b = spawn(NodeSpec {
        name: "mac",
        key_pem: kb,
        port: pb,
        peer_port: pa,
        paired_with: Some((ida, "ubuntu".into())),
        kvm_role: KvmRole::Client,
        camera_role: "source",
    });
    Pair { a, b }
}

async fn connected(p: &Pair) {
    wait_for("both CONNECTED", T, || {
        p.a.handle.status().link == LinkState::Connected
            && p.b.handle.status().link == LinkState::Connected
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handshake_and_heartbeat() {
    init_tracing();
    let p = paired_pair();
    connected(&p).await;
    let sa = p.a.handle.status();
    assert_eq!(sa.peer.as_ref().unwrap().device_name, "mac");
    assert_eq!(sa.peer.as_ref().unwrap().device_id, p.b.id);
    // Heartbeat: a Pong must arrive within one interval (5 s) → RTT measured.
    let link = p.a.handle.0.link().unwrap();
    wait_for("heartbeat pong", Duration::from_secs(8), || {
        link.clock.rtt_us() > 0
    })
    .await;
    // Exactly one link per side (tie-break): both report the same session up.
    assert_eq!(p.b.handle.status().peer.unwrap().device_id, p.a.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnects_after_peer_restart_and_notifies_once() {
    init_tracing();
    let p = paired_pair();
    connected(&p).await;

    // Kill B. A must notice, raise exactly one "not found", keep searching.
    p.b.handle.shutdown();
    wait_for("A searching", Duration::from_secs(25), || {
        p.a.handle.status().link != LinkState::Connected
    })
    .await;
    tokio::time::sleep(Duration::from_secs(5)).await; // grace is 2 s in tests
    let not_found = |n: &Node| {
        n.mock
            .st()
            .shown
            .iter()
            .filter(|n| n.title.contains("not found"))
            .count()
    };
    assert_eq!(not_found(&p.a), 1, "exactly one not-found notification");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(not_found(&p.a), 1, "still exactly one");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_heals_after_restart() {
    init_tracing();
    let (ka, kb) = (
        Identity::generate_pem().unwrap(),
        Identity::generate_pem().unwrap(),
    );
    let (ida, idb) = (identity(&ka, "a").device_id, identity(&kb, "b").device_id);
    let (pa, pb) = (free_port(), free_port());
    let spec_b = |kb: String| NodeSpec {
        name: "b",
        key_pem: kb,
        port: pb,
        peer_port: pa,
        paired_with: Some((ida.clone(), "a".into())),
        kvm_role: KvmRole::Client,
        camera_role: "source",
    };
    let a = spawn(NodeSpec {
        name: "a",
        key_pem: ka,
        port: pa,
        peer_port: pb,
        paired_with: Some((idb.clone(), "b".into())),
        kvm_role: KvmRole::Server,
        camera_role: "sink",
    });
    let b = spawn(spec_b(kb.clone()));
    wait_for("connected", T, || {
        a.handle.status().link == LinkState::Connected
    })
    .await;
    b.handle.shutdown();
    let _ = b.task.await;
    wait_for("A lost link", Duration::from_secs(25), || {
        a.handle.status().link != LinkState::Connected
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let b2 = spawn(spec_b(kb));
    wait_for("reconnected", Duration::from_secs(40), || {
        a.handle.status().link == LinkState::Connected
            && b2.handle.status().link == LinkState::Connected
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpaired_device_is_rejected() {
    init_tracing();
    let p = paired_pair();
    connected(&p).await;
    // A stranger that "thinks" it is paired with A dials A directly.
    let ks = Identity::generate_pem().unwrap();
    let ps = free_port();
    let stranger = spawn(NodeSpec {
        name: "stranger",
        key_pem: ks,
        port: ps,
        peer_port: p.a.port,
        paired_with: Some((p.a.id.clone(), "ubuntu".into())),
        kvm_role: KvmRole::Server,
        camera_role: "sink",
    });
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_ne!(stranger.handle.status().link, LinkState::Connected);
    // A's link to its real peer is unaffected.
    assert_eq!(p.a.handle.status().peer.unwrap().device_id, p.b.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_with_sas_then_connect() {
    init_tracing();
    let (ka, kb) = (
        Identity::generate_pem().unwrap(),
        Identity::generate_pem().unwrap(),
    );
    let (pa, pb) = (free_port(), free_port());
    let a = spawn(NodeSpec {
        name: "a",
        key_pem: ka,
        port: pa,
        peer_port: pb,
        paired_with: None,
        kvm_role: KvmRole::Server,
        camera_role: "sink",
    });
    let b = spawn(NodeSpec {
        name: "b",
        key_pem: kb,
        port: pb,
        peer_port: pa,
        paired_with: None,
        kvm_role: KvmRole::Client,
        camera_role: "source",
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_ne!(
        a.handle.status().link,
        LinkState::Connected,
        "unpaired devices must not link"
    );

    let mut ea = a.handle.pair_begin();
    let mut eb = b.handle.pair_begin();
    let code_a = next_sas(&mut ea).await.expect("A got SAS");
    let code_b = next_sas(&mut eb).await.expect("B got SAS");
    assert_eq!(code_a, code_b, "both machines show the same code");
    assert!(a.handle.pair_decide(true));
    assert!(b.handle.pair_decide(true));
    // Each side finishes independently (B pins after reading A's confirm).
    for rx in [&mut ea, &mut eb] {
        let done = tokio::time::timeout(T, rx.recv()).await.unwrap().unwrap();
        assert!(
            matches!(done, linker_core::pairing::PairEvent::Paired { .. }),
            "{done:?}"
        );
    }
    assert_eq!(a.handle.status().paired.unwrap().device_id, b.id);
    assert_eq!(b.handle.status().paired.unwrap().device_id, a.id);
    wait_for("connected after pairing", T, || {
        a.handle.status().link == LinkState::Connected
            && b.handle.status().link == LinkState::Connected
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_rejected_does_not_pin() {
    init_tracing();
    let (ka, kb) = (
        Identity::generate_pem().unwrap(),
        Identity::generate_pem().unwrap(),
    );
    let (pa, pb) = (free_port(), free_port());
    let a = spawn(NodeSpec {
        name: "a",
        key_pem: ka,
        port: pa,
        peer_port: pb,
        paired_with: None,
        kvm_role: KvmRole::Server,
        camera_role: "sink",
    });
    let b = spawn(NodeSpec {
        name: "b",
        key_pem: kb,
        port: pb,
        peer_port: pa,
        paired_with: None,
        kvm_role: KvmRole::Client,
        camera_role: "source",
    });
    let mut ea = a.handle.pair_begin();
    let mut eb = b.handle.pair_begin();
    next_sas(&mut ea).await.unwrap();
    next_sas(&mut eb).await.unwrap();
    a.handle.pair_decide(true);
    b.handle.pair_decide(false); // user says codes differ
    let ev = tokio::time::timeout(T, ea.recv()).await.unwrap().unwrap();
    assert!(
        matches!(ev, linker_core::pairing::PairEvent::Failed { .. }),
        "{ev:?}"
    );
    assert!(a.handle.status().paired.is_none());
    assert!(b.handle.status().paired.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_text_both_ways_without_echo() {
    init_tracing();
    let p = paired_pair();
    connected(&p).await;
    wait_for("watchers", T, || {
        p.a.mock.st().clip_tx.is_some() && p.b.mock.st().clip_tx.is_some()
    })
    .await;

    let send = |n: &Node, text: &str| {
        let tx = n.mock.st().clip_tx.clone().unwrap();
        tx.try_send(ClipboardChange {
            content: ClipboardContent::Text {
                text: text.into(),
                html: None,
            },
            concealed: false,
        })
        .unwrap();
    };
    send(&p.a, "hello from ubuntu");
    wait_for("B got text", T, || {
        p.b.mock.st().clip_written.iter().any(
            |c| matches!(c, ClipboardContent::Text { text, .. } if text == "hello from ubuntu"),
        )
    })
    .await;
    send(&p.b, "hello from mac");
    wait_for("A got text", T, || {
        p.a.mock
            .st()
            .clip_written
            .iter()
            .any(|c| matches!(c, ClipboardContent::Text { text, .. } if text == "hello from mac"))
    })
    .await;
    // Let any echo settle; each side must have applied exactly one write.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(p.a.mock.st().clip_written.len(), 1, "no echo loop on A");
    assert_eq!(p.b.mock.st().clip_written.len(), 1, "no echo loop on B");

    // Concealed (password) content is not synced.
    p.a.mock
        .st()
        .clip_tx
        .clone()
        .unwrap()
        .try_send(ClipboardChange {
            content: ClipboardContent::Text {
                text: "hunter2".into(),
                html: None,
            },
            concealed: true,
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(p.b.mock.st().clip_written.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_files_and_image_pulled_on_bulk_stream() {
    init_tracing();
    let p = paired_pair();
    connected(&p).await;
    wait_for("watchers", T, || p.b.mock.st().clip_tx.is_some()).await;

    let dir = tempfile::tempdir().unwrap();
    let f1 = dir.path().join("notes.txt");
    let f2 = dir.path().join("big.bin");
    std::fs::write(&f1, "x".repeat(10_000)).unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 31 % 251) as u8).collect();
    std::fs::write(&f2, &big).unwrap();
    p.b.mock
        .st()
        .clip_tx
        .clone()
        .unwrap()
        .try_send(ClipboardChange {
            content: ClipboardContent::Files {
                paths: vec![f1, f2],
            },
            concealed: false,
        })
        .unwrap();
    wait_for("A received files", T, || {
        p.a.mock
            .st()
            .clip_written
            .iter()
            .any(|c| matches!(c, ClipboardContent::Files { .. }))
    })
    .await;
    let paths = match p.a.mock.st().clip_written.last().unwrap().clone() {
        ClipboardContent::Files { paths } => paths,
        _ => unreachable!(),
    };
    assert_eq!(paths.len(), 2);
    assert_eq!(
        std::fs::read_to_string(&paths[0]).unwrap(),
        "x".repeat(10_000)
    );
    assert_eq!(std::fs::read(&paths[1]).unwrap(), big);

    // A 600 KB "image" (above the inline threshold) goes via offer + pull.
    let rgba: Vec<u8> = (0..400 * 400 * 4).map(|i| (i * 7 % 256) as u8).collect();
    let png = linker_platform::hash::rgba_to_png(400, 400, &rgba).unwrap();
    p.b.mock
        .st()
        .clip_tx
        .clone()
        .unwrap()
        .try_send(ClipboardChange {
            content: ClipboardContent::Image { png: png.clone() },
            concealed: false,
        })
        .unwrap();
    wait_for("A received image", T, || {
        p.a.mock
            .st()
            .clip_written
            .iter()
            .any(|c| matches!(c, ClipboardContent::Image { png: got } if *got == png))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifications_and_dnd_sync() {
    init_tracing();
    let p = paired_pair();
    connected(&p).await;
    wait_for("watchers", T, || {
        p.b.mock.st().notif_tx.is_some() && p.a.mock.st().dnd_tx.is_some()
    })
    .await;

    // Mac notification mirrors to Ubuntu.
    p.b.mock
        .st()
        .notif_tx
        .clone()
        .unwrap()
        .try_send(Notification {
            app: "Messages".into(),
            title: "Hi".into(),
            subtitle: String::new(),
            body: "there".into(),
            timestamp_ms: 1,
        })
        .unwrap();
    wait_for("shown on A", T, || {
        p.a.mock.st().shown.iter().any(|n| n.app == "Messages")
    })
    .await;

    // DND on A propagates to B; no ping-pong.
    let a_dnd_tx = p.a.mock.st().dnd_tx.clone().unwrap();
    p.a.mock.st().dnd = true;
    a_dnd_tx.try_send(true).unwrap();
    wait_for("B DND on", T, || p.b.mock.st().dnd).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(p.a.mock.st().dnd, "A stays on (no bounce)");
    assert!(p.b.mock.st().dnd);

    // While DND is on, mirrored banners are suppressed on A.
    let before = p.a.mock.st().shown.len();
    p.b.mock
        .st()
        .notif_tx
        .clone()
        .unwrap()
        .try_send(Notification {
            app: "Mail".into(),
            title: "quiet".into(),
            subtitle: String::new(),
            body: String::new(),
            timestamp_ms: 2,
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(p.a.mock.st().shown.len(), before);

    // DND off on B propagates back to A.
    let b_dnd_tx = p.b.mock.st().dnd_tx.clone().unwrap();
    p.b.mock.st().dnd = false;
    b_dnd_tx.try_send(false).unwrap();
    wait_for("A DND off", T, || !p.a.mock.st().dnd).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kvm_handoff_and_dead_mans_switch() {
    init_tracing();
    let p = paired_pair();
    connected(&p).await;
    wait_for("capture", T, || p.a.mock.st().input_tx.is_some()).await;
    let input = p.a.mock.st().input_tx.clone().unwrap();

    // Push past the right edge of A's 1000x1000 screen.
    input
        .send(LocalInput::Motion {
            dx: 2000,
            dy: 0,
            pos: None,
        })
        .await
        .unwrap();
    wait_for("A captured", T, || p.a.mock.st().captured).await;
    wait_for("B warped", T, || {
        p.b.mock
            .st()
            .injected
            .iter()
            .any(|s| s.starts_with("warp Left"))
    })
    .await;

    input
        .send(LocalInput::Motion {
            dx: 5,
            dy: 3,
            pos: None,
        })
        .await
        .unwrap();
    input
        .send(LocalInput::Key {
            hid: 0x04,
            pressed: true,
        })
        .await
        .unwrap();
    input
        .send(LocalInput::Key {
            hid: 0x04,
            pressed: false,
        })
        .await
        .unwrap();
    wait_for("B injected", T, || {
        let s = p.b.mock.st();
        s.injected.contains(&"move 5 3".to_string())
            && s.injected.contains(&"key 0x4 false".to_string())
    })
    .await;

    // Client pushes back out its left edge → control returns.
    input
        .send(LocalInput::Motion {
            dx: -5000,
            dy: 0,
            pos: None,
        })
        .await
        .unwrap();
    wait_for("A released", T, || !p.a.mock.st().captured).await;

    // Capture again, hold a key, then drop the link: A must ungrab.
    input
        .send(LocalInput::Motion {
            dx: 2000,
            dy: 0,
            pos: None,
        })
        .await
        .unwrap();
    wait_for("A captured again", T, || p.a.mock.st().captured).await;
    input
        .send(LocalInput::Key {
            hid: 0xE1,
            pressed: true,
        })
        .await
        .unwrap();
    wait_for("B holds shift", T, || {
        p.b.mock
            .st()
            .injected
            .contains(&"key 0xe1 true".to_string())
    })
    .await;
    p.b.handle.shutdown();
    wait_for("dead-man release", T, || !p.a.mock.st().captured).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kvm_panic_hotkey_releases() {
    init_tracing();
    let p = paired_pair();
    connected(&p).await;
    wait_for("capture", T, || p.a.mock.st().input_tx.is_some()).await;
    let input = p.a.mock.st().input_tx.clone().unwrap();
    input
        .send(LocalInput::Motion {
            dx: 2000,
            dy: 0,
            pos: None,
        })
        .await
        .unwrap();
    wait_for("captured", T, || p.a.mock.st().captured).await;
    for hid in [0xE0u16, 0xE2, 0xE1, 0x29] {
        input
            .send(LocalInput::Key { hid, pressed: true })
            .await
            .unwrap();
    }
    wait_for("panic release", T, || !p.a.mock.st().captured).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn camera_on_demand_stream() {
    init_tracing();
    let p = paired_pair();
    connected(&p).await;
    // No app reading → the Mac camera must stay off.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        p.b.mock.st().cam_tx.is_none(),
        "camera must not start without demand"
    );

    // An app opens the virtual camera on A.
    p.a.mock.st().sink_consumers = 1;
    wait_for("source started", T, || p.b.mock.st().cam_tx.is_some()).await;
    let cam = p.b.mock.st().cam_tx.clone().unwrap();
    // Keyframe of ~50 KB (fragmented across many datagrams) then P-frames.
    let key: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
    cam.send(CameraEvent::Frame(EncodedFrame {
        data: key.clone(),
        keyframe: true,
        capture_ts_us: now_us(),
    }))
    .await
    .unwrap();
    for i in 0..10u8 {
        tokio::time::sleep(Duration::from_millis(33)).await;
        cam.send(CameraEvent::Frame(EncodedFrame {
            data: vec![i; 3000],
            keyframe: false,
            capture_ts_us: now_us(),
        }))
        .await
        .unwrap();
    }
    wait_for("sink got frames", T, || {
        p.a.mock.st().sink_frames.len() >= 11
    })
    .await;
    assert_eq!(
        p.a.mock.st().sink_frames[0],
        key,
        "keyframe reassembled intact"
    );

    // App closes the device → stream stops after the idle delay.
    p.a.mock.st().sink_consumers = 0;
    wait_for("source stopped", Duration::from_secs(15), || {
        p.b.mock.st().cam_tx.is_none()
    })
    .await;
}
