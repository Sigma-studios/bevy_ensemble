//! Backend-agnostic network-condition simulation ("netsim").
//!
//! Gated by the `netdebug` feature (on by default). The simulator impairs the **inbound**
//! packet path — the single point [`decode_ensemble_packet`](crate::decode_ensemble_packet)
//! that every backend calls when bytes arrive. Delaying, dropping, jittering and
//! duplicating inbound packets on each peer reproduces a symmetric bad link: when
//! both ends impair their inbound path, a one-way `delay_ms` shows up as roughly
//! `2 * delay_ms` of round-trip time in the existing ping/RTT machinery, packet loss
//! is bidirectional, and so on. Impairing one direction at one clearly-defined seam
//! keeps this free of any backend cooperation.
//!
//! It stays completely inert until [`NetDebugPlugin`](crate::NetDebugPlugin) installs the
//! [`NetSim`] resource *and* a non-[`Off`](NetPreset::Off) preset is selected, so shipping
//! it in a release build costs nothing until someone deliberately turns it on.
//!
//! Timing uses Bevy's [`Time`] clock (works natively and on WASM); randomness uses a
//! small seeded xorshift PRNG so a given preset produces a repeatable trace — the
//! point of the tool is reproducing bugs, not surprising you with fresh ones.

use bevy::prelude::*;

use crate::{PlayerUUID, registry::decode_ensemble_packet_now};

/// Named network-condition presets. Values are **one-way** inbound impairments;
/// remember round-trip latency is roughly double `delay_ms` because both peers
/// impair their own inbound path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetPreset {
    /// No impairment — packets pass straight through (zero added cost).
    Off,
    /// Wired broadband: low latency, negligible loss.
    Cable,
    /// Typical mobile 4G: moderate latency and jitter.
    FourG,
    /// Congested wifi: high jitter, noticeable loss, occasional duplicates.
    BadWifi,
    /// Geostationary satellite: very high latency.
    Satellite,
}

impl NetPreset {
    /// All presets in display order, for the overlay's preset row.
    pub const ALL: [NetPreset; 5] = [
        NetPreset::Off,
        NetPreset::Cable,
        NetPreset::FourG,
        NetPreset::BadWifi,
        NetPreset::Satellite,
    ];

    /// Short label for the overlay button.
    pub fn label(self) -> &'static str {
        match self {
            NetPreset::Off => "Off",
            NetPreset::Cable => "Cable",
            NetPreset::FourG => "4G",
            NetPreset::BadWifi => "Bad wifi",
            NetPreset::Satellite => "Satellite",
        }
    }

    /// The impairment knobs for this preset.
    pub fn config(self) -> NetSimConfig {
        match self {
            NetPreset::Off => NetSimConfig::OFF,
            NetPreset::Cable => NetSimConfig {
                delay_ms: 15.0,
                jitter_ms: 3.0,
                loss: 0.001,
                duplicate: 0.0,
            },
            NetPreset::FourG => NetSimConfig {
                delay_ms: 40.0,
                jitter_ms: 15.0,
                loss: 0.005,
                duplicate: 0.0,
            },
            NetPreset::BadWifi => NetSimConfig {
                delay_ms: 60.0,
                jitter_ms: 40.0,
                loss: 0.03,
                duplicate: 0.01,
            },
            NetPreset::Satellite => NetSimConfig {
                delay_ms: 300.0,
                jitter_ms: 20.0,
                loss: 0.01,
                duplicate: 0.0,
            },
        }
    }
}

/// The tunable impairment knobs applied to inbound packets.
#[derive(Debug, Clone, Copy)]
pub struct NetSimConfig {
    /// Base one-way delay added to every packet, milliseconds.
    pub delay_ms: f32,
    /// Extra uniform random delay in `[0, jitter_ms)` added per packet, milliseconds.
    /// Because it is sampled per packet, it also reorders the unreliable stream.
    pub jitter_ms: f32,
    /// Probability in `[0, 1]` that a packet is dropped outright.
    pub loss: f32,
    /// Probability in `[0, 1]` that a delivered packet is also duplicated.
    pub duplicate: f32,
}

impl NetSimConfig {
    /// A completely transparent configuration.
    pub const OFF: NetSimConfig = NetSimConfig {
        delay_ms: 0.0,
        jitter_ms: 0.0,
        loss: 0.0,
        duplicate: 0.0,
    };
}

/// One inbound packet held back until its scheduled release time.
struct Delayed {
    release: f64,
    sender: Option<PlayerUUID>,
    bytes: Vec<u8>,
}

