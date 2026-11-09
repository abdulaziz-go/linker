//! Link supervisor: keeps exactly one healthy link to the paired peer.
//!
//! ```text
//!   SEARCHING ──(candidate address & our turn to dial)──▶ CONNECTING
//!       ▲                                                    │
//!       │◀──────────── dial failed (capped backoff) ─────────┤
//!       │                                                    ▼
//!       └──────── heartbeat lost / closed / sleep ──────── CONNECTED
//! ```
//!
//! * Tie-break: the device with the lexicographically smaller `device_id`
//!   dials; the other listens. If the listener has been without a link for a
//!   while (e.g. one-way discovery), it dials too; a duplicate is resolved by
//!   keeping the connection dialed by the smaller id.
//! * After `not_found_grace_secs` without a link, exactly one "peer not found"
//!   notification per down-episode; retries continue quietly.
//! * Sleep/wake (OS events, plus wall-clock jump detection as a portable
//!   fallback) drops the stale link and redials immediately.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use linker_platform::{now_ms, Notification, PowerEvent};
use linker_proto::{Hello, Message, PairConfirm};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::discovery::{Discovered, Discovery, SelfInfo};
use crate::identity::short_id;
use crate::link::{self, Link, Router};
use crate::pairing::{sas_code, PairEvent};
use crate::status::{LinkState, PeerStatus};
use crate::tls::{peer_fingerprint, SERVER_NAME};
use crate::trust::PairedPeer;
use crate::Shared;

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
/// Listener starts dialing too after this long without a link.
const LISTENER_FALLBACK: Duration = Duration::from_secs(20);
const PAIR_DECISION_TIMEOUT: Duration = Duration::from_secs(120);

enum Event {
    Established {
        conn: Connection,
        hello: Hello,
        send: SendStream,
        recv: RecvStream,
        we_dialed: bool,
    },
    DialFailed(String),
    SessionEnded {
        session_id: u64,
        reason: String,
    },
    PairingDone {
        stable_id: usize,
    },
}

struct PairingConn {
    conn: Connection,
    preferred: bool,
    task: JoinHandle<()>,
}

/// Close code used when dropping the non-preferred of two simultaneous connections.
const CLOSE_DUPLICATE: u32 = 3;

struct Current {
    session_id: u64,
    link: Link,
    /// Was this connection dialed by the smaller device id?
    preferred: bool,
    task: JoinHandle<()>,
}

struct Candidate {
    name: String,
    addrs: Vec<SocketAddr>,
}

