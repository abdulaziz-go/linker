//! Deterministic network impairment simulator.
//!
//! No Go counterpart file: it generalises the impairments of audiobridge
//! `cmd/testtone` (simulate bad Wi-Fi: jitter, stalls, packet loss,
//! reordering) and the stall model of `jitter_test.go` into a virtual-time
//! component. Tests drive it with `t0 + Duration` instants; the daemon can
//! insert it into the real audio path via `LINKER_AUDIO_IMPAIR`
//! (e.g. `loss=5,reorder=10,jitter=5,stall_every=1500,stall_ms=100`).
//!
//! It uses its own tiny xorshift64* PRNG so the same [`Impair::seed`] always
//! gives the same packet fate, without depending on `rand`.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

/// Impairment settings. All-zero ([`Default`]) means a perfect network.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Impair {
    /// Packet loss, percent (0..=100).
    pub loss_pct: f64,
    /// Probability (percent) that a packet swaps places with the previous
    /// still-pending packet.
    pub reorder_pct: f64,
    /// Random extra delay, uniform in `[0, jitter_ms)`. Like testtone (a
    /// sleep inside the single send loop) jitter never reorders by itself:
    /// a packet is never due before its predecessor.
    pub jitter_ms: f64,
    /// Every `stall_every_ms` the link stalls (AWDL channel hop / Wi-Fi
    /// scan); 0 = never.
    pub stall_every_ms: u64,
    /// Stall length; packets sent during it are released together at its end.
    pub stall_ms: u64,
    /// Duplicate packets, percent.
    pub dup_pct: f64,
    /// PRNG seed.
    pub seed: u64,
}

/// testtone's `-stall-ms` default, used when `stall_every` is given alone.
const DEFAULT_STALL_MS: u64 = 100;

impl Impair {
    /// Parse `key=value` pairs separated by commas. Keys: `loss`, `reorder`,
    /// `jitter`, `stall_every`, `stall_ms`, `dup`, `seed`. An empty string is
    /// the default (no impairment); an unknown key or bad value is an error.
    /// `stall_every` without `stall_ms` uses 100 ms like testtone.
    pub fn parse(s: &str) -> Result<Impair, String> {
        let mut imp = Impair::default();
        let mut stall_ms_set = false;
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (k, v) = part
                .split_once('=')
                .ok_or_else(|| format!("impair: expected key=value, got {part:?}"))?;
            let (k, v) = (k.trim(), v.trim());
            let pct = |v: &str| -> Result<f64, String> {
                match v.parse::<f64>() {
                    Ok(x) if (0.0..=100.0).contains(&x) => Ok(x),
                    _ => Err(format!("impair: {k}={v:?} must be a percentage 0..100")),
                }
            };
            let int = |v: &str| -> Result<u64, String> {
                v.parse::<u64>()
                    .map_err(|_| format!("impair: {k}={v:?} must be a non-negative integer"))
            };
            match k {
                "loss" => imp.loss_pct = pct(v)?,
                "reorder" => imp.reorder_pct = pct(v)?,
                "dup" => imp.dup_pct = pct(v)?,
                "jitter" => {
                    imp.jitter_ms = match v.parse::<f64>() {
                        Ok(x) if x.is_finite() && x >= 0.0 => x,
                        _ => return Err(format!("impair: jitter={v:?} must be ms >= 0")),
                    }
                }
                "stall_every" => imp.stall_every_ms = int(v)?,
                "stall_ms" => {
                    imp.stall_ms = int(v)?;
                    stall_ms_set = true;
                }
                "seed" => imp.seed = int(v)?,
                _ => return Err(format!("impair: unknown key {k:?}")),
            }
        }
        if imp.stall_every_ms > 0 && !stall_ms_set {
            imp.stall_ms = DEFAULT_STALL_MS;
        }
        Ok(imp)
    }

    /// True when the settings do nothing (the simulator can be skipped).
    pub fn is_none(&self) -> bool {
        self.loss_pct <= 0.0
            && self.reorder_pct <= 0.0
            && self.jitter_ms <= 0.0
            && self.dup_pct <= 0.0
            && (self.stall_every_ms == 0 || self.stall_ms == 0)
    }
}

