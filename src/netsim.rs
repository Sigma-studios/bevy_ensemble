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
//! Timing reads the [`NetSimClock`] resource, which defaults to Bevy's [`Time`] (works
//! natively and on WASM) and can be switched to a manually advanced clock; randomness
//! uses a small seeded xorshift PRNG so a given preset produces a repeatable trace — the
//! point of the tool is reproducing bugs, not surprising you with fresh ones.
//!
//! # What this does and does not model
//!
//! The seam is *below* the reliability distinction and *above* the transport's work to
//! honour it. Both backends merge their channels before they get here — Steam receives
//! everything on channel 0, and the WebRTC socket's `receive()` returns one queue for
//! both data channels — so netsim cannot read a packet's
//! [`SendMode`](crate::SendMode) and has to be told which channel to assume.
//!
//! [`ChannelModel`] is that switch, and picking it wrong is the easiest way to
//! misread a result. The default, [`Unreliable`](ChannelModel::Unreliable), models a wire:
//! packets overtake each other, `loss` drops them and `duplicate` delivers them twice.
//! Run a reliable protocol under it and you will watch it fail on impairments its
//! transport would have absorbed — a lost packet that the real channel simply resends.
//! [`Reliable`](ChannelModel::Reliable) models what the transport hands the decode seam
//! instead: order preserved per sender, loss costing a retransmit round trip, duplicates
//! gone. The presets are one-way wire conditions either way; the model decides what the
//! layer above does with them.

use bevy::prelude::*;

use crate::{PlayerUUID, registry::decode_ensemble_packet_now};

/// Installs the simulator and its drain system, and nothing else.
///
/// [`NetDebugPlugin`](crate::NetDebugPlugin) adds this for you alongside the F3 overlay.
/// Add it directly when you want impairment without any UI — a headless test or a
/// dedicated server, neither of which can spawn the overlay's nodes.
pub struct NetSimPlugin;

impl Plugin for NetSimPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NetSim>()
            .init_resource::<NetSimClock>()
            // Replays delayed packets — another inbound seam, so it joins the backend
            // receive systems in PreUpdate.
            .add_systems(
                PreUpdate,
                drain_netsim.in_set(crate::EnsembleSet::ReceivePackets),
            );
    }
}

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

/// Which kind of channel the simulator pretends inbound packets arrived on.
///
/// Netsim sits at the decode seam, behind the point where both backends merge their
/// channels into one receive queue — Steam takes everything off channel 0, and the WebRTC
/// socket's `receive()` returns one stream for both data channels. So it cannot read a
/// packet's [`SendMode`](crate::SendMode) and has to be told which one to assume.
///
/// Pick the one that matches the traffic you care about. A game whose protocol is entirely
/// reliable (anything lockstep) wants [`Reliable`](ChannelModel::Reliable); one being
/// tested for how it copes with genuine packet loss wants
/// [`Unreliable`](ChannelModel::Unreliable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelModel {
    /// A wire. Jitter is sampled per packet, so packets overtake each other; `loss` drops
    /// them outright and `duplicate` delivers them twice. The default, and what an
    /// unreliable channel really does.
    #[default]
    Unreliable,
    /// A reliable-ordered channel, as the transport presents it to the decode seam.
    ///
    /// A sender's packets are released in the order they arrived, so jitter becomes
    /// head-of-line delay rather than reordering. `loss` costs a retransmit — one round
    /// trip added to that packet, and to everything queued behind it — instead of losing
    /// the message. `duplicate` is ignored, because the transport deduplicates.
    ///
    /// Ordering is per sender: nothing in either transport orders one peer's stream
    /// against another's.
    Reliable,
}

/// The clock netsim schedules against.
///
/// Defaults to [`Time`], which is what a running game wants. Tests select
/// [`Manual`](NetSimClock::Manual) and advance it themselves, so a 40ms delay is exactly
/// 40ms of simulated time instead of however long the frame happened to take — which is
/// what makes a netsim-driven test reproduce rather than merely usually pass.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub enum NetSimClock {
    /// Read elapsed seconds from Bevy's [`Time`].
    #[default]
    Time,
    /// A manually advanced clock, in seconds from an arbitrary origin.
    Manual(f64),
}

impl NetSimClock {
    /// Advance a manual clock by `secs`. Does nothing to [`NetSimClock::Time`], which
    /// advances on its own.
    pub fn advance(&mut self, secs: f64) {
        if let NetSimClock::Manual(now) = self {
            *now += secs;
        }
    }

