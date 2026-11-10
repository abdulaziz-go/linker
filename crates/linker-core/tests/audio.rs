//! Audio integration tests: two real daemons over QUIC on localhost with
//! simulated real-time audio devices (see `common/audio_sim.rs`).
//!
//! Node A = Ubuntu-like (audio sink: virtual "Linker Microphone" + "Linker
//! Speaker", camera sink), node B = Mac-like (audio source: microphone +
//! speakers, camera source).
//!
//! `LINKER_SOAK_SECS` (default 20) sets how long the combined
//! audio + camera test streams; the acceptance run uses 300 (5 minutes).

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use linker_core::config::{AudioMode, Config, KvmRole};
use linker_core::identity::Identity;
use linker_core::status::LinkState;
use linker_platform::{now_us, CameraEvent, EncodedFrame};

const T: Duration = Duration::from_secs(15);

struct Keys {
    ka: String,
    kb: String,
    ida: String,
    idb: String,
    pa: u16,
    pb: u16,
}

fn keys() -> Keys {
    let (ka, kb) = (
        Identity::generate_pem().unwrap(),
        Identity::generate_pem().unwrap(),
    );
    let (ida, idb) = (
        identity(&ka, "ubuntu").device_id,
        identity(&kb, "mac").device_id,
    );
    Keys {
        ka,
        kb,
        ida,
        idb,
        pa: free_port(),
        pb: free_port(),
    }
}

fn ubuntu(k: &Keys, mock: Mock, tweak: impl FnOnce(&mut Config)) -> Node {
    spawn_with(
        NodeSpec {
            name: "ubuntu",
            key_pem: k.ka.clone(),
            port: k.pa,
            peer_port: k.pb,
            paired_with: Some((k.idb.clone(), "mac".into())),
            kvm_role: KvmRole::Server,
            camera_role: "sink",
            audio_role: "sink",
        },
        mock,
        tweak,
    )
}

fn mac(k: &Keys, mock: Mock, tweak: impl FnOnce(&mut Config)) -> Node {
    spawn_with(
        NodeSpec {
            name: "mac",
            key_pem: k.kb.clone(),
            port: k.pb,
            peer_port: k.pa,
            paired_with: Some((k.ida.clone(), "ubuntu".into())),
            kvm_role: KvmRole::Client,
            camera_role: "source",
            audio_role: "source",
        },
        mock,
        tweak,
    )
}

async fn both_connected(a: &Node, b: &Node) {
    wait_for("both CONNECTED", T, || {
        a.handle.status().link == LinkState::Connected
            && b.handle.status().link == LinkState::Connected
    })
    .await;
}

fn audio_of(n: &Node, stream: &str) -> Option<linker_core::status::AudioStats> {
    n.handle
        .status()
        .audio
        .into_iter()
        .find(|a| a.stream == stream)
}

fn sim(n: &Node) -> std::sync::Arc<AudioSim> {
    n.mock.st().audio.clone()
}

/// Assert a consumer meter shows continuous audio with no dropouts.
fn assert_clean(what: &str, m: &audio_sim::Meter, min_secs: f64) {
    let secs = m.samples as f64 / 48_000.0;
    eprintln!(
        "{what}: {secs:.1}s played, dropouts {} (longest {:.1} ms), max jump {}, latency median {:?} ms (n={})",
        m.dropouts,
        m.longest_dropout_ms,
        m.max_jump,
        m.median_latency_ms(),
        m.latencies_ms.len()
    );
    assert!(secs >= min_secs, "{what}: only {secs:.1}s of audio pulled");
    assert!(
        m.nonzero_samples as f64 >= 0.98 * m.samples as f64,
        "{what}: {} of {} samples were silent",
        m.samples - m.nonzero_samples,
        m.samples
    );
    assert_eq!(m.dropouts, 0, "{what}: audible gaps");
    // 440 Hz at amplitude ≤ 22000 changes by ≤ ~1270 per sample; Opus
    // coding noise adds a little. A click is far above this.
    assert!(m.max_jump < 4000, "{what}: click (jump {})", m.max_jump);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mic_is_on_demand() {
    init_tracing();
    let k = keys();
    let a = ubuntu(&k, Mock::default(), |_| {});
    let b = mac(&k, Mock::default(), |_| {});
    both_connected(&a, &b).await;
    let (sa, sb) = (sim(&a), sim(&b));
    // The virtual devices exist on Ubuntu from the start …
    wait_for("virtual devices created", T, || {
        sa.live(AudioKind::VirtualMic) == 1 && sa.live(AudioKind::VirtualSpeaker) == 1
    })
    .await;
    // … but the Mac microphone stays off while no app records.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        sb.opened(AudioKind::Capture),
        0,
        "mic must not start without demand"
    );

    sa.mic_consumer.store(true, Ordering::SeqCst);
    wait_for("Mac mic opened", T, || sb.live(AudioKind::Capture) == 1).await;
    wait_for("audio reaches Linker Microphone", T, || {
        sa.meter(AudioKind::VirtualMic).nonzero_samples > 48_000
    })
    .await;

    // App closes → mic released after the idle delay (~5 s).
    sa.mic_consumer.store(false, Ordering::SeqCst);
    wait_for("Mac mic closed", Duration::from_secs(15), || {
        sb.live(AudioKind::Capture) == 0
    })
    .await;
    wait_for("Mac mic reported idle", Duration::from_secs(3), || {
        audio_of(&b, "mic").is_some_and(|st| st.direction == "send" && st.state == "idle")
    })
    .await;
}