pub async fn run(shared: Arc<Shared>, endpoint: Endpoint, router: Router) -> anyhow::Result<()> {
    let me = shared.identity.device_id.clone();
    let (ev_tx, mut ev_rx) = mpsc::channel::<Event>(64);

    // Discovery.
    let (disc_tx, mut disc_rx) = mpsc::channel::<Discovered>(64);
    let discovery = Discovery::start(
        SelfInfo {
            device_id: me.clone(),
            device_name: shared.identity.device_name.clone(),
            device_type: shared.identity.device_type.clone(),
            port: endpoint.local_addr()?.port(),
        },
        shared.cfg.mdns,
        shared.cfg.broadcast.then_some(shared.cfg.discovery_port),
        disc_tx,
    )
    .await;

    // Power events from the OS.
    let (power_tx, mut power_rx) = mpsc::channel::<PowerEvent>(8);
    match shared.backend.watch_power(power_tx.clone()) {
        Ok(()) => shared
            .status
            .feature("power", "active (OS sleep/wake events)"),
        Err(e) => shared
            .status
            .feature("power", format!("clock-jump detection only ({})", e.0)),
    }

    // Incoming connections.
    {
        let endpoint = endpoint.clone();
        let shared = shared.clone();
        let ev_tx = ev_tx.clone();
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let shared = shared.clone();
                let ev_tx = ev_tx.clone();
                tokio::spawn(async move {
                    let remote = incoming.remote_address();
                    match accept_one(&shared, incoming).await {
                        Ok(ev) => {
                            let _ = ev_tx.send(ev).await;
                        }
                        Err(e) => tracing::info!(%remote, "incoming connection rejected: {e:#}"),
                    }
                });
            }
        });
    }

    let static_addrs: Vec<SocketAddr> = shared
        .cfg
        .static_peers
        .iter()
        .filter_map(|s| match s.parse() {
            Ok(a) => Some(a),
            Err(_) => {
                tracing::warn!("ignoring invalid static peer address {s:?}");
                None
            }
        })
        .collect();

    let mut candidates: HashMap<String, Candidate> = HashMap::new();
    let mut current: Option<Current> = None;
    let mut next_session = 1u64;
    let mut dialing = false;
    let mut backoff = BACKOFF_MIN;
    let mut next_dial = Instant::now();
    let mut down_since = Instant::now();
    let mut notified = false;
    let mut pairing_conn: Option<PairingConn> = None;
    let mut unpaired_logged = false;
    let mut clock = (Instant::now(), SystemTime::now());
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    shared.status.link(LinkState::Searching);
    let mut shutdown = shared.shutdown_signal();

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("shutting down");
                if let Some(cur) = current.take() {
                    cur.link.conn.close(0u32.into(), b"shutdown");
                    let _ = tokio::time::timeout(Duration::from_millis(500), cur.task).await;
                }
                if let Some(pc) = pairing_conn.take() {
                    pc.task.abort();
                }
                shared.set_link(None);
                endpoint.close(0u32.into(), b"shutdown");
                let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
                drop(discovery);
                return Ok(());
            }

            Some(d) = disc_rx.recv() => {
                let paired = shared.trust.paired();
                let is_paired_peer = paired.as_ref().is_some_and(|p| p.device_id == d.device_id);
                if is_paired_peer || shared.pairing.is_active() {
                    let known = candidates.get(&d.device_id).is_some_and(|c| c.addrs == d.addrs);
                    if !known {
                        tracing::info!(peer = %d.device_name, id = short_id(&d.device_id), addrs = ?d.addrs, via = d.via, "discovered");
                        // New info: try right away instead of waiting out the backoff.
                        next_dial = Instant::now();
                    }
                }
                candidates.insert(d.device_id.clone(), Candidate { name: d.device_name, addrs: d.addrs });
            }

            Some(ev) = ev_rx.recv() => match ev {
                Event::DialFailed(e) => {
                    dialing = false;
                    tracing::debug!("dial failed: {e}; retry in {backoff:?}");
                    next_dial = Instant::now() + backoff;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    if current.is_none() && pairing_conn.is_none() {
                        shared.status.link(LinkState::Searching);
                    }
                }
                Event::Established { conn, hello, send, recv, we_dialed } => {
                    if we_dialed {
                        dialing = false;
                    }
                    let peer_id = hello.device_id.clone();
                    let dialer = if we_dialed { me.as_str() } else { peer_id.as_str() };
                    let preferred = dialer == me.as_str().min(peer_id.as_str());
                    if !shared.trust.is_pinned(&peer_id) {
                        if !shared.pairing.is_active() {
                            conn.close(2u32.into(), b"not paired");
                            continue;
                        }
                        // Both sides may have dialed each other; both keep the
                        // connection dialed by the smaller id.
                        if let Some(pc) = &pairing_conn {
                            if pc.preferred || !preferred {
                                conn.close(CLOSE_DUPLICATE.into(), b"duplicate");
                                continue;
                            }
                            pc.task.abort();
                            pc.conn.close(CLOSE_DUPLICATE.into(), b"duplicate");
                        }
                        shared.status.link(LinkState::Pairing);
                        let stable_id = conn.stable_id();
                        let task = {
                            let shared = shared.clone();
                            let ev_tx = ev_tx.clone();
                            let conn = conn.clone();
                            tokio::spawn(async move {
                                if let Err(e) = run_pairing(&shared, conn.clone(), hello, send, recv).await {
                                    let dup = matches!(conn.close_reason(),
                                        Some(quinn::ConnectionError::ApplicationClosed(c)) if c.error_code == CLOSE_DUPLICATE.into());
                                    if !dup {
                                        shared.pairing.finish(PairEvent::Failed { reason: format!("{e:#}") });
                                    }
                                }
                                let _ = ev_tx.send(Event::PairingDone { stable_id }).await;
                            })
                        };
                        pairing_conn = Some(PairingConn { conn, preferred, task });
                        continue;
                    }

                    if let Some(cur) = &current {
                        if cur.preferred && !preferred {
                            tracing::debug!("duplicate connection; keeping the preferred one");
                            conn.close(CLOSE_DUPLICATE.into(), b"duplicate");
                            continue;
                        }
                        tracing::info!("replacing existing link with a newer connection");
                        cur.link.conn.close(CLOSE_DUPLICATE.into(), b"replaced");
                    }

                    let session_id = next_session;
                    next_session += 1;
                    let addr = conn.remote_address();
                    let (lnk, ctl_rx) = Link::new(session_id, conn, hello);
                    tracing::info!(
                        peer = %lnk.peer.device_name, %addr, session_id, dialed = we_dialed,
                        "CONNECTED"
                    );
                    let task = {
                        let lnk = lnk.clone();
                        let router = router.clone();
                        let ev_tx = ev_tx.clone();
                        tokio::spawn(async move {
                            let reason = link::run_session(lnk, send, recv, ctl_rx, router).await;
                            let _ = ev_tx.send(Event::SessionEnded { session_id, reason: format!("{reason:?}") }).await;
                        })
                    };
                    if notified {
                        let _ = shared.backend.show_notification(&own_notification(
                            "Linker reconnected",
                            &format!("Connected to {} again.", lnk.peer.device_name),
                        ));
                    }
                    notified = false;
                    backoff = BACKOFF_MIN;
                    shared.status.update(|s| {
                        s.link = LinkState::Connected;
                        s.peer = Some(PeerStatus {
                            device_id: lnk.peer.device_id.clone(),
                            device_name: lnk.peer.device_name.clone(),
                            device_type: lnk.peer.device_type.clone(),
                            address: addr.to_string(),
                            rtt_ms: 0.0,
                            connected_secs: 0,
                        });
                    });
                    shared.set_link(Some(lnk.clone()));
                    current = Some(Current { session_id, link: lnk, preferred, task });
                }
                Event::SessionEnded { session_id, reason } => {
                    if current.as_ref().is_some_and(|c| c.session_id == session_id) {
                        tracing::warn!(session_id, %reason, "link lost; SEARCHING");
                        current = None;
                        shared.set_link(None);
                        shared.status.update(|s| { s.link = LinkState::Searching; s.peer = None; });
                        down_since = Instant::now();
                        backoff = BACKOFF_MIN;
                        next_dial = Instant::now();
                    }
                }
                Event::PairingDone { stable_id } => {
                    if pairing_conn.as_ref().is_none_or(|pc| pc.conn.stable_id() != stable_id) {
                        continue;
                    }
                    pairing_conn = None;
                    shared.trust.close_pairing();
                    if current.is_none() {
                        shared.status.link(LinkState::Searching);
                    }
                    // Reconnect promptly with the freshly pinned key.
                    down_since = Instant::now();
                    backoff = BACKOFF_MIN;
                    next_dial = Instant::now() + Duration::from_millis(300);
                }
            },

            Some(p) = power_rx.recv() => {
                handle_power(&shared, p, &mut current, &discovery);
                if p == PowerEvent::Woke {
                    backoff = BACKOFF_MIN;
                    next_dial = Instant::now();
                    down_since = Instant::now();
                }
            }

            _ = tick.tick() => {
                // Portable sleep detection: the monotonic clock stops during
                // suspend on both Linux and macOS, the wall clock doesn't.
                let (mono0, wall0) = clock;
                let mono = mono0.elapsed();
                let wall = SystemTime::now().duration_since(wall0).unwrap_or_default();
                clock = (Instant::now(), SystemTime::now());
                if wall > mono + Duration::from_secs(5) {
                    tracing::info!(slept = ?(wall - mono), "wake detected (clock jump)");
                    let _ = power_tx.try_send(PowerEvent::Woke);
                }

                if let Some(cur) = &current {
                    let rtt = cur.link.clock.rtt_us() as f64 / 1000.0;
                    let rtt = if rtt > 0.0 { rtt } else { cur.link.conn.rtt().as_secs_f64() * 1000.0 };
                    let up = cur.link.since.elapsed().as_secs();
                    let addr = cur.link.conn.remote_address().to_string();
                    shared.status.update(|s| if let Some(p) = &mut s.peer {
                        p.rtt_ms = rtt;
                        p.connected_secs = up;
                        p.address = addr;
                    });
                    continue;
                }

                let paired = shared.trust.paired();
                if paired.is_none() && !unpaired_logged && !shared.pairing.is_active()
                    && down_since.elapsed() > Duration::from_secs(shared.cfg.not_found_grace_secs)
                {
                    tracing::warn!("not paired; run `linkerd pair` on both machines");
                    unpaired_logged = true; // log once, no banner nag when unpaired
                }

                if let Some(p) = &paired {
                    if !notified && down_since.elapsed() > Duration::from_secs(shared.cfg.not_found_grace_secs) {
                        notified = true;
                        tracing::warn!(peer = %p.device_name, "peer not found; will keep retrying quietly");
                        let n = own_notification(
                            "Linker: peer not found",
                            &format!("Can't reach {}. Linker keeps retrying in the background.", p.device_name),
                        );
                        if let Err(e) = shared.backend.show_notification(&n) {
                            tracing::warn!("could not show notification: {e:#}");
                        }
                    }
                }

                if dialing || pairing_conn.is_some() || Instant::now() < next_dial {
                    continue;
                }
                let targets = dial_targets(&me, paired.as_ref(), &candidates, &static_addrs,
                    shared.pairing.is_active(), down_since.elapsed() > LISTENER_FALLBACK);
                if targets.is_empty() {
                    continue;
                }
                dialing = true;
                shared.status.link(LinkState::Connecting);
                let shared = shared.clone();
                let endpoint = endpoint.clone();
                let ev_tx = ev_tx.clone();
                tokio::spawn(async move {
                    let ev = match dial(&shared, &endpoint, &targets).await {
                        Ok(ev) => ev,
                        Err(e) => Event::DialFailed(format!("{e:#}")),
                    };
                    let _ = ev_tx.send(ev).await;
                });
            }
        }
    }
}

