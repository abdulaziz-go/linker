//! Linker's low-latency audio engine — a faithful Rust port of the Go
//! `audiobridge` project (see `docs/feature-audio.md`).
//!
//! Everything here is platform-independent and I/O-free: PCM is always
//! 48 kHz interleaved stereo `i16`, time is passed in as [`std::time::Instant`]
//! so the whole chain can be driven in virtual time by tests.

#[cfg(feature = "codec")]
pub mod codec;
pub mod frame;
pub mod jitter;
pub mod proto;
#[cfg(feature = "codec")]
pub mod receiver;
pub mod reorder;
#[cfg(feature = "codec")]
pub mod sender;
pub mod sim;

/// Sample rate of everything inside the engine.
pub const RATE: u32 = 48_000;
/// Interleaved channels of everything inside the engine.
pub const CHANNELS: usize = 2;