/// Inverse of [`Impair::parse`] (all keys, so it round-trips exactly).
impl fmt::Display for Impair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "loss={},reorder={},jitter={},stall_every={},stall_ms={},dup={},seed={}",
            self.loss_pct,
            self.reorder_pct,
            self.jitter_ms,
            self.stall_every_ms,
            self.stall_ms,
            self.dup_pct,
            self.seed
        )
    }
}

/// xorshift64* — tiny, fast, deterministic; plenty for impairment dice.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        // splitmix64 scramble so small / zero seeds still give a good,
        // non-zero state.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Rng(if z == 0 { 0x2545_F491_4F6C_DD1D } else { z })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, 1)`.
    fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// True with probability `pct` percent (like Go `rand.Float64()*100 < pct`).
    fn chance(&mut self, pct: f64) -> bool {
        pct > 0.0 && self.f64() * 100.0 < pct
    }
}

/// Packet counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimStats {
    /// Packets handed to [`NetSim::send`].
    pub sent: u64,
    /// Packets lost.
    pub dropped: u64,
    /// Extra copies created by `dup`.
    pub duplicated: u64,
    /// Packets (including duplicates) handed out by [`NetSim::poll`].
    pub delivered: u64,
}

/// Longest a reorder-held packet waits for a successor (testtone holds it
/// until the next send; this only matters at the end of a stream).
const HOLD_MAX: Duration = Duration::from_millis(20);

/// Delivery key: (due time, insertion order) — ties deliver in send order.
type Key = (Instant, u64);

/// The impaired link: [`send`](NetSim::send) packets in, [`poll`](NetSim::poll)
/// them out when due.
#[derive(Debug)]
pub struct NetSim {
    imp: Impair,
    rng: Rng,
    start: Option<Instant>,
    order: u64,
    pending: BTreeMap<Key, Vec<u8>>,
    /// With `reorder`, the most recent packet (and its due time) waits here
    /// for the next send, which may overtake it.
    /// The `bool` is the packet's own `dup` decision.
    held: Option<(Instant, Vec<u8>, bool)>,
    /// Due time of the previous packet (before reordering), so jitter keeps
    /// the stream in order like testtone's in-loop sleep.
    last_due: Option<Instant>,
    stats: SimStats,
}

impl NetSim {
    pub fn new(imp: Impair) -> NetSim {
        NetSim {
            rng: Rng::new(imp.seed),
            imp,
            start: None,
            order: 0,
            pending: BTreeMap::new(),
            held: None,
            last_due: None,
            stats: SimStats::default(),
        }
    }

    pub fn impair(&self) -> &Impair {
        &self.imp
    }

    fn next_key(&mut self, due: Instant) -> Key {
        self.order += 1;
        (due, self.order)
    }

    /// Put a packet on the link at `now` (the stall schedule starts at the
    /// first send). Fate, in order: loss → jitter delay → stall hold →
    /// duplicate → reorder (with `reorder` > 0 the
    /// packet is held until the next send, which may overtake it).
    pub fn send(&mut self, now: Instant, pkt: &[u8]) {
        self.stats.sent += 1;
        let start = *self.start.get_or_insert(now);
        if self.rng.chance(self.imp.loss_pct) {
            self.stats.dropped += 1;
            return;
        }
        let mut due = now;
        if self.imp.jitter_ms > 0.0 {
            // Like testtone: rand.IntN(jitter*1000) µs.
            let us = (self.rng.f64() * self.imp.jitter_ms * 1000.0) as u64;
            due += Duration::from_micros(us);
            // testtone sleeps before each write in one loop, so the stream
            // stays in order: never due before the previous packet.
            if let Some(last) = self.last_due {
                due = due.max(last);
            }
        }
        if self.imp.stall_every_ms > 0 && self.imp.stall_ms > 0 {
            let every = self.imp.stall_every_ms as u128 * 1_000_000;
            let stall = self.imp.stall_ms as u128 * 1_000_000;
            let phase = now.saturating_duration_since(start).as_nanos() % every;
            if phase < stall {
                // Held until the stall window ends, then released as a burst.
                let end = now + Duration::from_nanos((stall - phase) as u64);
                due = due.max(end);
            }
        }
        self.last_due = Some(due);
        let dup = self.rng.chance(self.imp.dup_pct);
        if self.imp.reorder_pct > 0.0 {
            // Like testtone: the last packet is held one step so the next
            // one can overtake it.
            let swap = self.rng.chance(self.imp.reorder_pct);
            if let Some((hdue, hpkt, hdup)) = self.held.take() {
                if swap {
                    self.insert(due.max(now), pkt.to_vec(), dup);
                    self.insert(hdue.max(now), hpkt, hdup);
                    return;
                }
                self.insert(hdue.max(now), hpkt, hdup);
            }
            self.held = Some((due, pkt.to_vec(), dup));
            return;
        }
        self.insert(due, pkt.to_vec(), dup);
    }

