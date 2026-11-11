//! `linkerd` — the Linker daemon and its command-line interface.

mod audio_check;

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use linker_core::config::{Config, Paths, DEFAULT_CONFIG_TOML};
use linker_core::control::{ControlClient, Request};
use linker_core::Daemon;
use linker_platform::PlatformBackend;

const USAGE: &str = "\
linkerd — Linker continuity daemon (clipboard, notifications, DND, KVM, camera, audio)

USAGE:
    linkerd [OPTIONS] [COMMAND]

COMMANDS:
    run          Run the daemon in the foreground (default; used by the service)
    status       Show link state, paired peer and per-feature health
    pair         Pair with the other machine (run on both, compare the code)
    unpair       Forget the paired machine
    check        Probe OS capabilities/permissions and report what works
    id           Print this device's id and file locations
    init-config  Write a commented config.toml if none exists

OPTIONS:
    --config-dir <DIR>   Use DIR instead of the default config directory
    --json               (status) print raw JSON
    --status             Same as `status`
    -v, --verbose        Debug logging (or set RUST_LOG)
    -h, --help           Show this help
";

struct Args {
    cmd: String,
    config_dir: Option<PathBuf>,
    json: bool,
    verbose: bool,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut a = Args {
        cmd: "run".into(),
        config_dir: None,
        json: false,
        verbose: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" | "help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("linkerd {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--config-dir" => {
                a.config_dir = Some(it.next().context("--config-dir needs a value")?.into())
            }
            "--json" => a.json = true,
            "--status" => a.cmd = "status".into(),
            "-v" | "--verbose" => a.verbose = true,
            "run" | "status" | "pair" | "unpair" | "check" | "id" | "init-config" => a.cmd = arg,
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    Ok(a)
}

fn paths(args: &Args) -> anyhow::Result<Paths> {
    match &args.config_dir {
        Some(d) => Ok(Paths::in_dir(d.clone(), None)),
        None => Paths::default_paths(),
    }
}

fn init_logging(verbose: bool) {
    let default = if verbose {
        "info,linker_core=debug,linker_linux=debug,linker_macos=debug,linkerd=debug"
    } else {
        "info"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default))
        // quinn is chatty at info about benign UDP errors during network changes
        .add_directive("quinn_udp=warn".parse().expect("valid directive"))
        .add_directive("mdns_sd=warn".parse().expect("valid directive"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn make_backend() -> Arc<dyn PlatformBackend> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(linker_linux::LinuxBackend::new())
    }
    #[cfg(target_os = "macos")]
    {
        Arc::new(linker_macos::MacBackend::new())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        compile_error!("linkerd supports Linux and macOS only");
    }
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    init_logging(args.verbose);

    // macOS: AppKit notifications (sleep/wake, …) are delivered on the main
    // thread's run loop, so the async runtime lives on a worker thread there.
    #[cfg(target_os = "macos")]
    {
        linker_macos::run_with_main_loop(move || run_async(args))
    }
    #[cfg(not(target_os = "macos"))]
    {
        run_async(args)
    }
}

fn run_async(args: Args) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("linkerd")
        .build()?;
    rt.block_on(async move {
        let paths = paths(&args)?;
        match args.cmd.as_str() {
            "run" => run_daemon(paths).await,
            "status" => status(&paths, args.json).await,
            "pair" => pair(&paths).await,
            "unpair" => unpair(&paths).await,
            "check" => check(&paths).await,
            "id" => id(&paths).await,
            "init-config" => init_config(&paths),
            _ => unreachable!(),
        }
    })
}

fn init_config(paths: &Paths) -> anyhow::Result<()> {
    std::fs::create_dir_all(&paths.config_dir)?;
    if paths.config_file.exists() {
        println!("{} already exists", paths.config_file.display());
    } else {
        std::fs::write(&paths.config_file, DEFAULT_CONFIG_TOML)?;
        println!("wrote {}", paths.config_file.display());
    }
    Ok(())
}

async fn run_daemon(paths: Paths) -> anyhow::Result<()> {
    if !paths.config_file.exists() {
        let _ = init_config(&paths);
    }
    let cfg = Config::load(&paths.config_file)?;
    let backend = make_backend();
    let daemon = Daemon::new(cfg, paths, backend).await?;
    let handle = daemon.handle();
    let mut task = tokio::spawn(daemon.run());

    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let terminated = term.recv();
    #[cfg(not(unix))]
    let terminated = std::future::pending::<Option<()>>();

    tokio::select! {
        r = &mut task => return r?,
        _ = tokio::signal::ctrl_c() => {}
        _ = terminated => {}
    }
    tracing::info!("signal received; closing link");
    handle.shutdown();
    match tokio::time::timeout(Duration::from_secs(3), task).await {
        Ok(r) => r?,
        Err(_) => Ok(()),
    }
}

async fn status(paths: &Paths, json: bool) -> anyhow::Result<()> {
    let mut c = ControlClient::connect(&paths.socket).await?;
    c.send(&Request::Status).await?;
    let v = c.recv().await?.context("daemon closed the connection")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        let s: linker_core::status::Status = serde_json::from_value(v)
            .context("unexpected status format (daemon/CLI version mismatch?)")?;
        print!("{}", linker_core::status::render(&s));
    }
    Ok(())
}