fn handle_power(
    shared: &Shared,
    p: PowerEvent,
    current: &mut Option<Current>,
    discovery: &Discovery,
) {
    match p {
        PowerEvent::Sleeping => {
            tracing::info!("system going to sleep; closing link");
            if let Some(cur) = current.as_ref() {
                cur.link.conn.close(4u32.into(), b"sleep");
            }
        }
        PowerEvent::Woke => {
            tracing::info!("system woke; dropping stale link and searching");
            if let Some(cur) = current.as_ref() {
                // The session task will report SessionEnded.
                cur.link.conn.close(5u32.into(), b"wake");
            }
            discovery.refresh();
            shared.status.link(LinkState::Searching);
        }
    }
}

/// Addresses to dial now, honoring the tie-break.
fn dial_targets(
    me: &str,
    paired: Option<&PairedPeer>,
    candidates: &HashMap<String, Candidate>,
    static_addrs: &[SocketAddr],
    pairing: bool,
    listener_fallback: bool,
) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    match paired {
        Some(p) if !pairing => {
            if !(me < p.device_id.as_str() || listener_fallback) {
                return out;
            }
            if let Some(c) = candidates.get(&p.device_id) {
                out.extend(c.addrs.iter().copied());
            }
            out.extend(static_addrs.iter().copied());
        }
        _ if pairing => {
            for (id, c) in candidates {
                if me < id.as_str() || listener_fallback {
                    tracing::debug!(peer = %c.name, "pairing candidate");
                    out.extend(c.addrs.iter().copied());
                }
            }
            out.extend(static_addrs.iter().copied());
        }
        _ => {}
    }
    out.dedup();
    out
}

