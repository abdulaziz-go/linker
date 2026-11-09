//! Notification mirroring + Do-Not-Disturb sync.
//!
//! DND conflict rule: each side tracks when its state last changed locally.
//! A received state is applied if it is newer; on an exact tie (e.g. both
//! unknown at first connect) the device with the smaller id wins. Applying a
//! remote state records it in an echo guard so the local watcher doesn't
//! bounce it back.

use std::sync::Arc;
use std::time::Duration;

use linker_platform::echo::EchoGuard;
use linker_platform::{now_ms, Notification};
use linker_proto::{Dnd, Feature, Message};
use tokio::sync::mpsc;

use crate::config::WhenDnd;
use crate::Shared;

/// Pure DND decision logic (unit-tested).
#[derive(Debug)]
pub struct DndState {
    pub local: Option<bool>,
    pub changed_at_ms: u64,
    guard: EchoGuard<bool>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DndAction {
    /// Apply this value locally.
    Apply(bool),
    /// Tell the peer our (newer) state.
    Reply(Dnd),
    Nothing,
}

impl DndState {
    pub fn new(local: Option<bool>) -> Self {
        DndState {
            local,
            changed_at_ms: 0,
            guard: EchoGuard::new(Duration::from_secs(5)),
        }
    }

    /// A local change was observed. Returns the message to send, if any.
    pub fn local_change(&mut self, on: bool, now_ms: u64) -> Option<Dnd> {
        if self.guard.is_echo(&on) {
            self.local = Some(on);
            return None;
        }
        if self.local == Some(on) {
            return None;
        }
        self.local = Some(on);
        self.changed_at_ms = now_ms;
        Some(Dnd {
            enabled: on,
            changed_at_ms: now_ms,
        })
    }

    pub fn remote(&mut self, d: Dnd, peer_wins_tie: bool) -> DndAction {
        let newer = d.changed_at_ms > self.changed_at_ms
            || (d.changed_at_ms == self.changed_at_ms && peer_wins_tie);
        if newer {
            self.changed_at_ms = d.changed_at_ms;
            if self.local != Some(d.enabled) {
                self.local = Some(d.enabled);
                self.guard.mark_applied(d.enabled);
                return DndAction::Apply(d.enabled);
            }
            DndAction::Nothing
        } else if let Some(local) = self.local {
            if local != d.enabled {
                DndAction::Reply(Dnd {
                    enabled: local,
                    changed_at_ms: self.changed_at_ms,
                })
            } else {
                DndAction::Nothing
            }
        } else {
            DndAction::Nothing
        }
    }