async fn unpair(paths: &Paths) -> anyhow::Result<()> {
    let mut c = ControlClient::connect(&paths.socket).await?;
    c.send(&Request::Unpair).await?;
    c.recv().await?;
    println!("Unpaired. Run `linkerd pair` on both machines to pair again.");
    Ok(())
}

async fn pair(paths: &Paths) -> anyhow::Result<()> {
    let mut c = ControlClient::connect(&paths.socket).await?;
    c.send(&Request::Pair).await?;
    println!("Pairing mode is open for 2 minutes.");
    println!("Run `linkerd pair` on the other machine too (same Wi-Fi/LAN)…");
    loop {
        let Some(ev) = c.recv().await? else {
            bail!("daemon closed the connection")
        };
        match ev["event"].as_str().unwrap_or_default() {
            "waiting" => {}
            "sas" => {
                let code = ev["code"].as_str().unwrap_or("?");
                let peer = ev["peer_name"].as_str().unwrap_or("?");
                println!();
                println!("  Found \"{peer}\". Pairing code:");
                println!();
                println!("        {code}");
                println!();
                print!("  Does the other machine show the SAME code? [y/N] ");
                std::io::stdout().flush()?;
                let answer = tokio::task::spawn_blocking(|| {
                    let mut s = String::new();
                    std::io::stdin().lock().read_line(&mut s).map(|_| s)
                })
                .await??;
                let accept = matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes");
                c.send(&Request::Decide { accept }).await?;
                if accept {
                    println!("  Waiting for the other machine to confirm…");
                }
            }
            "paired" => {
                println!(
                    "✔ Paired with \"{}\". The link connects automatically from now on.",
                    ev["peer_name"].as_str().unwrap_or("?")
                );
                return Ok(());
            }
            "failed" => bail!(
                "pairing failed: {}",
                ev["reason"].as_str().unwrap_or("unknown")
            ),
            _ => {}
        }
    }
}

async fn id(paths: &Paths) -> anyhow::Result<()> {
    let cfg = Config::load(&paths.config_file)?;
    println!("config dir : {}", paths.config_dir.display());
    println!("config     : {}", paths.config_file.display());
    println!("paired     : {}", paths.paired_file.display());
    println!("socket     : {}", paths.socket.display());
    println!("name       : {}", cfg.device_name());
    match std::fs::read_to_string(&paths.identity_file) {
        Ok(s) => print!("identity   :\n{s}"),
        Err(_) => println!("identity   : (not created yet — start the daemon once)"),
    }
    Ok(())
}

