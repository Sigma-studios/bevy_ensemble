//! An in-process backend, so a multiplayer session is something a test can assert about.
//!
//! Every other backend — `steam`, `webrtc`, `sockets` — is a real transport with real async and
//! real time. Testing two peers therefore meant two processes, two keyboards and a stopwatch,
//! which is why netcode tends to be the least-tested part of a game that depends on it entirely.
//!
//! This crate runs N apps in one process on a clock you control. It is a real backend, not a
//! mock: messages are postcard-encoded, routed by entity, and decoded through the registry, so a
//! type registered in the wrong order or a message that fails to round-trip fails here exactly as
//! it would on a socket.
//!
//! # The two seams
//!
//! The whole transport contract is two functions wide, and both are public:
//!
//! - **outbound:** [`SerializedLobbyPacket`] is triggered once a message has been encoded. On a
//!   host it fires on each [`LobbyClient`] entity, on a client on the [`Lobby`] entity — so the
//!   entity is the address. [`Outbox`] records it.
//! - **inbound:** [`decode_ensemble_packet`] turns bytes back into a `ReceivedEnsembleMessage<T>`.
//!   [`Inbox`] holds them and `drain_inbox` calls it from `PreUpdate` inside
//!   [`EnsembleSet::ReceivePackets`] — the slot a real backend uses, so `Update` readers see a
//!   packet on the frame it arrives rather than the frame after.
//!
//! [`LoopbackTransportPlugin`] is those two things and nothing else. [`LoopbackNetwork`] is the
//! part around them: peers, addressing, the link, and the frame loop.
//!
//! # How it models a bad connection
//!
//! [`Link`] delays packets by a whole number of frames and the network holds them until they come
//! due. Two decisions in it are worth stating, because getting them wrong would make the tests
//! lie:
//!
//! - **Reliable packets are never dropped.** A reliable transport retransmits rather than loses,
//!   so [`Link::loss`] on a `SendMode::Reliable` packet costs it an extra round trip — which is
//!   what a retransmission is — and only an `Unreliable` packet is genuinely discarded.
//! - **Per-link order is preserved.** Jitter moves a reliable packet's delivery later, never
//!   before one sent earlier on the same link. A reliable ordered channel does not reorder, and a
//!   test that saw reordering would be debugging the harness.
//!
//! It can make both of those calls because it impairs on the **send** side, where a packet still
//! carries its [`SendMode`]. That is the difference between it and `bevy_ensemble`'s `netsim`,
//! which impairs at the decode seam — behind the point where every backend has merged its
//! channels — and so has to be *told* which channel to assume. Use [`Link`] to model a link
//! whose two channels behave differently; use [`use_netsim`](LoopbackNetwork::use_netsim) to
//! exercise netsim's own queueing with the presets a game ships with. They compose, but by
//! default only one of them is doing anything.
//!
//! # Determinism
//!
//! No wall clock anywhere. One [`step`](LoopbackNetwork::step) is one frame on every peer, the
//! link's randomness is a seeded xorshift, and netsim runs on an injected
//! [`NetSimClock`](bevy_ensemble::NetSimClock) pinned to the frame counter. The same run twice
//! produces the same trace, which is the entire point — a flaky netcode test teaches nobody
//! anything.
//!
//! # Using it
//!
//! The caller builds each [`App`], because what a peer *is* belongs to the game. This crate only
//! wires them together.
//!
//! ```ignore
//! let mut network = LoopbackNetwork::new(TICK_DURATION);
//! let host = network.add_host(1, build_app(1));
//! let client = network.add_client(2, build_app(2));
//! network.set_link(Link::four_g());
//! for _ in 0..600 {
//!     network.step();
//! }
//! ```

use bevy::prelude::*;
use bevy_ensemble::{
    EnsembleSet, Host, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyParticipantOf, NetPreset,
    PeerRtt, PlayerUUID, SendMode, SerializedLobbyPacket, decode_ensemble_packet,
};
use std::time::Duration;

