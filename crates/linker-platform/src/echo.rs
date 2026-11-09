//! Echo / loop prevention.
//!
//! When a device applies content it *received* from the peer (clipboard
//! payload, DND state), its own watcher will observe that change a moment
//! later. Without suppression it would send it straight back, and the two
//! machines would ping-pong forever. The guard records what was just applied
//! and tells the watcher to ignore the matching change.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Remembers recently applied fingerprints for `ttl`.
#[derive(Debug)]
pub struct EchoGuard<K: PartialEq + Clone> {
    ttl: Duration,
    recent: VecDeque<(K, Instant)>,
    cap: usize,
}

impl<K: PartialEq + Clone> EchoGuard<K> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            recent: VecDeque::new(),
            cap: 32,
        }
    }

    /// Record that `key` was just applied from the remote side.
    pub fn mark_applied(&mut self, key: K) {
        self.mark_applied_at(key, Instant::now());
    }

    pub fn mark_applied_at(&mut self, key: K, now: Instant) {
        self.expire(now);
        self.recent.push_back((key, now));
        while self.recent.len() > self.cap {
            self.recent.pop_front();
        }
    }

    /// Should a locally observed change with `key` be ignored (because we
    /// caused it)? Consumes the record so a later genuine re-copy of the same
    /// content by the user is still propagated.
    pub fn is_echo(&mut self, key: &K) -> bool {
        self.is_echo_at(key, Instant::now())
    }

    pub fn is_echo_at(&mut self, key: &K, now: Instant) -> bool {
        self.expire(now);
        if let Some(pos) = self.recent.iter().position(|(k, _)| k == key) {
            self.recent.remove(pos);
            true
        } else {
            false
        }
    }

    /// Was `key` applied recently (non-consuming)?
    pub fn contains(&self, key: &K) -> bool {
        let now = Instant::now();
        self.recent
            .iter()
            .any(|(k, t)| k == key && now.duration_since(*t) <= self.ttl)
    }

    fn expire(&mut self, now: Instant) {
        while let Some((_, t)) = self.recent.front() {
            if now.duration_since(*t) > self.ttl {
                self.recent.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applied_change_is_suppressed_once() {
        let mut g = EchoGuard::new(Duration::from_secs(5));
        g.mark_applied([1u8; 32]);
        assert!(g.is_echo(&[1u8; 32]));
        // A second, genuine copy of the same content later is NOT an echo.
        assert!(!g.is_echo(&[1u8; 32]));
    }

    #[test]
    fn other_content_is_not_an_echo() {
        let mut g = EchoGuard::new(Duration::from_secs(5));
        g.mark_applied([1u8; 32]);
        assert!(!g.is_echo(&[2u8; 32]));
        assert!(g.contains(&[1u8; 32]));
    }

    #[test]
    fn records_expire() {
        let mut g = EchoGuard::new(Duration::from_millis(100));
        let t0 = Instant::now();
        g.mark_applied_at(true, t0);
        assert!(!g.is_echo_at(&true, t0 + Duration::from_millis(500)));
    }

    #[test]
    fn simulated_two_device_ping_pong_terminates() {
        // Device A copies X. B receives and applies it; B's watcher sees X.
        let mut guard_b = EchoGuard::new(Duration::from_secs(5));
        let x = [7u8; 32];
        guard_b.mark_applied(x); // B applied remote content
        let b_would_send = !guard_b.is_echo(&x); // B's watcher fires
        assert!(!b_would_send, "B must not bounce X back to A");
    }

    #[test]
    fn capacity_is_bounded() {
        let mut g = EchoGuard::new(Duration::from_secs(60));
        for i in 0..100u32 {
            g.mark_applied(i);
        }
        assert!(!g.contains(&0));
        assert!(g.contains(&99));
    }
}