/// Probe each capability and print a readable report (used by the runbook).
async fn check(paths: &Paths) -> anyhow::Result<()> {
    use linker_platform::*;
    let cfg = Config::load(&paths.config_file)?;
    let b = make_backend();
    let ok = |name: &str, msg: String| println!("  [ OK ] {name:<22} {msg}");
    let bad = |name: &str, msg: String| println!("  [FAIL] {name:<22} {msg}");
    println!("linkerd check — platform {}", b.name());

    println!("  [ .. ] {:<22} {}", "config", paths.config_file.display());
    ok(
        "keystore",
        if b.keystore_available() {
            "OS keystore available".into()
        } else {
            "file (0600) fallback".into()
        },
    );

    // Clipboard
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    match b.clipboard_watch(tx) {
        Ok(()) => ok("clipboard watch", "started".into()),
        Err(e) => bad("clipboard watch", e.0),
    }
    drop(rx.try_recv());

    // Notifications
    let n = Notification {
        app: "Linker".into(),
        title: "Linker check".into(),
        subtitle: String::new(),
        body: "If you can see this, notifications work.".into(),
        timestamp_ms: now_ms(),
    };
    match b.show_notification(&n) {
        Ok(()) => ok("notification", "test banner sent — did it appear?".into()),
        Err(e) => bad("notification", format!("{e:#}")),
    }
    let (ntx, _nrx) = tokio::sync::mpsc::channel(4);
    match b.watch_notifications(ntx) {
        Ok(()) => ok("notification capture", "available".into()),
        Err(e) => println!("  [ -- ] {:<22} {}", "notification capture", e.0),
    }

    // DND
    match b.get_dnd() {
        Ok(on) => ok(
            "dnd read",
            format!("currently {}", if on { "ON" } else { "off" }),
        ),
        Err(e) => bad("dnd read", e.0),
    }
    let (dtx, _drx) = tokio::sync::mpsc::channel(4);
    match b.watch_dnd(dtx) {
        Ok(()) => ok("dnd watch", "available".into()),
        Err(e) => println!("  [ -- ] {:<22} {}", "dnd watch", e.0),
    }

    // Power
    let (ptx, _prx) = tokio::sync::mpsc::channel(4);
    match b.watch_power(ptx) {
        Ok(()) => ok("sleep/wake events", "available".into()),
        Err(e) => println!(
            "  [ -- ] {:<22} {} (clock-jump fallback is used)",
            "sleep/wake events", e.0
        ),
    }

    // KVM — report the DAEMON's own permission state. The daemon tracks live
    // readiness in its "kvm" feature health; asking it over the socket is
    // authoritative. Probing here in the CLI process is misleading on macOS,
    // where the OS attributes the Accessibility check to Terminal, not linkerd.
    let dstatus = daemon_status(paths).await;
    match dstatus.as_ref().and_then(|s| s.features.get("kvm")) {
        Some(h) if kvm_health_ok(h) => ok("kvm (daemon)", h.clone()),
        Some(h) => bad("kvm (daemon)", h.clone()),
        None => {
            // Daemon not running yet: fall back to a local probe, clearly
            // labelled as the CLI process (macOS attributes it to Terminal).
            let note = "daemon not running — probed in THIS process";
            match cfg.kvm_role() {
                linker_core::config::KvmRole::Client => match b.input_injector() {
                    Ok(i) => ok(
                        "kvm inject (cli)",
                        format!("screen {:?} ({note})", i.screen_size()),
                    ),
                    Err(e) => bad("kvm inject (cli)", format!("{} ({note})", e.0)),
                },
                _ => {
                    let (itx, _irx) = tokio::sync::mpsc::channel(16);
                    match b.input_capture(itx) {
                        Ok(c) => ok(
                            "kvm capture (cli)",
                            format!("screen {:?} ({note})", c.screen_size()),
                        ),
                        Err(e) => bad("kvm capture (cli)", format!("{} ({note})", e.0)),
                    }
                }
            }
        }
    }

    // Camera
    let params = CameraParams {
        width: cfg.camera.width,
        height: cfg.camera.height,
        fps: cfg.camera.fps,
        bitrate_kbps: cfg.camera.bitrate_kbps,
    };
    if cfg!(target_os = "macos") {
        let (ctx, mut crx) = tokio::sync::mpsc::channel(8);
        match b.camera_source(params, ctx) {
            Ok(src) => {
                let got = tokio::time::timeout(Duration::from_secs(5), async {
                    while let Some(ev) = crx.recv().await {
                        match ev {
                            CameraEvent::Frame(f) if f.keyframe => return Ok(f.data.len()),
                            CameraEvent::State {
                                state: CamState::Unavailable | CamState::Busy,
                                detail,
                            } => return Err(detail),
                            _ => {}
                        }
                    }
                    Err("no frames".into())
                })
                .await;
                drop(src);
                match got {
                    Ok(Ok(n)) => ok(
                        "camera source",
                        format!("captured + encoded a keyframe ({n} bytes)"),
                    ),
                    Ok(Err(e)) => bad("camera source", e),
                    Err(_) => bad(
                        "camera source",
                        "no frame within 5 s (permission prompt pending?)".into(),
                    ),
                }
            }
            Err(e) => bad("camera source", e.0),
        }
    } else {
        match b.camera_sink(params) {
            Ok(s) => ok(
                "camera sink",
                format!("{} via {}", s.device_path(), s.decoder_name()),
            ),
            Err(e) => bad("camera sink", e.0),
        }
    }

    // Audio (microphone + speaker sharing)
    if cfg.audio.enabled {
        let source = match cfg.audio.role.as_str() {
            "source" => true,
            "sink" => false,
            _ => cfg!(target_os = "macos"),
        };
        let (b2, acfg) = (b.clone(), cfg.audio.clone());
        tokio::task::spawn_blocking(move || {
            if source {
                audio_check::check_devices(&*b2, &acfg)
            } else {
                audio_check::check_virtual(&*b2, &acfg)
            }
        })
        .await?;
    } else {
        println!("  [ -- ] {:<22} disabled in config", "audio");
    }

    // Daemon reachable? (reuse the snapshot fetched for the KVM check)
    match &dstatus {
        Some(s) => {
            ok("daemon", format!("running ({})", paths.socket.display()));
            audio_check::live(s);
        }
        None => println!("  [ -- ] {:<22} not running", "daemon"),
    }
    tokio::time::sleep(Duration::from_millis(300)).await; // let async probes print
    Ok(())
}

/// Fetch the running daemon's status over the control socket, if reachable.
async fn daemon_status(paths: &Paths) -> Option<linker_core::status::Status> {
    let mut c = ControlClient::connect(&paths.socket).await.ok()?;
    c.send(&Request::Status).await.ok()?;
    let v = c.recv().await.ok()??;
    serde_json::from_value(v).ok()
}

/// Classify a daemon "kvm" feature-health string as OK or a problem.
fn kvm_health_ok(h: &str) -> bool {
    let h = h.to_lowercase();
    !(h.contains("not ready")
        || h.contains("unsupported")
        || h.contains("failed")
        || h.contains("disabled"))
}