    fn insert(&mut self, due: Instant, pkt: Vec<u8>, dup: bool) {
        if dup {
            self.stats.duplicated += 1;
            let k = self.next_key(due);
            self.pending.insert(k, pkt.clone());
        }
        let k = self.next_key(due);
        self.pending.insert(k, pkt);
    }

    /// Release the held (reorder) packet once it is [`HOLD_MAX`] overdue, so
    /// the end of a stream is not stranded.
    fn release_held(&mut self, now: Instant) {
        if let Some((due, _, _)) = &self.held {
            let deadline = *due + HOLD_MAX;
            if deadline <= now {
                let (_, pkt, dup) = self.held.take().expect("checked");
                self.insert(deadline, pkt, dup);
            }
        }
    }

    /// Deliver every packet due at or before `now`, in delivery-time order
    /// (ties in send order).
    pub fn poll(&mut self, now: Instant, mut out: impl FnMut(&[u8])) {
        self.release_held(now);
        while let Some(entry) = self.pending.first_entry() {
            if entry.key().0 > now {
                break;
            }
            let pkt = entry.remove();
            self.stats.delivered += 1;
            out(&pkt);
        }
    }

    /// When the next packet becomes deliverable.
    pub fn next_due(&self) -> Option<Instant> {
        let p = self.pending.first_key_value().map(|(k, _)| k.0);
        let h = self.held.as_ref().map(|(d, _, _)| *d + HOLD_MAX);
        match (p, h) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Packets still in flight.
    pub fn in_flight(&self) -> usize {
        self.pending.len() + self.held.is_some() as usize
    }

    pub fn stats(&self) -> SimStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn idx(p: &[u8]) -> u32 {
        u32::from_le_bytes(p[..4].try_into().unwrap())
    }

    /// Send `n` packets (payload = index) every `period` from `t0`, polling
    /// as we go, then drain. Returns (index, delivery time) in order.
    fn run(imp: Impair, n: u32, period: Duration) -> (Vec<(u32, Instant)>, SimStats) {
        let mut sim = NetSim::new(imp);
        let t0 = Instant::now();
        let mut log = Vec::new();
        for i in 0..n {
            let now = t0 + period * i;
            // deliver what was due before this send, at its exact due time
            while let Some(d) = sim.next_due().filter(|&d| d <= now) {
                sim.poll(d, |p| log.push((idx(p), d)));
            }
            sim.send(now, &i.to_le_bytes());
        }
        while let Some(d) = sim.next_due() {
            sim.poll(d, |p| log.push((idx(p), d)));
        }
        (log, sim.stats())
    }

    #[test]
    fn parse_roundtrip_and_errors() {
        assert_eq!(Impair::parse("").unwrap(), Impair::default());
        assert!(Impair::parse(" ").unwrap().is_none());
        let imp =
            Impair::parse("loss=5,reorder=10,jitter=5,stall_every=1500,stall_ms=100,dup=1,seed=7")
                .unwrap();
        assert_eq!(
            imp,
            Impair {
                loss_pct: 5.0,
                reorder_pct: 10.0,
                jitter_ms: 5.0,
                stall_every_ms: 1500,
                stall_ms: 100,
                dup_pct: 1.0,
                seed: 7
            }
        );
        assert!(!imp.is_none());
        assert_eq!(Impair::parse(&imp.to_string()).unwrap(), imp);
        let frac = Impair::parse("jitter=2.5, loss=0.1").unwrap();
        assert_eq!(Impair::parse(&frac.to_string()).unwrap(), frac);
        assert_eq!(Impair::parse("stall_every=1000").unwrap().stall_ms, 100);

        for bad in [
            "bogus=1",
            "loss",
            "loss=x",
            "loss=101",
            "loss=-1",
            "jitter=-2",
            "stall_every=1.5",
            "seed=-1",
        ] {
            assert!(Impair::parse(bad).is_err(), "{bad:?} must fail");
        }
    }

    #[test]
    fn perfect_link_delivers_immediately_in_order() {
        let (log, st) = run(Impair::default(), 100, 5 * MS);
        assert_eq!(st.sent, 100);
        assert_eq!(st.delivered, 100);
        let t0 = log[0].1;
        for (i, &(k, t)) in log.iter().enumerate() {
            assert_eq!(k, i as u32);
            assert_eq!(t, t0 + 5 * MS * i as u32);
        }
    }

    #[test]
    fn loss_rate_close() {
        let imp = Impair {
            loss_pct: 5.0,
            seed: 1,
            ..Default::default()
        };
        let (log, st) = run(imp, 10_000, MS);
        assert_eq!(st.sent, 10_000);
        assert!((400..=600).contains(&st.dropped), "dropped {}", st.dropped);
        assert_eq!(st.delivered + st.dropped, st.sent);
        assert_eq!(log.len() as u64, st.delivered);
    }

    #[test]
    fn stall_releases_burst_at_window_end() {
        let imp = Impair {
            stall_every_ms: 1000,
            stall_ms: 100,
            ..Default::default()
        };
        let (log, _) = run(imp, 400, 5 * MS); // 2 s
        let t0 = log.iter().find(|e| e.0 == 0).unwrap().1 - 100 * MS;
        for &(i, t) in &log {
            let sent = 5 * i;
            let expect = if sent % 1000 < 100 {
                sent - sent % 1000 + 100
            } else {
                sent
            };
            assert_eq!(t, t0 + MS * expect, "packet {i}");
        }
        // 20 packets (0..95 ms) come out together at 100 ms, in send order.
        let burst: Vec<u32> = log
            .iter()
            .filter(|e| e.1 == t0 + 100 * MS)
            .map(|e| e.0)
            .collect();
        assert_eq!(burst, (0..21).collect::<Vec<_>>()); // + the one sent at 100 ms
        let burst2 = log.iter().filter(|e| e.1 == t0 + 1100 * MS).count();
        assert_eq!(burst2, 21);
        assert_eq!(log.len(), 400);
    }

    #[test]
    fn jitter_within_bounds() {
        let imp = Impair {
            jitter_ms: 5.0,
            seed: 3,
            ..Default::default()
        };
        let mut sim = NetSim::new(imp);
        let t0 = Instant::now();
        for i in 0..2000u32 {
            sim.send(t0 + MS * i, &i.to_le_bytes());
        }
        let mut n = 0;
        let mut delayed = 0;
        let mut last = t0;
        while let Some(d) = sim.next_due() {
            assert!(d >= last, "delivery time went backwards");
            last = d;
            sim.poll(d, |p| {
                let sent = t0 + MS * idx(p);
                assert!(
                    d >= sent && d < sent + 5 * MS,
                    "packet {} outside jitter",
                    idx(p)
                );
                if d > sent + MS {
                    delayed += 1;
                }
                n += 1;
            });
        }
        assert_eq!(n, 2000);
        assert!(delayed > 1000, "jitter barely applied: {delayed}");
    }

    #[test]
    fn deterministic_with_same_seed() {
        let imp =
            Impair::parse("loss=5,reorder=10,jitter=5,stall_every=300,stall_ms=50,dup=2,seed=7")
                .unwrap();
        let a = run(imp.clone(), 2000, 5 * MS);
        let b = run(imp.clone(), 2000, 5 * MS);
        let order = |l: &Vec<(u32, Instant)>| l.iter().map(|e| e.0).collect::<Vec<_>>();
        assert_eq!(order(&a.0), order(&b.0));
        assert_eq!(a.1, b.1);
        let c = run(Impair { seed: 8, ..imp }, 2000, 5 * MS);
        assert_ne!(order(&a.0), order(&c.0));
    }

    #[test]
    fn reorder_reorders_but_never_loses() {
        let imp = Impair {
            reorder_pct: 30.0,
            seed: 5,
            ..Default::default()
        };
        let mut sim = NetSim::new(imp);
        let t0 = Instant::now();
        let mut got = Vec::new();
        for i in 0..1000u32 {
            // Like testtone: the last packet is held one step, so all are
            // sent before anything is delivered at this instant.
            sim.send(t0 + MS * i, &i.to_le_bytes());
        }
        sim.poll(t0 + Duration::from_secs(10), |p| got.push(idx(p)));
        assert_eq!(got.len(), 1000);
        let inversions = got.windows(2).filter(|w| w[0] > w[1]).count();
        assert!(inversions > 100, "only {inversions} inversions");
        let mut sorted = got.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..1000).collect::<Vec<_>>());
        assert_eq!(sim.stats().delivered, 1000);
        assert_eq!(sim.in_flight(), 0);