/// Identifies a peer inside a [`LoopbackNetwork`]. Index, not a uuid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(pub usize);

/// Packets this peer has encoded and not yet handed to the network.
///
/// `Entity` is the address: a [`LobbyClient`] on a host, the [`Lobby`] on a client.
#[derive(Resource, Default)]
pub struct Outbox(pub Vec<(Entity, Vec<u8>, SendMode)>);

/// Packets the network has delivered but the app has not yet decoded.
#[derive(Resource, Default)]
pub struct Inbox(pub Vec<(PlayerUUID, Vec<u8>)>);

/// The backend itself. Everything `bevy_ensemble` needs from a transport, and nothing else.
///
/// Add it to every app that joins a [`LoopbackNetwork`].
pub struct LoopbackTransportPlugin;

impl Plugin for LoopbackTransportPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Outbox>()
            .init_resource::<Inbox>()
            .add_observer(capture_outbound_packet)
            .add_systems(PreUpdate, drain_inbox.in_set(EnsembleSet::ReceivePackets));
    }
}

fn capture_outbound_packet(packet: On<SerializedLobbyPacket>, mut outbox: ResMut<Outbox>) {
    outbox
        .0
        .push((packet.entity, packet.packet.clone(), packet.send_mode));
}

fn drain_inbox(world: &mut World) {
    let packets = std::mem::take(&mut world.resource_mut::<Inbox>().0);
    for (sender, bytes) in packets {
        decode_ensemble_packet(world, Some(sender), &bytes);
    }
}

/// The shape of the connection between every pair of peers.
///
/// One-way values: a [`delay`](Self::delay) of 50 ms shows up as a ~100 ms round trip, which is
/// what [`PeerRtt`] is given.
#[derive(Clone, Copy, Debug)]
pub struct Link {
    /// One-way transit time.
    pub delay: Duration,
    /// Maximum extra delay applied per packet, uniform in `0..=jitter`.
    pub jitter: Duration,
    /// Probability a packet needs retransmitting (reliable) or is lost (unreliable), `0.0..=1.0`.
    pub loss: f32,
}

impl Default for Link {
    fn default() -> Self {
        Self::perfect()
    }
}

impl Link {
    /// No delay, no jitter, no loss. Packets arrive on the next frame.
    pub fn perfect() -> Self {
        Self {
            delay: Duration::ZERO,
            jitter: Duration::ZERO,
            loss: 0.0,
        }
    }

    /// A fixed one-way delay.
    pub fn delayed(delay: Duration) -> Self {
        Self {
            delay,
            ..Self::perfect()
        }
    }

    /// Wired broadband. Mirrors [`NetPreset::Cable`].
    pub fn cable() -> Self {
        Self {
            delay: Duration::from_millis(15),
            jitter: Duration::from_millis(3),
            loss: 0.001,
        }
    }

    /// Typical mobile. Mirrors [`NetPreset::FourG`].
    pub fn four_g() -> Self {
        Self {
            delay: Duration::from_millis(40),
            jitter: Duration::from_millis(15),
            loss: 0.005,
        }
    }

    /// Congested wifi: the jitter is the interesting part. Mirrors [`NetPreset::BadWifi`].
    pub fn bad_wifi() -> Self {
        Self {
            delay: Duration::from_millis(60),
            jitter: Duration::from_millis(40),
            loss: 0.02,
        }
    }

    /// Geostationary satellite. Mirrors [`NetPreset::Satellite`].
    pub fn satellite() -> Self {
        Self {
            delay: Duration::from_millis(300),
            jitter: Duration::from_millis(20),
            loss: 0.01,
        }
    }

    /// The round trip this link implies, which is what a ping would measure.
    pub fn round_trip(&self) -> Duration {
        self.delay * 2
    }
}