    /// Switch to a manual clock reading `secs`.
    pub fn set(&mut self, secs: f64) {
        *self = NetSimClock::Manual(secs);
    }
}

/// Scheduling "now": the manual clock when one is installed, otherwise [`Time`].
///
/// Returns `None` only when neither resource exists, which leaves the simulator inert
/// rather than guessing at a time base.
fn clock_now(world: &World) -> Option<f64> {
    match world.get_resource::<NetSimClock>() {
        Some(NetSimClock::Manual(now)) => Some(*now),
        _ => world.get_resource::<Time>().map(|t| t.elapsed_secs_f64()),
    }
}

/// One inbound packet held back until its scheduled release time.
struct Delayed {
    release: f64,
    /// Insertion order, breaking release-time ties. `take_due` pulls packets out with
    /// `swap_remove`, so queue position is not insertion order and a stable sort alone
    /// would not give a repeatable drain.
    seq: u64,
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
    channel: ChannelModel,
    /// Latest release time scheduled per sender, for [`ChannelModel::Reliable`].
    /// A short association list: a session has a handful of peers, and unlike a
    /// hash map it cannot introduce iteration-order surprises.
    last_release: Vec<(Option<PlayerUUID>, f64)>,
    next_seq: u64,
}

impl Default for NetSim {
    fn default() -> Self {
        Self {
            preset: NetPreset::Off,
            config: NetSimConfig::OFF,
            queue: Vec::new(),
            // Fixed non-zero seed → reproducible impairment traces across runs.
            rng: 0x9E37_79B9_7F4A_7C15,
            channel: ChannelModel::Unreliable,
            last_release: Vec::new(),
            next_seq: 0,
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

    /// Which channel the simulator is treating every packet as having arrived on.
    pub fn channel_model(&self) -> ChannelModel {
        self.channel
    }

    /// Choose which channel the simulator models. See [`ChannelModel`].
    pub fn set_channel_model(&mut self, channel: ChannelModel) {
        self.channel = channel;
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

    /// Queue one packet at its scheduled release.
    ///
    /// Under [`ChannelModel::Reliable`] the release is clamped to this sender's previous
    /// one, so arrival order survives an unlucky jitter sample.
    fn enqueue(&mut self, sender: Option<PlayerUUID>, bytes: Vec<u8>, mut release: f64) {
        if self.channel == ChannelModel::Reliable {
            match self.last_release.iter_mut().find(|(s, _)| *s == sender) {
                Some((_, last)) => {
                    release = release.max(*last);
                    *last = release;
                }
                None => self.last_release.push((sender, release)),
            }
        }

        let seq = self.next_seq;
        self.next_seq += 1;
        self.queue.push(Delayed {
            release,
            seq,
            sender,
            bytes,
        });
    }

    /// Drain and return every packet whose release time has passed, oldest first.
    /// Sorting by release time is what turns per-packet jitter into real reordering;
    /// the `seq` tiebreak keeps packets clamped to the same release — and packets the
    /// caller queued at one instant — in the order they arrived.
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
        due.sort_by(|a, b| {
            a.release
                .partial_cmp(&b.release)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.seq.cmp(&b.seq))
        });
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
    let Some(now) = clock_now(world) else {
        return SimVerdict::PassThrough;
    };

    let Some(mut sim) = world.get_resource_mut::<NetSim>() else {
        return SimVerdict::PassThrough;
    };
    if !sim.is_active() {
        return SimVerdict::PassThrough;
    }

    let config = sim.config;
    let channel = sim.channel;

    // The roll happens either way, so the two channel models read the same PRNG stream.
    let lost = sim.roll() < config.loss;
    if lost && channel == ChannelModel::Unreliable {
        return SimVerdict::Dropped;
    }

    let schedule = |sim: &mut NetSim| {
        let jitter = if config.jitter_ms > 0.0 {
            sim.roll() * config.jitter_ms
        } else {
            0.0
        };
        // A reliable channel does not lose the message, it resends it — which costs a
        // round trip, and (via `enqueue`'s clamp) holds up everything behind it. Modelling
        // it as a drop would test a failure a reliable protocol cannot actually have.
        let retransmit = if lost { config.delay_ms * 2.0 } else { 0.0 };
        now + ((config.delay_ms + jitter + retransmit) as f64) / 1000.0
    };

    let release = schedule(&mut sim);
    sim.enqueue(sender, packet.to_vec(), release);

    let mut duplicated = 0;
    // A reliable channel deduplicates, so a duplicate never reaches the decode seam.
    if channel == ChannelModel::Unreliable && config.duplicate > 0.0 && sim.roll() < config.duplicate
    {
        let release = schedule(&mut sim);
        sim.enqueue(sender, packet.to_vec(), release);
        duplicated = 1;
    }

    SimVerdict::Delayed { duplicated }
}

/// Exclusive system: replay every inbound packet whose delay has elapsed.
pub(crate) fn drain_netsim(world: &mut World) {
    let Some(now) = clock_now(world) else {
        return;
    };
    let due = world.resource_mut::<NetSim>().take_due(now);
    for (sender, bytes) in due {
        decode_ensemble_packet_now(world, sender, &bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Queue `releases` in the order given, all from `sender`, then drain everything.
    /// Payloads are the arrival index, so the returned bytes spell out the delivery order.
    fn drain_in_order(sim: &mut NetSim, sender: Option<PlayerUUID>, releases: &[f64]) -> Vec<u8> {
        for (i, release) in releases.iter().enumerate() {
            sim.enqueue(sender, vec![i as u8], *release);
        }
        sim.take_due(f64::MAX).into_iter().map(|(_, b)| b[0]).collect()
    }

    #[test]
    fn a_manual_clock_wins_over_time() {
        let mut world = World::new();
        world.insert_resource(Time::<()>::default());
        world.insert_resource(NetSimClock::Manual(5.0));
        assert_eq!(clock_now(&world), Some(5.0));
    }

    #[test]
    fn without_a_manual_clock_time_is_the_clock() {
        let mut world = World::new();
        world.insert_resource(Time::<()>::default());

        // The default variant is `Time`, so installing the resource changes nothing.
        assert_eq!(clock_now(&world), Some(0.0));
        world.insert_resource(NetSimClock::default());
        assert_eq!(clock_now(&world), Some(0.0));
    }

    #[test]
    fn with_no_clock_at_all_the_simulator_stays_inert() {
        // Rather than substituting a zero and silently scheduling everything into the past.
        assert_eq!(clock_now(&World::new()), None);
    }

    #[test]
    fn advance_moves_a_manual_clock_and_leaves_time_alone() {
        let mut clock = NetSimClock::Manual(1.0);
        clock.advance(0.5);
        assert_eq!(clock, NetSimClock::Manual(1.5));

        let mut real = NetSimClock::Time;
        real.advance(0.5);
        assert_eq!(real, NetSimClock::Time, "`Time` advances itself");
        real.set(2.0);
        assert_eq!(real, NetSimClock::Manual(2.0));
    }

    #[test]
    fn jitter_reorders_a_wire() {
        // The default. Release time alone decides, so a packet that drew less jitter
        // overtakes the one in front of it -- what an unreliable channel really does.
        let mut sim = NetSim::default();
        assert_eq!(sim.channel_model(), ChannelModel::Unreliable);
        assert_eq!(drain_in_order(&mut sim, Some(1), &[0.3, 0.1, 0.2]), vec![1, 2, 0]);
    }

    #[test]
    fn a_reliable_channel_holds_a_sender_stream_in_arrival_order() {
        // The post-transport seam: a reliable channel would have delivered these in send
        // order however unlucky the jitter samples were.
        let mut sim = NetSim::default();
        sim.set_channel_model(ChannelModel::Reliable);
        assert_eq!(drain_in_order(&mut sim, Some(1), &[0.3, 0.1, 0.2]), vec![0, 1, 2]);
    }

    #[test]
    fn a_reliable_channel_delays_the_stream_behind_a_late_packet() {
        // Head-of-line blocking: the clamped packets inherit the leader's release, so
        // nothing behind a late packet arrives before it.
        let mut sim = NetSim::default();
        sim.set_channel_model(ChannelModel::Reliable);
        sim.enqueue(Some(1), vec![0], 1.0);
        sim.enqueue(Some(1), vec![1], 0.1);

        assert!(
            sim.take_due(0.5).is_empty(),
            "the second packet was released ahead of the one blocking it"
        );
        assert_eq!(sim.take_due(1.0).len(), 2);
    }

    #[test]
    fn order_is_kept_per_sender_not_globally() {
        // Nothing in either transport orders one peer's stream against another's.
        let mut sim = NetSim::default();
        sim.set_channel_model(ChannelModel::Reliable);
        sim.enqueue(Some(1), vec![0], 1.0);
        sim.enqueue(Some(2), vec![1], 0.5);

        let due: Vec<u8> = sim.take_due(0.6).into_iter().map(|(_, b)| b[0]).collect();
        assert_eq!(due, vec![1], "peer 2 was held back by a packet of peer 1's");
    }

    #[test]
    fn packets_sharing_a_release_time_drain_in_arrival_order() {
        // `take_due` pulls with `swap_remove`, so queue position is not arrival order;
        // without the sequence tiebreak this drains differently as the queue grows.
        let mut sim = NetSim::default();
        let order = drain_in_order(&mut sim, Some(1), &[0.1; 8]);
        assert_eq!(order, (0..8).collect::<Vec<u8>>());
    }

    /// Offer `count` packets to a world running `preset` under `channel`, and report how
    /// many survived to the queue.
    fn offer_many(preset: NetPreset, channel: ChannelModel, count: usize) -> (usize, usize) {
        let mut world = World::new();
        world.insert_resource(NetSimClock::Manual(0.0));
        let mut sim = NetSim::default();
        sim.set_preset(preset);
        sim.set_channel_model(channel);
        world.insert_resource(sim);

        let mut dropped = 0;
        let mut duplicated = 0;
        for _ in 0..count {
            match offer_inbound(&mut world, Some(1), &[0u8]) {
                SimVerdict::Dropped => dropped += 1,
                SimVerdict::Delayed { duplicated: d } => duplicated += d as usize,
                SimVerdict::PassThrough => unreachable!("the preset is active"),
            }
        }
        (dropped, duplicated)
    }

    #[test]
    fn a_reliable_channel_resends_rather_than_dropping() {
        // Bad wifi is 3% loss and 1% duplication. Neither reaches a reliable protocol:
        // the transport retransmits what it loses and deduplicates what it repeats.
        // Dropping here is what stalled a lockstep session that could never really stall.
        let (dropped, duplicated) = offer_many(NetPreset::BadWifi, ChannelModel::Reliable, 500);
        assert_eq!(dropped, 0, "a reliable channel does not lose messages");
        assert_eq!(duplicated, 0, "a reliable channel deduplicates");
    }

    #[test]
    fn an_unreliable_channel_really_does_drop() {
        // The other half of the contract: the default model still models a wire.
        let (dropped, duplicated) = offer_many(NetPreset::BadWifi, ChannelModel::Unreliable, 500);
        assert!(dropped > 0, "3% loss over 500 packets dropped none");
        assert!(duplicated > 0, "1% duplication over 500 packets duplicated none");
    }

    #[test]
    fn a_retransmitted_packet_costs_a_round_trip() {
        // Loss shows up as latency instead of absence, so a test can still tell the
        // difference between a clean link and a lossy one.
        let clean = |channel| {
            let mut world = World::new();
            world.insert_resource(NetSimClock::Manual(0.0));
            let mut sim = NetSim::default();
            // No jitter, so the only variation left is the retransmit penalty.
            sim.set_preset(NetPreset::Satellite);
            sim.config.jitter_ms = 0.0;
            sim.set_channel_model(channel);
            world.insert_resource(sim);
            for _ in 0..200 {
                offer_inbound(&mut world, Some(1), &[0u8]);
            }
            // One-way delay is 300ms; a resend adds a 600ms round trip on top.
            let sim = world.resource::<NetSim>();
            sim.queue.iter().filter(|d| d.release > 0.5).count()
        };
        assert!(
            clean(ChannelModel::Reliable) > 0,
            "1% loss over 200 packets never triggered a retransmit"
        );
        assert_eq!(
            clean(ChannelModel::Unreliable),
            0,
            "an unreliable channel drops instead of paying for a resend"
        );
    }

    #[test]
    fn a_preset_run_replays_exactly() {
        // The whole point of the seeded PRNG: same preset, same trace.
        let trace = || {
            let mut sim = NetSim::default();
            sim.set_preset(NetPreset::BadWifi);
            let config = sim.config();
            (0..64)
                .map(|_| (sim.roll() < config.loss, sim.roll() * config.jitter_ms))
                .collect::<Vec<_>>()
        };
        assert_eq!(trace(), trace());
    }
}