    /// The state to announce when a link comes up.
    pub fn announce(&self) -> Option<Dnd> {
        self.local.map(|enabled| Dnd {
            enabled,
            changed_at_ms: self.changed_at_ms,
        })
    }
}

pub async fn run(shared: Arc<Shared>, mut rx: mpsc::Receiver<Message>) {
    let ncfg = shared.cfg.notifications.clone();
    let dcfg = shared.cfg.dnd.clone();
    let me = shared.identity.device_id.clone();

    // Notification capture (only where the platform can).
    let (ntx, mut nrx) = mpsc::channel::<Notification>(64);
    if ncfg.enabled {
        let capture = if ncfg.send {
            match shared.backend.watch_notifications(ntx) {
                Ok(()) => "capturing + showing".to_string(),
                Err(e) => format!("show-only (capture: {})", e.0),
            }
        } else {
            "show-only".into()
        };
        shared.status.feature("notifications", capture);
    } else {
        shared.status.feature("notifications", "disabled");
    }

    // DND.
    let (dtx, mut drx) = mpsc::channel::<bool>(8);
    let mut dnd = DndState::new(None);
    if dcfg.enabled {
        let initial = shared.backend.get_dnd();
        let watch = shared.backend.watch_dnd(dtx);
        dnd.local = initial.as_ref().ok().copied();
        let health = match (&initial, &watch) {
            (Ok(_), Ok(())) => "active (bidirectional)".to_string(),
            (_, Err(e)) => format!("set-only — changes made here are not detected ({})", e.0),
            (Err(e), Ok(())) => format!("active (initial state unknown: {})", e.0),
        };
        shared.status.feature("dnd", health);
    } else {
        shared.status.feature("dnd", "disabled");
    }

    let mut link_rx = shared.watch_link();
    loop {
        tokio::select! {
            changed = link_rx.changed() => {
                if changed.is_err() { return; }
                let link = link_rx.borrow_and_update().clone();
                if let Some(link) = link {
                    if dcfg.enabled && link.peer_has(Feature::Dnd) {
                        if let Some(d) = dnd.announce() {
                            link.send(Message::Dnd(d));
                        }
                    }
                }
            }

            Some(n) = nrx.recv() => {
                if ncfg.ignore_apps.iter().any(|a| a.eq_ignore_ascii_case(&n.app)) {
                    continue;
                }
                if let Some(link) = shared.link() {
                    if link.peer_has(Feature::Notifications) {
                        tracing::debug!(app = %n.app, "mirroring notification");
                        link.send(Message::Notification(n));
                    }
                }
            }

            Some(on) = drx.recv(), if dcfg.enabled => {
                if let Some(d) = dnd.local_change(on, now_ms()) {
                    tracing::info!(on, "local DND changed");
                    if let Some(link) = shared.link() {
                        if link.peer_has(Feature::Dnd) {
                            link.send(Message::Dnd(d));
                        }
                    }
                }
            }

            Some(msg) = rx.recv() => match msg {
                Message::Notification(n) => {
                    if !ncfg.enabled || !ncfg.show {
                        continue;
                    }
                    if dnd.local == Some(true) {
                        match ncfg.when_dnd {
                            WhenDnd::Log => tracing::info!(app = %n.app, title = %n.title, "notification suppressed (DND)"),
                            WhenDnd::Drop => {}
                        }
                        continue;
                    }
                    if let Err(e) = shared.backend.show_notification(&n) {
                        tracing::warn!("showing notification failed: {e:#}");
                    }
                }
                Message::Dnd(d) if dcfg.enabled => {
                    let peer_wins = shared.link().is_some_and(|l| l.peer.device_id < me);
                    match dnd.remote(d, peer_wins) {
                        DndAction::Apply(on) => {
                            tracing::info!(on, "applying DND from peer");
                            let backend = shared.backend.clone();
                            match tokio::task::spawn_blocking(move || backend.set_dnd(on)).await {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => tracing::warn!("setting DND failed: {e:#}"),
                                Err(e) => tracing::warn!("DND task failed: {e}"),
                            }
                        }
                        DndAction::Reply(d) => {
                            if let Some(link) = shared.link() {
                                link.send(Message::Dnd(d));
                            }
                        }
                        DndAction::Nothing => {}
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_change_applies_and_does_not_echo() {
        let mut s = DndState::new(Some(false));
        assert_eq!(
            s.remote(
                Dnd {
                    enabled: true,
                    changed_at_ms: 10
                },
                false
            ),
            DndAction::Apply(true)
        );
        // Our watcher reports the change we just made: must not be re-sent.
        assert_eq!(s.local_change(true, 11), None);
    }

    #[test]
    fn genuine_local_change_is_sent() {
        let mut s = DndState::new(Some(false));
        assert_eq!(
            s.local_change(true, 5),
            Some(Dnd {
                enabled: true,
                changed_at_ms: 5
            })
        );
        assert_eq!(s.local_change(true, 6), None, "no duplicate sends");
    }

    #[test]
    fn older_remote_state_gets_reply() {
        let mut s = DndState::new(Some(false));
        s.local_change(true, 100);
        assert_eq!(
            s.remote(
                Dnd {
                    enabled: false,
                    changed_at_ms: 50
                },
                true
            ),
            DndAction::Reply(Dnd {
                enabled: true,
                changed_at_ms: 100
            })
        );
    }

    #[test]
    fn tie_resolved_by_device_id_without_swap() {
        // A (smaller id) has DND on, B off, both never changed (t=0).
        let mut a = DndState::new(Some(true));
        let mut b = DndState::new(Some(false));
        let from_a = a.announce().unwrap();
        let from_b = b.announce().unwrap();
        // On B, peer (A) wins the tie; on A, the peer (B) doesn't.
        assert_eq!(b.remote(from_a, true), DndAction::Apply(true));
        assert_eq!(
            a.remote(from_b, false),
            DndAction::Reply(Dnd {
                enabled: true,
                changed_at_ms: 0
            })
        );
        assert_eq!(a.local, Some(true));
        assert_eq!(b.local, Some(true));
    }

    #[test]
    fn no_ping_pong_over_many_rounds() {
        let mut a = DndState::new(Some(false));
        let mut b = DndState::new(Some(false));
        let mut wire = vec![(true, a.local_change(true, 1).unwrap())]; // (to_b, msg)
        let mut rounds = 0;
        while let Some((to_b, msg)) = wire.pop() {
            rounds += 1;
            assert!(rounds < 10, "ping-pong detected");
            let (me, peer_wins) = if to_b {
                (&mut b, true)
            } else {
                (&mut a, false)
            };
            match me.remote(msg, peer_wins) {
                DndAction::Apply(on) => {
                    // watcher fires after applying
                    if let Some(m) = me.local_change(on, 2) {
                        wire.push((!to_b, m));
                    }
                }
                DndAction::Reply(m) => wire.push((!to_b, m)),
                DndAction::Nothing => {}
            }
        }
        assert_eq!(a.local, Some(true));
        assert_eq!(b.local, Some(true));
    }
}