/// Seeded xorshift64*, so a run replays. Same reasoning as `bevy_ensemble`'s netsim: the tool
/// exists to reproduce bugs, not to find fresh ones every time CI runs.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        state
    }

    /// Uniform in `[0, 1)`.
    fn next_unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn next_below(&mut self, bound: u64) -> u64 {
        if bound == 0 { 0 } else { self.next_u64() % bound }
    }
}

struct Packet {
    to: PeerId,
    from_uuid: PlayerUUID,
    bytes: Vec<u8>,
    deliver_at_frame: u64,
}

struct Peer {
    app: App,
    uuid: PlayerUUID,
    is_host: bool,
    lobby: Entity,
    /// Whether this peer is still attached. A disconnected peer keeps ticking, so a test can
    /// watch what it does alone.
    connected: bool,
}

/// One host, any number of clients, one deterministic clock.
pub struct LoopbackNetwork {
    peers: Vec<Peer>,
    link: Link,
    in_flight: Vec<Packet>,
    /// Per-link (`from`, `to`) frame of the last scheduled delivery, so ordering is preserved.
    last_delivery: std::collections::HashMap<(usize, usize), u64>,
    frame: u64,
    rng: Rng,
    /// How much virtual time one frame represents. Converts [`Link`] durations into frames, and
    /// drives the netsim clock.
    frame_duration: Duration,
    netsim: NetPreset,
}

impl LoopbackNetwork {
    /// A network whose frames are `frame_duration` of virtual time each.
    ///
    /// Pass whatever one call to `App::update` represents in the game being tested — usually its
    /// tick duration, since a test that pins one tick to one frame is the easiest kind to reason
    /// about.
    pub fn new(frame_duration: Duration) -> Self {
        Self {
            peers: Vec::new(),
            link: Link::perfect(),
            in_flight: Vec::new(),
            last_delivery: std::collections::HashMap::new(),
            frame: 0,
            rng: Rng(0x2545_f491_4f6c_dd1d),
            frame_duration,
            netsim: NetPreset::Off,
        }
    }

    fn duration_to_frames(&self, duration: Duration) -> u64 {
        (duration.as_secs_f64() / self.frame_duration.as_secs_f64()).round() as u64
    }

    /// Attach `app` as the host, spawning the `(Lobby, Host)` entity a real backend would.
    ///
    /// The app must already have [`LoopbackTransportPlugin`]. Everything else about it — its
    /// plugins, its starting world — belongs to the game.
    pub fn add_host(&mut self, uuid: PlayerUUID, mut app: App) -> PeerId {
        let lobby = app.world_mut().spawn((Lobby, Host)).id();
        self.push_peer(app, uuid, true, lobby)
    }

    /// Attach `app` as a client and open the connection.
    ///
    /// Spawns the client's own `Lobby` — which is what starts the join handshake, since
    /// `request_join_snapshot_on_client_join` fires on `Added<Lobby>` — and the matching
    /// `LobbyClient` on the host, which is what a real backend spawns when a peer arrives.
    ///
    /// Returns without running the handshake: how long a join takes, and whether it finishes at
    /// all, is exactly what a test may want to assert about.
    pub fn add_client(&mut self, uuid: PlayerUUID, mut app: App) -> PeerId {
        let lobby = app.world_mut().spawn(Lobby).id();
        let peer = self.push_peer(app, uuid, false, lobby);

        let host = self.host();
        let host_lobby = self.peers[host.0].lobby;
        self.peers[host.0].app.world_mut().spawn((
            LobbyClient,
            LobbyClientPlayerUuid(uuid),
            LobbyParticipantOf(host_lobby),
        ));

        peer
    }

    fn push_peer(&mut self, app: App, uuid: PlayerUUID, is_host: bool, lobby: Entity) -> PeerId {
        let peer = PeerId(self.peers.len());
        self.peers.push(Peer {
            app,
            uuid,
            is_host,
            lobby,
            connected: true,
        });
        self.apply_netsim(peer);
        peer
    }