async fn dial(
    shared: &Shared,
    endpoint: &Endpoint,
    targets: &[SocketAddr],
) -> anyhow::Result<Event> {
    let mut last_err = anyhow::anyhow!("no addresses");
    for addr in targets {
        tracing::debug!(%addr, "dialing");
        let attempt = async {
            let conn = endpoint.connect(*addr, SERVER_NAME)?.await?;
            establish(shared, conn, true).await
        };
        match tokio::time::timeout(DIAL_TIMEOUT, attempt).await {
            Ok(Ok(ev)) => return Ok(ev),
            Ok(Err(e)) => last_err = e,
            Err(_) => last_err = anyhow::anyhow!("timeout dialing {addr}"),
        }
    }
    Err(last_err)
}

async fn accept_one(shared: &Shared, incoming: quinn::Incoming) -> anyhow::Result<Event> {
    let conn = incoming.await?;
    establish(shared, conn, false).await
}

/// Post-TLS: bind the Hello to the authenticated key and exchange Hellos.
async fn establish(shared: &Shared, conn: Connection, we_dialed: bool) -> anyhow::Result<Event> {
    let fp = peer_fingerprint(&conn)?;
    let (hello, send, recv) = link::handshake(&conn, we_dialed, &shared.hello()).await?;
    if hello.device_id != fp {
        conn.close(6u32.into(), b"identity mismatch");
        anyhow::bail!("peer Hello device_id does not match its TLS key");
    }
    if fp == shared.identity.device_id {
        conn.close(7u32.into(), b"self");
        anyhow::bail!("connected to ourselves");
    }
    Ok(Event::Established {
        conn,
        hello,
        send,
        recv,
        we_dialed,
    })
}

