//! Both audio flows over a deliberately bad network (loss, reordering,
//! jitter and Wi-Fi-like stalls injected on the sending side through
//! `LINKER_AUDIO_IMPAIR`). Separate test binary because the impairment is
//! process-wide.
//!
//! Expectation (the audiobridge design goal): after the adaptive buffer has
//! seen the stalls, there are no audible gaps; lost packets are recovered
//! from redundancy, and the few frames lost in a row are concealed by PLC.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use linker_core::config::KvmRole;
use linker_core::identity::Identity;
use linker_core::status::LinkState;

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn audio_survives_loss_reorder_jitter_and_stalls() {
    init_tracing();
    let spec = std::env::var("LINKER_TEST_IMPAIR").unwrap_or_else(|_| {
        "loss=5,reorder=10,jitter=4,stall_every=2000,stall_ms=60,seed=11".into()
    });
    std::env::set_var(linker_core::features::audio::IMPAIR_ENV, &spec);
    let secs: u64 = std::env::var("LINKER_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let (ka, kb) = (
        Identity::generate_pem().unwrap(),
        Identity::generate_pem().unwrap(),
    );
    let (ida, idb) = (
        identity(&ka, "ubuntu").device_id,
        identity(&kb, "mac").device_id,
    );
    let (pa, pb) = (free_port(), free_port());
    let (ma, mb) = mock_pair();
    let a = spawn_with(
        NodeSpec {
            name: "ubuntu",
            key_pem: ka,
            port: pa,
            peer_port: pb,
            paired_with: Some((idb, "mac".into())),
            kvm_role: KvmRole::Server,
            camera_role: "sink",
            audio_role: "sink",
        },
        ma,
        |_| {},
    );
    let b = spawn_with(
        NodeSpec {
            name: "mac",
            key_pem: kb,
            port: pb,
            peer_port: pa,
            paired_with: Some((ida, "ubuntu".into())),
            kvm_role: KvmRole::Client,
            camera_role: "source",
            audio_role: "source",
        },
        mb,
        |_| {},
    );
    wait_for("connected", Duration::from_secs(15), || {
        a.handle.status().link == LinkState::Connected
            && b.handle.status().link == LinkState::Connected
    })
    .await;
    let (sa, sb) = (a.mock.st().audio.clone(), b.mock.st().audio.clone());
    sa.mic_consumer.store(true, Ordering::SeqCst);
    sa.app_playing.store(true, Ordering::SeqCst);

    // Let the buffer meet a few stalls and adapt (target sized from the
    // largest gap of the last 30 s).
    tokio::time::sleep(Duration::from_secs(8)).await;
    sa.reset_meter(AudioKind::VirtualMic);
    sb.reset_meter(AudioKind::Playback);
    let before = (
        a.handle
            .status()
            .audio
            .iter()
            .find(|s| s.stream == "mic")
            .cloned()
            .unwrap(),
        b.handle
            .status()
            .audio
            .iter()
            .find(|s| s.stream == "speaker")
            .cloned()
            .unwrap(),
    );
    tokio::time::sleep(Duration::from_secs(secs)).await;

    let mic_m = sa.meter(AudioKind::VirtualMic);
    let spk_m = sb.meter(AudioKind::Playback);
    let mic = a
        .handle
        .status()
        .audio
        .into_iter()
        .find(|s| s.stream == "mic")
        .unwrap();
    let spk = b
        .handle
        .status()
        .audio
        .into_iter()
        .find(|s| s.stream == "speaker")
        .unwrap();
    eprintln!("impairment: {spec}");
    for (name, m, st, b4) in [
        ("mic", &mic_m, &mic, &before.0),
        ("speaker", &spk_m, &spk, &before.1),
    ] {
        eprintln!("{}", linker_core::status::render_audio(st));
        eprintln!(
            "{name}: dropouts {} (longest {:.1} ms), max jump {}, latency median {:?} ms, recovered +{} PLC +{} underruns +{}",
            m.dropouts,
            m.longest_dropout_ms,
            m.max_jump,
            m.median_latency_ms(),
            st.recovered - b4.recovered,
            st.plc - b4.plc,
            st.underruns - b4.underruns
        );
        let played = m.samples as f64 / 48_000.0;
        assert!(played >= secs as f64 * 0.95, "{name}: only {played:.1}s");
        assert_eq!(m.dropouts, 0, "{name}: audible gap after adaptation");
        assert_eq!(
            st.underruns - b4.underruns,
            0,
            "{name}: underruns after adaptation"
        );
        assert!(
            st.recovered > b4.recovered,
            "{name}: redundancy never used?"
        );
        // 5 % random loss with 2 redundant copies: three in a row are lost
        // ~0.0125 % of the time → PLC is rare.
        let frames = secs * 200;
        assert!(
            (st.plc - b4.plc) * 1000 / frames < 5,
            "{name}: too much PLC ({} of {frames})",
            st.plc - b4.plc
        );
        assert!(
            st.target_ms >= 50,
            "{name}: buffer did not adapt to 60 ms stalls"
        );
        assert!(
            st.target_ms <= 120,
            "{name}: buffer over-adapted ({} ms)",
            st.target_ms
        );
    }
}