/// The acceptance scenario: Zoom on Ubuntu uses Linker Camera + Linker
/// Microphone + Linker Speaker at the same time.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn both_flows_and_camera_together() {
    init_tracing();
    let secs: u64 = std::env::var("LINKER_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let k = keys();
    let (ma, mb) = mock_pair();
    // Clock drift between the machines' audio clocks.
    ma.st().audio.drift_ppm[AudioKind::VirtualSpeaker as usize].store(-150, Ordering::SeqCst);
    mb.st().audio.drift_ppm[AudioKind::Capture as usize].store(200, Ordering::SeqCst);
    let a = ubuntu(&k, ma, |_| {});
    let b = mac(&k, mb, |_| {});
    both_connected(&a, &b).await;
    let (sa, sb) = (sim(&a), sim(&b));

    // Zoom opens all three devices.
    sa.mic_consumer.store(true, Ordering::SeqCst);
    sa.app_playing.store(true, Ordering::SeqCst);
    a.mock.st().sink_consumers = 1;
    wait_for("camera source started", T, || b.mock.st().cam_tx.is_some()).await;
    wait_for("Mac output opened", T, || sb.live(AudioKind::Playback) == 1).await;
    wait_for("Mac mic opened", T, || sb.live(AudioKind::Capture) == 1).await;

    // Camera: 30 fps frames for the whole run.
    let cam = b.mock.st().cam_tx.clone().unwrap();
    let cam_task = tokio::spawn(async move {
        let key: Vec<u8> = (0..40_000u32).map(|i| i as u8).collect();
        let mut i = 0u32;
        let mut tick = tokio::time::interval(Duration::from_millis(33));
        loop {
            tick.tick().await;
            let keyframe = i.is_multiple_of(60);
            let data = if keyframe {
                key.clone()
            } else {
                vec![i as u8; 4000]
            };
            if cam
                .send(CameraEvent::Frame(EncodedFrame {
                    data,
                    keyframe,
                    capture_ts_us: now_us(),
                }))
                .await
                .is_err()
            {
                return;
            }
            i += 1;
        }
    });

    // Warm-up: buffers adapt, then measure a clean window.
    tokio::time::sleep(Duration::from_secs(3)).await;
    sa.reset_meter(AudioKind::VirtualMic);
    sb.reset_meter(AudioKind::Playback);
    let cam_before = a.mock.st().sink_frames.len();
    // Underruns while the adaptive buffer learns the link are expected
    // (audiobridge: "1–2 at the start is normal"); count only from here.
    let under0 = (
        audio_of(&a, "mic").unwrap().underruns,
        audio_of(&b, "speaker").unwrap().underruns,
    );
    tokio::time::sleep(Duration::from_secs(secs)).await;

    let mic = sa.meter(AudioKind::VirtualMic);
    let spk = sb.meter(AudioKind::Playback);
    assert_clean("mic   (Mac → Linker Microphone)", &mic, secs as f64 * 0.95);
    assert_clean("speaker (Linker Speaker → Mac)", &spk, secs as f64 * 0.95);

    let cam_frames = a.mock.st().sink_frames.len() - cam_before;
    let st_a = a.handle.status();
    let cam_stats = st_a.camera.clone().unwrap();
    let mic_st = audio_of(&a, "mic").unwrap();
    let spk_st = audio_of(&b, "speaker").unwrap();
    eprintln!(
        "camera: {cam_frames} frames in {secs}s, latency {:.1} ms",
        cam_stats.latency_ms
    );
    eprintln!("{}", linker_core::status::render_audio(&mic_st));
    eprintln!("{}", linker_core::status::render_audio(&spk_st));
    eprintln!(
        "{}",
        linker_core::status::render_audio(&audio_of(&b, "mic").unwrap())
    );

    // Camera unaffected: ~30 fps throughout.
    assert!(
        cam_frames as f64 >= secs as f64 * 30.0 * 0.9,
        "camera starved: {cam_frames} frames"
    );
    // Receiver stats: nothing lost on a clean link.
    assert_eq!(mic_st.plc, 0, "mic PLC on a clean link");
    assert_eq!(spk_st.plc, 0, "speaker PLC on a clean link");
    assert_eq!(
        mic_st.underruns - under0.0,
        0,
        "mic underruns after warm-up"
    );
    assert_eq!(
        spk_st.underruns - under0.1,
        0,
        "speaker underruns after warm-up"
    );
    // The receiver knows the sender's current bitrate.
    assert!(mic_st.bitrate_kbps >= 64, "{mic_st:?}");

    // Latency (simulated devices, localhost): capture → playout.
    let mic_lat = mic.median_latency_ms().expect("mic markers detected");
    let spk_lat = spk.median_latency_ms().expect("speaker markers detected");
    assert!(mic_lat < 60.0, "mic latency {mic_lat:.1} ms");
    assert!(spk_lat < 60.0, "speaker latency {spk_lat:.1} ms");
    // Lip sync for the call: audio vs video pipeline latency on Ubuntu.
    // ITU-R BT.1359: detectable beyond +45 ms (audio early) / −125 ms (late).
    let skew = mic_lat - cam_stats.latency_ms;
    eprintln!("A/V skew (mic − camera pipeline latency): {skew:+.1} ms");
    assert!(
        (-45.0..=125.0).contains(&skew),
        "A/V out of sync: audio {mic_lat:.1} ms vs video {:.1} ms",
        cam_stats.latency_ms
    );

    cam_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directions_are_independent() {
    init_tracing();
    let k = keys();
    let a = ubuntu(&k, Mock::default(), |_| {});
    let b = mac(&k, Mock::default(), |_| {});
    both_connected(&a, &b).await;
    let (sa, sb) = (sim(&a), sim(&b));

    // Speaker only: Mac output plays, Mac mic stays off.
    sa.app_playing.store(true, Ordering::SeqCst);
    wait_for("speaker audio on the Mac", T, || {
        sb.meter(AudioKind::Playback).nonzero_samples > 48_000
    })
    .await;
    assert_eq!(sb.opened(AudioKind::Capture), 0, "mic must stay off");

    // Speaker stops (app paused) → Mac releases the output after idle_close.
    sa.app_playing.store(false, Ordering::SeqCst);
    // Mic only.
    sa.mic_consumer.store(true, Ordering::SeqCst);
    wait_for("mic audio on Ubuntu", T, || {
        sa.meter(AudioKind::VirtualMic).nonzero_samples > 48_000
    })
    .await;
    wait_for(
        "Mac output closed when idle",
        Duration::from_secs(20),
        || sb.live(AudioKind::Playback) == 0,
    )
    .await;
    sa.reset_meter(AudioKind::VirtualMic);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_clean("mic alone", &sa.meter(AudioKind::VirtualMic), 2.8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_recovers_after_peer_restart_and_device_failure() {
    init_tracing();
    let k = keys();
    let ma = Mock::default();
    let a = ubuntu(&k, ma.clone(), |_| {});
    let b = mac(&k, Mock::default(), |_| {});
    both_connected(&a, &b).await;
    let sa = sim(&a);
    sa.mic_consumer.store(true, Ordering::SeqCst);
    sa.app_playing.store(true, Ordering::SeqCst);
    wait_for("mic audio flowing", T, || {
        sa.meter(AudioKind::VirtualMic).nonzero_samples > 24_000
    })
    .await;

    // 1. Mac goes away (sleep / crash / link drop) …
    b.handle.shutdown();
    let _ = b.task.await;
    wait_for("A notices", Duration::from_secs(25), || {
        a.handle.status().link != LinkState::Connected
    })
    .await;
    // … the virtual mic keeps running (apps don't error) and goes silent.
    assert_eq!(sa.live(AudioKind::VirtualMic), 1);

    // 2. … and comes back (wake / restart): audio resumes by itself.
    let b2 = mac(&k, Mock::default(), |_| {});
    both_connected(&a, &b2).await;
    sa.reset_meter(AudioKind::VirtualMic);
    let sb2 = sim(&b2);
    wait_for("mic audio resumed after reconnect", T, || {
        sa.meter(AudioKind::VirtualMic).nonzero_samples > 48_000
    })
    .await;
    wait_for("speaker audio resumed after reconnect", T, || {
        sb2.meter(AudioKind::Playback).nonzero_samples > 48_000
    })
    .await;

    // 3. CoreAudio error (e.g. after wake / device unplugged) on the Mac mic:
    //    the device is reopened and the stream continues.
    let opened = sb2.opened(AudioKind::Capture);
    assert!(sb2.inject_failure(AudioKind::Capture, "device lost"));
    wait_for("Mac mic reopened", T, || {
        sb2.opened(AudioKind::Capture) > opened && sb2.live(AudioKind::Capture) == 1
    })
    .await;
    // 4. PipeWire hiccup on the Ubuntu virtual speaker: recreated.
    let opened = sa.opened(AudioKind::VirtualSpeaker);
    assert!(sa.inject_failure(AudioKind::VirtualSpeaker, "pipewire restarted"));
    wait_for("virtual speaker recreated", T, || {
        sa.opened(AudioKind::VirtualSpeaker) > opened && sa.live(AudioKind::VirtualSpeaker) == 1
    })
    .await;
    // Steady again afterwards.
    tokio::time::sleep(Duration::from_secs(2)).await;
    sa.reset_meter(AudioKind::VirtualMic);
    sb2.reset_meter(AudioKind::Playback);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_clean("mic after recovery", &sa.meter(AudioKind::VirtualMic), 3.8);
    assert_clean(
        "speaker after recovery",
        &sb2.meter(AudioKind::Playback),
        3.8,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_drop_without_restart_recovers() {
    init_tracing();
    let k = keys();
    let a = ubuntu(&k, Mock::default(), |c| {
        c.audio.mic_mode = AudioMode::Always
    });
    let b = mac(&k, Mock::default(), |_| {});
    both_connected(&a, &b).await;
    let (sa, sb) = (sim(&a), sim(&b));
    sa.mic_consumer.store(true, Ordering::SeqCst);
    wait_for("mic flowing", T, || {
        sa.meter(AudioKind::VirtualMic).nonzero_samples > 24_000
    })
    .await;
    // Kill the QUIC connection under both daemons (like a Wi-Fi drop): the
    // supervisors reconnect on their own.
    let link = a.handle.0.link().unwrap();
    let old = link.session_id;
    link.conn.close(7u32.into(), b"test drop");
    wait_for("new session", Duration::from_secs(30), || {
        a.handle.0.link().is_some_and(|l| l.session_id != old)
            && b.handle.status().link == LinkState::Connected
    })
    .await;
    sa.reset_meter(AudioKind::VirtualMic);
    wait_for("mic resumed on the new session", T, || {
        sa.meter(AudioKind::VirtualMic).nonzero_samples > 48_000
    })
    .await;
    assert_eq!(
        sb.live(AudioKind::Capture),
        1,
        "exactly one mic capture open"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_audio_reports_reason_and_never_opens_devices() {
    init_tracing();
    let k = keys();
    let a = ubuntu(&k, Mock::default(), |_| {});
    let b = mac(&k, Mock::default(), |c| c.audio.mic = false);
    both_connected(&a, &b).await;
    let (sa, sb) = (sim(&a), sim(&b));
    sa.mic_consumer.store(true, Ordering::SeqCst);
    wait_for("peer reason shown on Ubuntu", T, || {
        audio_of(&a, "mic").is_some_and(|s| s.state.contains("disabled"))
    })
    .await;
    assert_eq!(sb.opened(AudioKind::Capture), 0);
    // The other direction still works.
    sa.app_playing.store(true, Ordering::SeqCst);
    wait_for("speaker still works", T, || {
        sb.meter(AudioKind::Playback).nonzero_samples > 24_000
    })
    .await;
}
