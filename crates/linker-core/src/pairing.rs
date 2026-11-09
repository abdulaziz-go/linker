//! First-time pairing with a short authentication string (SAS).
//!
//! 1. The user runs `linkerd pair` on both machines; each daemon opens a
//!    pairing window (2 min) during which the TLS layer accepts an unknown key.
//! 2. Once a TLS session with an unpaired device is up, both sides derive the
//!    same 6-digit code from the two key fingerprints and show it.
//! 3. The user confirms the codes match on each machine. Each side sends
//!    `PairConfirm`; only if *both* accepted does each side pin the other's
//!    fingerprint. A man-in-the-middle would hold a different key on each leg
//!    and therefore produce different codes on the two screens.

use std::sync::Mutex;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

pub const PAIRING_WINDOW: Duration = Duration::from_secs(120);

/// 6-digit SAS derived from both key fingerprints (order-independent).
pub fn sas_code(fp_a: &str, fp_b: &str) -> String {
    let (lo, hi) = if fp_a <= fp_b {
        (fp_a, fp_b)
    } else {
        (fp_b, fp_a)
    };
    let mut h = Sha256::new();
    h.update(b"linker-sas-v1\0");
    h.update(lo.as_bytes());
    h.update([0]);
    h.update(hi.as_bytes());
    let d = h.finalize();
    let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PairEvent {
    Waiting,
    Sas {
        code: String,
        peer_name: String,
        peer_id: String,
    },
    Paired {
        peer_name: String,
    },
    Failed {
        reason: String,
    },
}

struct Active {
    events: mpsc::UnboundedSender<PairEvent>,
    decision: Option<oneshot::Sender<bool>>,
    /// Code already shown to the user (a duplicate connection with the same
    /// peer yields the same code and must not prompt twice).
    presented: Option<String>,
    /// The user's answer for `presented`, once given.
    decided: Option<bool>,
}

/// Coordinates the interactive CLI and the pairing session.
#[derive(Default)]
pub struct Pairing {
    active: Mutex<Option<Active>>,
}

impl Pairing {
    /// Start a pairing attempt (replacing any previous one).
    pub fn begin(&self) -> mpsc::UnboundedReceiver<PairEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(PairEvent::Waiting);
        *self.active.lock().expect("pairing lock") = Some(Active {
            events: tx,
            decision: None,
            presented: None,
            decided: None,
        });
        rx
    }

    pub fn is_active(&self) -> bool {
        self.active.lock().expect("pairing lock").is_some()
    }

    /// Show the SAS to the user; returns a receiver for their decision.
    pub fn present_sas(
        &self,
        code: String,
        peer_name: String,
        peer_id: String,
    ) -> Option<oneshot::Receiver<bool>> {
        let mut g = self.active.lock().expect("pairing lock");
        let a = g.as_mut()?;
        let (tx, rx) = oneshot::channel();
        if a.presented.as_deref() == Some(code.as_str()) {
            if let Some(d) = a.decided {
                let _ = tx.send(d);
                return Some(rx);
            }
        } else {
            a.presented = Some(code.clone());
            a.decided = None;
            let _ = a.events.send(PairEvent::Sas {
                code,
                peer_name,
                peer_id,
            });
        }
        a.decision = Some(tx);
        Some(rx)
    }

    /// The local user's answer. Returns false if nothing was waiting.
    pub fn decide(&self, accept: bool) -> bool {
        let mut g = self.active.lock().expect("pairing lock");
        let Some(a) = g.as_mut() else { return false };
        if a.presented.is_none() {
            return false;
        }
        a.decided = Some(accept);
        if let Some(tx) = a.decision.take() {
            let _ = tx.send(accept);
        }
        true
    }

    /// End the attempt with a final event.
    pub fn finish(&self, ev: PairEvent) {
        if let Some(a) = self.active.lock().expect("pairing lock").take() {
            let _ = a.events.send(ev);
        }
    }

    pub fn cancel(&self) {
        self.active.lock().expect("pairing lock").take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sas_is_symmetric_and_six_digits() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let s = sas_code(&a, &b);
        assert_eq!(s, sas_code(&b, &a));
        assert_eq!(s.len(), 7);
        assert!(s.chars().filter(|c| *c != ' ').all(|c| c.is_ascii_digit()));
        assert_ne!(s, sas_code(&a, &"c".repeat(64)));
    }

    #[tokio::test]
    async fn decision_flow() {
        let p = Pairing::default();
        let mut ev = p.begin();
        assert_eq!(ev.recv().await, Some(PairEvent::Waiting));
        let rx = p
            .present_sas("123 456".into(), "mac".into(), "id".into())
            .unwrap();
        assert!(matches!(ev.recv().await, Some(PairEvent::Sas { .. })));
        assert!(p.decide(true));
        assert!(rx.await.unwrap());
        p.finish(PairEvent::Paired {
            peer_name: "mac".into(),
        });
        assert!(matches!(ev.recv().await, Some(PairEvent::Paired { .. })));
        assert!(!p.is_active());
    }

    #[tokio::test]
    async fn decision_survives_duplicate_connection_swap() {
        let p = Pairing::default();
        let mut ev = p.begin();
        ev.recv().await;
        let _first = p
            .present_sas("111 222".into(), "mac".into(), "id".into())
            .unwrap();
        ev.recv().await;
        assert!(p.decide(false));
        // The preferred connection arrives after the user answered.
        let second = p
            .present_sas("111 222".into(), "mac".into(), "id".into())
            .unwrap();
        assert!(!second.await.unwrap());
        assert!(ev.try_recv().is_err(), "user is not prompted twice");
    }
}