async fn run_pairing(
    shared: &Shared,
    conn: Connection,
    peer: Hello,
    mut send: SendStream,
    mut recv: RecvStream,
) -> anyhow::Result<()> {
    let code = sas_code(&shared.identity.device_id, &peer.device_id);
    tracing::info!(peer = %peer.device_name, "pairing: comparing code {code}");
    let decision = shared
        .pairing
        .present_sas(code, peer.device_name.clone(), peer.device_id.clone())
        .ok_or_else(|| anyhow::anyhow!("pairing was cancelled"))?;

    let local = tokio::time::timeout(PAIR_DECISION_TIMEOUT, decision)
        .await
        .unwrap_or(Ok(false))
        .unwrap_or(false);
    link::write_msg(
        &mut send,
        &Message::PairConfirm(PairConfirm { accepted: local }),
    )
    .await?;
    let remote = tokio::time::timeout(PAIR_DECISION_TIMEOUT, async {
        loop {
            match link::read_msg(&mut recv).await? {
                Some(Message::PairConfirm(c)) => return anyhow::Ok(c.accepted),
                Some(_) => continue,
                None => anyhow::bail!("peer closed before confirming"),
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for the other machine's confirmation"))??;

    let result = if local && remote {
        shared.trust.pin(PairedPeer {
            device_id: peer.device_id.clone(),
            device_name: peer.device_name.clone(),
            device_type: peer.device_type.clone(),
            paired_at_ms: now_ms(),
        })?;
        tracing::info!(peer = %peer.device_name, "paired");
        shared.pairing.finish(PairEvent::Paired {
            peer_name: peer.device_name.clone(),
        });
        Ok(())
    } else {
        let reason = if !local {
            "rejected on this machine"
        } else {
            "rejected on the other machine"
        };
        shared.pairing.finish(PairEvent::Failed {
            reason: reason.into(),
        });
        Ok(())
    };
    let _ = send.finish();
    // Give the confirm frame a moment to flush before closing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    conn.close(0u32.into(), b"pairing complete");
    result
}

pub fn own_notification(title: &str, body: &str) -> Notification {
    Notification {
        app: "Linker".into(),
        title: title.into(),
        subtitle: String::new(),
        body: body.into(),
        timestamp_ms: now_ms(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(id: &str) -> HashMap<String, Candidate> {
        let mut m = HashMap::new();
        m.insert(
            id.to_string(),
            Candidate {
                name: "x".into(),
                addrs: vec!["10.0.0.2:47800".parse().unwrap()],
            },
        );
        m
    }

    fn peer(id: &str) -> PairedPeer {
        PairedPeer {
            device_id: id.into(),
            device_name: "p".into(),
            device_type: "t".into(),
            paired_at_ms: 0,
        }
    }

    #[test]
    fn smaller_id_dials() {
        assert_eq!(
            dial_targets("aaa", Some(&peer("bbb")), &cands("bbb"), &[], false, false).len(),
            1
        );
        assert!(
            dial_targets("ccc", Some(&peer("bbb")), &cands("bbb"), &[], false, false).is_empty()
        );
    }

    #[test]
    fn listener_dials_after_fallback() {
        assert_eq!(
            dial_targets("ccc", Some(&peer("bbb")), &cands("bbb"), &[], false, true).len(),
            1
        );
    }

    #[test]
    fn unpaired_strangers_not_dialed_outside_pairing() {
        assert!(dial_targets("aaa", None, &cands("bbb"), &[], false, true).is_empty());
        assert!(
            dial_targets("aaa", Some(&peer("zzz")), &cands("bbb"), &[], false, false).is_empty()
        );
    }

    #[test]
    fn static_addresses_used() {
        let s: Vec<SocketAddr> = vec!["192.168.1.9:47800".parse().unwrap()];
        assert_eq!(
            dial_targets("aaa", Some(&peer("bbb")), &HashMap::new(), &s, false, false),
            s
        );
    }
}