    /// Drop a peer off the network. Its app keeps running; nothing reaches it any more.
    ///
    /// Despawning the host's `LobbyClient` is what a backend does on disconnect, and it is what
    /// makes `on_lobby_client_removed` tell everyone else.
    pub fn disconnect(&mut self, peer: PeerId) {
        let uuid = self.peers[peer.0].uuid;
        self.peers[peer.0].connected = false;
        self.in_flight.retain(|packet| packet.to != peer);

        let host = self.host();
        let host_world = self.peers[host.0].app.world_mut();
        let client_entity = host_world
            .query_filtered::<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>()
            .iter(host_world)
            .find(|(_, player_uuid)| player_uuid.0 == uuid)
            .map(|(entity, _)| entity);
        if let Some(client_entity) = client_entity {
            host_world.despawn(client_entity);
        }
    }

    /// The host peer.
    pub fn host(&self) -> PeerId {
        PeerId(
            self.peers
                .iter()
                .position(|peer| peer.is_host)
                .expect("a network always has a host"),
        )
    }

    pub fn peers(&self) -> impl Iterator<Item = PeerId> + '_ {
        (0..self.peers.len()).map(PeerId)
    }

    pub fn uuid(&self, peer: PeerId) -> PlayerUUID {
        self.peers[peer.0].uuid
    }

    /// The peer that owns `uuid`, if it is on this network.
    pub fn peer_by_uuid(&self, uuid: PlayerUUID) -> Option<PeerId> {
        self.peers
            .iter()
            .position(|peer| peer.uuid == uuid)
            .map(PeerId)
    }

    /// This peer's `Lobby` entity.
    pub fn lobby(&self, peer: PeerId) -> Entity {
        self.peers[peer.0].lobby
    }

    pub fn is_connected(&self, peer: PeerId) -> bool {
        self.peers[peer.0].connected
    }

    pub fn app(&self, peer: PeerId) -> &App {
        &self.peers[peer.0].app
    }

    pub fn app_mut(&mut self, peer: PeerId) -> &mut App {
        &mut self.peers[peer.0].app
    }

    /// Every peer's app, in peer order.
    pub fn apps_mut(&mut self) -> impl Iterator<Item = &mut App> {
        self.peers.iter_mut().map(|peer| &mut peer.app)
    }

    /// Frames elapsed since the network was created.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    pub fn link(&self) -> Link {
        self.link
    }

    /// Replace the link shape for every pair. Takes effect on the next [`step`](Self::step).
    pub fn set_link(&mut self, link: Link) {
        self.link = link;
    }

    /// Impair inside the apps with `bevy_ensemble`'s netsim as well as, or instead of, [`Link`].
    ///
    /// Netsim runs at the decode seam, so it exercises its own queueing rather than this crate's —
    /// useful for checking a game against the presets it ships with. It cannot see a packet's
    /// [`SendMode`], so every app is put on
    /// [`ChannelModel::Reliable`](bevy_ensemble::ChannelModel::Reliable): the traffic a lockstep
    /// or state-sync game sends is reliable, and modelling loss as a drop would test a failure the
    /// transport cannot produce.
    ///
    /// Leave [`set_link`](Self::set_link) at [`Link::perfect`] when using this, or both
    /// impairments stack.
    pub fn use_netsim(&mut self, preset: NetPreset) {
        self.netsim = preset;
        for index in 0..self.peers.len() {
            self.apply_netsim(PeerId(index));
        }
        self.sync_netsim_clocks();
    }

    fn apply_netsim(&mut self, peer: PeerId) {
        let preset = self.netsim;
        let world = self.peers[peer.0].app.world_mut();
        let Some(mut sim) = world.get_resource_mut::<bevy_ensemble::NetSim>() else {
            // The app did not add `NetSimPlugin`, which is fine unless it wanted netsim.
            return;
        };
        sim.set_preset(preset);
        sim.set_channel_model(bevy_ensemble::ChannelModel::Reliable);
    }

    /// Hold every peer's simulator clock to the frame counter, so a netsim delay is measured in
    /// the same virtual time as everything else and no test depends on how long a frame took.
    fn sync_netsim_clocks(&mut self) {
        if self.netsim == NetPreset::Off {
            return;
        }
        let now = self.frame as f64 * self.frame_duration.as_secs_f64();
        for peer in &mut self.peers {
            peer.app
                .world_mut()
                .insert_resource(bevy_ensemble::NetSimClock::Manual(now));
        }
    }

    /// Advance the clock by `frames` and deliver everything that has come due.
    ///
    /// Split out from [`step`](Self::step) so a caller can do something to the apps between the
    /// delivery and the update — running several ticks inside one frame, say.
    pub fn advance(&mut self, frames: u64) {
        self.frame += frames;
        self.sync_netsim_clocks();
        self.deliver_due_packets();
        self.publish_peer_rtt();
    }

    /// Update every peer's app once.
    pub fn update_all(&mut self) {
        for peer in &mut self.peers {
            peer.app.update();
        }
    }

    /// Advance every peer by exactly one frame.
    pub fn step(&mut self) {
        self.advance(1);
        self.update_all();
        self.collect_outbound();
    }

    /// Advance `frames` frames.
    pub fn run(&mut self, frames: usize) {
        for _ in 0..frames {
            self.step();
        }
    }

    /// Step until `condition` holds, up to `max_frames`. Returns whether it held.
    pub fn run_until(&mut self, max_frames: usize, condition: impl Fn(&Self) -> bool) -> bool {
        for _ in 0..max_frames {
            if condition(self) {
                return true;
            }
            self.step();
        }
        condition(self)
    }

    /// Tell each peer what the connection currently looks like.
    ///
    /// Writing [`PeerRtt`] directly is the supported way to simulate a connection quality without
    /// simulating a connection: the ping system is the only other writer, and driving real pings
    /// would need real time.
    ///
    /// The published value carries the link's jitter, not just its mean. That detail is
    /// load-bearing for any game that sizes a buffer from observed RTT *variance*: a perfectly
    /// steady ping over a jittery link would tell it there is no jitter to protect against, and
    /// leave it with no headroom — a session that stalls constantly for a reason that exists only
    /// in the harness.
    pub fn publish_peer_rtt(&mut self) {
        let jitter_frames = self.duration_to_frames(self.link.jitter);
        // One sample of the same distribution `schedule` draws from, doubled for the round trip.
        let sampled_jitter =
            self.rng.next_below(jitter_frames + 1) as f64 * self.frame_duration.as_secs_f64();
        let mut round_trip = self.link.round_trip().as_secs_f64() + 2.0 * sampled_jitter;

        // Netsim delays each peer's *inbound* path, so a round trip crosses two impaired paths —
        // which is why its one-way `delay_ms` shows up as roughly double in `PeerRtt`. Leaving
        // this out is not a small inaccuracy: a game that sizes its buffer from `PeerRtt` would
        // keep a buffer for a perfect link and stall on the first packet that missed its tick.
        if self.netsim != NetPreset::Off {
            let config = self.netsim.config();
            let sampled = self.rng.next_unit() * config.jitter_ms;
            round_trip += 2.0 * f64::from(config.delay_ms + sampled) / 1000.0;
        }

        let connected: Vec<PlayerUUID> = self
            .peers
            .iter()
            .filter(|peer| peer.connected && !peer.is_host)
            .map(|peer| peer.uuid)
            .collect();

        for peer in &mut self.peers {
            if !peer.connected {
                continue;
            }
            if peer.is_host {
                let world = peer.app.world_mut();
                let clients: Vec<Entity> = world
                    .query_filtered::<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>()
                    .iter(world)
                    .filter(|(_, uuid)| connected.contains(&uuid.0))
                    .map(|(entity, _)| entity)
                    .collect();
                for client in clients {
                    world.entity_mut(client).insert(PeerRtt(round_trip));
                }
            } else {
                let lobby = peer.lobby;
                if let Ok(mut lobby) = peer.app.world_mut().get_entity_mut(lobby) {
                    lobby.insert(PeerRtt(round_trip));
                }
            }
        }
    }

    /// Drain every peer's outbox and schedule what it holds.
    pub fn collect_outbound(&mut self) {
        // Resolve each peer's outbox into (from, to, bytes, mode) before scheduling, because
        // scheduling needs `&mut self`.
        let mut resolved: Vec<(usize, usize, Vec<u8>, SendMode)> = Vec::new();
        let host = self.host().0;

        for index in 0..self.peers.len() {
            let packets =
                std::mem::take(&mut self.peers[index].app.world_mut().resource_mut::<Outbox>().0);
            if !self.peers[index].connected {
                continue;
            }
            for (entity, bytes, send_mode) in packets {
                let destination = if self.peers[index].is_host {
                    // On a host the packet is addressed to one `LobbyClient` entity.
                    let world = self.peers[index].app.world();
                    let Some(target_uuid) =
                        world.get::<LobbyClientPlayerUuid>(entity).map(|uuid| uuid.0)
                    else {
                        // The client entity is gone: the peer disconnected between encoding and
                        // now. Dropping is correct — there is nowhere to send it.
                        continue;
                    };
                    self.peers
                        .iter()
                        .position(|peer| peer.uuid == target_uuid && peer.connected)
                } else {
                    Some(host)
                };

                let Some(destination) = destination else {
                    continue;
                };
                if !self.peers[destination].connected {
                    continue;
                }
                resolved.push((index, destination, bytes, send_mode));
            }
        }

        for (from, to, bytes, send_mode) in resolved {
            self.schedule(from, to, bytes, send_mode);
        }
    }

    fn schedule(&mut self, from: usize, to: usize, bytes: Vec<u8>, send_mode: SendMode) {
        let mut delay_frames = self.duration_to_frames(self.link.delay);

        if self.link.jitter > Duration::ZERO {
            let jitter_frames = self.duration_to_frames(self.link.jitter);
            delay_frames += self.rng.next_below(jitter_frames + 1);
        }

        if self.link.loss > 0.0 && self.rng.next_unit() < self.link.loss {
            match send_mode {
                // A reliable channel retransmits. That costs a round trip; it does not lose the
                // message. Modelling it as a drop would test a failure the game cannot have.
                SendMode::Reliable => {
                    delay_frames += self.duration_to_frames(self.link.round_trip())
                }
                // Unreliable really is fire-and-forget.
                SendMode::Unreliable => return,
            }
        }

        // Never same-frame: a packet always takes at least until the next frame, as it would
        // through a real socket poll.
        let mut deliver_at_frame = self.frame + delay_frames.max(1);

        if matches!(send_mode, SendMode::Reliable) {
            // Preserve per-link ordering. Jitter may only ever push a packet later.
            let last = self.last_delivery.entry((from, to)).or_insert(0);
            deliver_at_frame = deliver_at_frame.max(*last);
            *last = deliver_at_frame;
        }

        self.in_flight.push(Packet {
            to: PeerId(to),
            from_uuid: self.peers[from].uuid,
            bytes,
            deliver_at_frame,
        });
    }

    fn deliver_due_packets(&mut self) {
        let frame = self.frame;
        let mut due: Vec<Packet> = Vec::new();
        // `extract_if` would be neater but the stable signature keeps changing; this is test
        // infrastructure and clarity wins.
        let mut remaining = Vec::with_capacity(self.in_flight.len());
        for packet in self.in_flight.drain(..) {
            if packet.deliver_at_frame <= frame {
                due.push(packet);
            } else {
                remaining.push(packet);
            }
        }
        self.in_flight = remaining;

        for packet in due {
            if !self.peers[packet.to.0].connected {
                continue;
            }
            self.peers[packet.to.0]
                .app
                .world_mut()
                .resource_mut::<Inbox>()
                .0
                .push((packet.from_uuid, packet.bytes));
        }
    }
}