        // Also with continuous polling (the daemon case): the held packet
        // lets its successor overtake it, and the tail is flushed.
        let imp = Impair {
            reorder_pct: 30.0,
            seed: 6,
            ..Default::default()
        };
        let (log, st) = run(imp, 1000, 5 * MS);
        let got: Vec<u32> = log.iter().map(|e| e.0).collect();
        assert!(got.windows(2).filter(|w| w[0] > w[1]).count() > 100);
        let mut sorted = got.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..1000).collect::<Vec<_>>());
        assert_eq!(st.delivered, 1000);
    }

    #[test]
    fn dup_creates_extra_copies() {
        let imp = Impair {
            dup_pct: 10.0,
            seed: 9,
            ..Default::default()
        };
        let (log, st) = run(imp, 5000, MS);
        assert!(
            (350..=650).contains(&st.duplicated),
            "dup {}",
            st.duplicated
        );
        assert_eq!(st.delivered, 5000 + st.duplicated);
        assert_eq!(log.len() as u64, st.delivered);
    }

    /// Regression: with `reorder` on, the held packet's `dup` decision used
    /// to be discarded when the next send flushed it.
    #[test]
    fn dup_with_reorder() {
        let imp = Impair {
            dup_pct: 10.0,
            reorder_pct: 10.0,
            seed: 9,
            ..Default::default()
        };
        let (log, st) = run(imp, 5000, MS);
        assert!(
            (350..=650).contains(&st.duplicated),
            "dup {}",
            st.duplicated
        );
        assert_eq!(st.delivered, 5000 + st.duplicated);
        assert_eq!(log.len() as u64, st.delivered);
    }

    /// Jitter larger than the send interval delays but never reorders
    /// (testtone sleeps inside its single send loop).
    #[test]
    fn jitter_alone_never_reorders() {
        let imp = Impair {
            jitter_ms: 20.0,
            seed: 4,
            ..Default::default()
        };
        let (log, st) = run(imp, 2000, 5 * MS);
        assert_eq!(st.delivered, 2000);
        let got: Vec<u32> = log.iter().map(|e| e.0).collect();
        assert_eq!(got, (0..2000).collect::<Vec<_>>());
        // ... yet the delay really varies (not a constant shift).
        let gaps: std::collections::BTreeSet<Duration> =
            log.windows(2).map(|w| w[1].1 - w[0].1).collect();
        assert!(gaps.len() > 100, "only {} distinct gaps", gaps.len());
    }
}