/// The network simulator resource. Present only under `netdebug`; defaults to
/// [`NetPreset::Off`], i.e. a single branch and no work until you pick a preset.
#[derive(Resource)]
pub struct NetSim {
    /// Currently selected preset (drives [`NetSim::config`]).
    pub preset: NetPreset,
    config: NetSimConfig,
    queue: Vec<Delayed>,
    rng: u64,
}

impl Default for NetSim {
    fn default() -> Self {
        Self {
            preset: NetPreset::Off,
            config: NetSimConfig::OFF,
            queue: Vec::new(),
            // Fixed non-zero seed → reproducible impairment traces across runs.
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

impl NetSim {
    /// Switch to a preset, applying its knobs. In-flight (already-delayed) packets
    /// keep their existing schedule; only newly-arriving packets see the new config.
    pub fn set_preset(&mut self, preset: NetPreset) {
        self.preset = preset;
        self.config = preset.config();
    }

    /// The active impairment configuration.
    pub fn config(&self) -> NetSimConfig {
        self.config
    }

    /// Whether the simulator is doing anything at all.
    pub fn is_active(&self) -> bool {
        self.preset != NetPreset::Off
    }

    /// xorshift64 — cheap, deterministic, good enough for impairment sampling.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Uniform `f32` in `[0, 1)`.
    fn roll(&mut self) -> f32 {
        // Top 24 bits → 24-bit mantissa precision, plenty for probabilities.
        (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32
    }

    /// Drain and return every packet whose release time has passed, oldest first.
    /// Sorting by release time is what turns per-packet jitter into real reordering.
    fn take_due(&mut self, now: f64) -> Vec<(Option<PlayerUUID>, Vec<u8>)> {
        let mut due: Vec<Delayed> = Vec::new();
        let mut i = 0;
        while i < self.queue.len() {
            if self.queue[i].release <= now {
                due.push(self.queue.swap_remove(i));
            } else {
                i += 1;
            }
        }
        due.sort_by(|a, b| a.release.partial_cmp(&b.release).unwrap_or(std::cmp::Ordering::Equal));
        due.into_iter().map(|d| (d.sender, d.bytes)).collect()
    }
}

/// Result of offering an inbound packet to the simulator.
pub(crate) enum SimVerdict {
    /// Simulator is off — decode the packet immediately as usual.
    PassThrough,
    /// Packet was dropped by simulated loss.
    Dropped,
    /// Packet was queued for later delivery (with `duplicated` extra copies).
    Delayed { duplicated: u32 },
}

/// Offer an inbound packet to the simulator. Called from `decode_ensemble_packet`.
///
/// Returns a verdict the caller uses only to update counters; the packet bytes are
/// taken ownership of here when delayed, and later replayed by [`drain_netsim`].
pub(crate) fn offer_inbound(world: &mut World, sender: Option<PlayerUUID>, packet: &[u8]) -> SimVerdict {
    let Some(now) = world
        .get_resource::<Time>()
        .map(|t| t.elapsed_secs_f64())
    else {
        return SimVerdict::PassThrough;
    };

    let Some(mut sim) = world.get_resource_mut::<NetSim>() else {
        return SimVerdict::PassThrough;
    };
    if !sim.is_active() {
        return SimVerdict::PassThrough;
    }

    let config = sim.config;

    if sim.roll() < config.loss {
        return SimVerdict::Dropped;
    }

    let schedule = |sim: &mut NetSim| {
        let jitter = if config.jitter_ms > 0.0 {
            sim.roll() * config.jitter_ms
        } else {
            0.0
        };
        now + ((config.delay_ms + jitter) as f64) / 1000.0
    };

    let release = schedule(&mut sim);
    sim.queue.push(Delayed {
        release,
        sender,
        bytes: packet.to_vec(),
    });

    let mut duplicated = 0;
    if config.duplicate > 0.0 && sim.roll() < config.duplicate {
        let release = schedule(&mut sim);
        sim.queue.push(Delayed {
            release,
            sender,
            bytes: packet.to_vec(),
        });
        duplicated = 1;
    }

    SimVerdict::Delayed { duplicated }
}

/// Exclusive system: replay every inbound packet whose delay has elapsed.
pub(crate) fn drain_netsim(world: &mut World) {
    let now = world.resource::<Time>().elapsed_secs_f64();
    let due = world.resource_mut::<NetSim>().take_due(now);
    for (sender, bytes) in due {
        decode_ensemble_packet_now(world, sender, &bytes);
    }
}
