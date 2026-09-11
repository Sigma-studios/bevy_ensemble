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
//! part around them: peers, addressing, the links, and the frame loop.
//!
//! # How it models a bad connection
//!
//! [`Link`] delays packets by a whole number of frames and the network holds them until they come
//! due. Two decisions in it are worth stating, because getting them wrong would make the tests
//! lie:
//!
//! - **Reliable packets are never dropped, duplicated or reordered.** A reliable transport
//!   retransmits rather than loses, so [`Link::loss`] on a `SendMode::Reliable` packet costs it an
//!   extra round trip — which is what a retransmission is — and [`Link::duplicate`] and
//!   [`Link::reorder`] do not apply to it at all. Only an `Unreliable` packet is discarded,
//!   delivered twice, or overtaken.
//! - **Per-link order is preserved for reliable traffic.** Jitter moves a reliable packet's
//!   delivery later, never before one sent earlier on the same link. A reliable ordered channel
//!   does not reorder, and a test that saw reordering would be debugging the harness.
//!
//! It can make both of those calls because it impairs on the **send** side, where a packet still
//! carries its [`SendMode`]. That is the difference between it and `bevy_ensemble`'s `netsim`,
//! which impairs at the decode seam — behind the point where every backend has merged its
//! channels — and so has to be *told* which channel to assume. Use [`Link`] to model a link
//! whose two channels behave differently; use [`use_netsim`](LoopbackNetwork::use_netsim) to
//! exercise netsim's own queueing with the presets a game ships with. They compose, but by
//! default only one of them is doing anything.
//!
//! Links are per direction and per pair: [`set_link`](LoopbackNetwork::set_link) sets the default
//! for every pair, [`set_link_between`](LoopbackNetwork::set_link_between) overrides one direction
//! of one pair. A client on satellite next to a client on cable, or an uplink far worse than the
//! downlink, are both a line each.
//!
//! # Determinism
//!
//! No wall clock anywhere. One [`step`](LoopbackNetwork::step) is one frame on every peer, the
//! links' randomness is a seeded xorshift, and netsim runs on an injected
//! [`NetSimClock`](bevy_ensemble::NetSimClock) pinned to the frame counter. The same run twice
//! produces the same trace, which is the entire point — a flaky netcode test teaches nobody
//! anything. [`seed`](LoopbackNetwork::seed) picks the run; [`rng`](LoopbackNetwork::rng) hands
//! the test a seeded generator of its own for the *content* it sends, so the content is as
//! reproducible as the link.
//!
//! # What this harness cannot see, and what was added so that it can
//!
//! A consumer of this crate wrote down every bug it shipped to a real session that its loopback
//! tests could not have caught. The list is the specification for half of this API, and it is
//! worth keeping here so the next person does not rediscover it:
//!
//! - **Setup ordering the harness normalises.** [`add_host`](LoopbackNetwork::add_host) spawns
//!   `(Lobby, Host)` in one call and [`add_client`](LoopbackNetwork::add_client) spawns the host's
//!   [`LobbyClient`] immediately. A real backend does this across several frames in an order
//!   nobody chose, and the data channel is up before the lobby is promoted. That window is where
//!   a body arrives a frame after the roster and ends a round. So:
//!   [`add_pending_client`](LoopbackNetwork::add_pending_client) attaches a client whose lobby is
//!   still [`PendingLobby`] and whose packets already flow, and
//!   [`promote`](LoopbackNetwork::promote) finishes the join when the test says so.
//! - **Identity assignment.** The harness used to insert `LocalMultiplayerPlayerId` with the real
//!   value at construction; a real backend inserts it later, and a host adopting a role from a
//!   placeholder was a shipped bug. [`set_local_id`](LoopbackNetwork::set_local_id) changes or
//!   removes the identity after construction, so that window exists here too.
//! - **Frames that are not ticks.** One `step()` is one frame. Whether it is also one tick is the
//!   game's business: [`advance`](LoopbackNetwork::advance) and
//!   [`update_all`](LoopbackNetwork::update_all) are separate so a test can run several ticks in
//!   one frame, and [`step_only`](LoopbackNetwork::step_only) freezes every peer but the listed
//!   ones — which is what a backgrounded tab looks like from the network.
//! - **Leaving, rejoining, rehosting.** [`disconnect`](LoopbackNetwork::disconnect) models a
//!   dropped connection. [`leave`](LoopbackNetwork::leave) models pressing Leave;
//!   [`rejoin`](LoopbackNetwork::rejoin) the same app coming back with the same uuid;
//!   [`rehost`](LoopbackNetwork::rehost) the host quitting and another peer hosting;
//!   [`half_open`](LoopbackNetwork::half_open) a peer that still hears everyone and is heard by
//!   nobody, which is what an expired NAT binding looks like.
//! - **The wire itself.** [`trace_packets`](LoopbackNetwork::trace_packets) records every packet
//!   with its fate, so a test can assert that a secret never left a peer, that a burst stayed in
//!   order, or that a lost snapshot was the one it meant to lose.
//!   [`drop_next`](LoopbackNetwork::drop_next), [`corrupt_next`](LoopbackNetwork::corrupt_next)
//!   and [`deliver_raw`](LoopbackNetwork::deliver_raw) make a specific packet's fate the test's
//!   decision rather than the seed's.
//!
//! What it still cannot see: anything visual, and the transport — ICE, signalling, an actual
//! socket's reordering. A headless peer over the real transport is the rung above this one.
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
    EnsembleSet, EnsembleTransportAppExt, Host, HostUuid, Instant, Lobby, LobbyClient,
    LobbyClientPlayerUuid, LobbyParticipantOf, LocalMultiplayerPlayerId, NetPreset, PeerRtt,
    PeerRttJitter, PendingLobby, PlayerUUID, SendMode, SerializedLobbyPacket,
    decode_ensemble_packet,
};
use std::collections::{HashMap, VecDeque};
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
        app.claim_transport("bevy_ensemble_loopback")
            .init_resource::<Outbox>()
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
        decode_ensemble_packet(world, Some(sender), &bytes, Instant::now());
    }
}

/// The shape of one direction of the connection between two peers.
///
/// One-way values: a [`delay`](Self::delay) of 50 ms on both directions shows up as a ~100 ms
/// round trip, which is what [`PeerRtt`] is given.
///
/// [`duplicate`](Self::duplicate), [`reorder`](Self::reorder) and
/// [`max_message_size`](Self::max_message_size) apply to unreliable packets only. A reliable
/// transport deduplicates, orders and fragments, so on the reliable channel an oversize packet is
/// a warning rather than a loss, and the other two never happen.
#[derive(Clone, Copy, Debug)]
pub struct Link {
    /// One-way transit time.
    pub delay: Duration,
    /// Maximum extra delay applied per packet, uniform in `0..=jitter`.
    pub jitter: Duration,
    /// Probability a packet needs retransmitting (reliable) or is lost (unreliable), `0.0..=1.0`.
    pub loss: f32,
    /// Probability an unreliable packet is delivered twice, one frame apart.
    pub duplicate: f32,
    /// Probability an unreliable packet overtakes the one sent before it on the same link.
    ///
    /// Jitter already reorders unreliable packets once it exceeds a frame; this knob does it
    /// without any delay at all, so a test about reordering does not have to be a test about
    /// latency as well.
    pub reorder: f32,
    /// Largest unreliable packet the link carries. Anything bigger is dropped, counted as
    /// [`PacketFate::Oversize`], and logged at `warn!` — the same fate a datagram past the
    /// transport's fragment limit meets on a real socket.
    pub max_message_size: Option<usize>,
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
            duplicate: 0.0,
            reorder: 0.0,
            max_message_size: None,
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
            ..Self::perfect()
        }
    }

    /// Typical mobile. Mirrors [`NetPreset::FourG`].
    pub fn four_g() -> Self {
        Self {
            delay: Duration::from_millis(40),
            jitter: Duration::from_millis(15),
            loss: 0.005,
            ..Self::perfect()
        }
    }

    /// Congested wifi: the jitter is the interesting part. Mirrors [`NetPreset::BadWifi`],
    /// including its occasional duplicate.
    pub fn bad_wifi() -> Self {
        Self {
            delay: Duration::from_millis(60),
            jitter: Duration::from_millis(40),
            loss: 0.02,
            duplicate: 0.01,
            ..Self::perfect()
        }
    }

    /// Geostationary satellite. Mirrors [`NetPreset::Satellite`].
    pub fn satellite() -> Self {
        Self {
            delay: Duration::from_millis(300),
            jitter: Duration::from_millis(20),
            loss: 0.01,
            ..Self::perfect()
        }
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn with_jitter(mut self, jitter: Duration) -> Self {
        self.jitter = jitter;
        self
    }

    pub fn with_loss(mut self, loss: f32) -> Self {
        self.loss = loss;
        self
    }

    pub fn with_duplicate(mut self, duplicate: f32) -> Self {
        self.duplicate = duplicate;
        self
    }

    pub fn with_reorder(mut self, reorder: f32) -> Self {
        self.reorder = reorder;
        self
    }

    pub fn with_max_message_size(mut self, bytes: usize) -> Self {
        self.max_message_size = Some(bytes);
        self
    }

    /// The round trip a symmetric pair of this link implies, which is what a ping would measure.
    pub fn round_trip(&self) -> Duration {
        self.delay * 2
    }
}

/// Seeded xorshift64*, so a run replays. Same reasoning as `bevy_ensemble`'s netsim: the tool
/// exists to reproduce bugs, not to find fresh ones every time CI runs.
///
/// Also handed to tests through [`LoopbackNetwork::rng`] for the content they send, so that a
/// scripted "random" drawing or a random walk is as reproducible as the link it crosses.
#[derive(Clone, Debug)]
pub struct SeededRng(u64);

impl SeededRng {
    /// A generator that produces the same sequence for the same `seed`. Zero is remapped,
    /// because a zero xorshift state never leaves zero.
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x2545_f491_4f6c_dd1d
        } else {
            seed
        })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        state
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Uniform in `0..bound`; `0` when `bound` is `0`.
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }
}

/// What the link did with a packet. Recorded per packet when tracing is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketFate {
    /// Scheduled to arrive on this frame. Reordering can still move an unreliable packet later.
    Delivered { at_frame: u64 },
    /// An unreliable packet the link lost, or one [`LoopbackNetwork::drop_next`] asked for.
    Dropped,
    /// An unreliable packet delivered twice, on these frames.
    Duplicated { at_frames: [u64; 2] },
    /// An unreliable packet larger than [`Link::max_message_size`].
    Oversize,
    /// Nobody on the other end: the destination cannot receive, or the sender is half-open.
    Unreachable,
}

/// One packet as the network saw it. See [`LoopbackNetwork::trace_packets`].
#[derive(Clone, Debug)]
pub struct SentPacket {
    /// The frame the sender handed it over.
    pub frame: u64,
    pub from: PeerId,
    pub to: PeerId,
    pub mode: SendMode,
    /// The bytes as scheduled — after [`LoopbackNetwork::corrupt_next`], if it applied.
    pub bytes: Vec<u8>,
    pub fate: PacketFate,
}

impl SentPacket {
    /// Whether `needle` occurs anywhere in the payload. The test for "this secret never left the
    /// host" is one `iter().any(|p| p.contains(secret))`.
    pub fn contains(&self, needle: &[u8]) -> bool {
        !needle.is_empty()
            && self
                .bytes
                .windows(needle.len())
                .any(|window| window == needle)
    }

    pub fn was_delivered(&self) -> bool {
        matches!(
            self.fate,
            PacketFate::Delivered { .. } | PacketFate::Duplicated { .. }
        )
    }
}

/// How a peer is attached to the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attachment {
    /// Data flows both ways but the roster does not know it yet: the client holds a
    /// [`PendingLobby`], the host has no [`LobbyClient`] for it.
    Pending,
    Connected,
    /// Receives everything, is heard by nobody.
    HalfOpen,
    /// Neither direction. The host's [`LobbyClient`] is gone; the client's [`Lobby`] remains.
    Disconnected,
    /// Neither direction, and the client's [`Lobby`] is gone too.
    Left,
}

impl Attachment {
    fn can_send(self) -> bool {
        matches!(self, Self::Pending | Self::Connected)
    }

    fn can_receive(self) -> bool {
        matches!(self, Self::Pending | Self::Connected | Self::HalfOpen)
    }
}

struct Packet {
    /// Position in this link's send order. Delivery within a frame follows it, and an overtake
    /// is a swap of two packets' sequence numbers.
    seq: u64,
    to: PeerId,
    from: PeerId,
    from_uuid: PlayerUUID,
    bytes: Vec<u8>,
    reliable: bool,
    deliver_at_frame: u64,
}

struct Peer {
    app: App,
    uuid: PlayerUUID,
    is_host: bool,
    lobby: Option<Entity>,
    attachment: Attachment,
}

type Corruption = Box<dyn FnOnce(&mut Vec<u8>)>;

/// One host, any number of clients, one deterministic clock.
pub struct LoopbackNetwork {
    peers: Vec<Peer>,
    default_link: Link,
    /// Per-direction overrides of the default link, keyed `(from, to)`.
    links: HashMap<(usize, usize), Link>,
    in_flight: Vec<Packet>,
    /// Per-link (`from`, `to`) frame of the last scheduled reliable delivery, so ordering is
    /// preserved.
    last_reliable_delivery: HashMap<(usize, usize), u64>,
    /// Per-link send-order counter, so delivery within a frame follows send order and an
    /// overtake is a swap of two sequence numbers.
    next_seq: HashMap<(usize, usize), u64>,
    frame: u64,
    link_rng: SeededRng,
    content_rng: SeededRng,
    /// How much virtual time one frame represents. Converts [`Link`] durations into frames, and
    /// drives the netsim clock.
    frame_duration: Duration,
    netsim: NetPreset,
    trace: Option<Vec<SentPacket>>,
    /// `(packets, bytes)` handed to the network per `(from, to)`, whether or not they arrived.
    sent: HashMap<(usize, usize), (u64, u64)>,
    pending_drops: HashMap<(usize, usize), usize>,
    pending_corruptions: HashMap<(usize, usize), VecDeque<Corruption>>,
    /// Every `LobbyClient` entity the host has had, and who it stood for. A packet addressed
    /// to a client that was despawned this frame — the kick notification is exactly that — still
    /// has somewhere to go, as it does on a transport that keeps the connection open a moment
    /// longer than the entity.
    known_clients: HashMap<Entity, PlayerUUID>,
}

impl LoopbackNetwork {
    /// A network whose frames are `frame_duration` of virtual time each.
    ///
    /// Pass whatever one call to `App::update` represents in the game being tested — usually its
    /// tick duration, since a test that pins one tick to one frame is the easiest kind to reason
    /// about.
    pub fn new(frame_duration: Duration) -> Self {
        let mut network = Self {
            peers: Vec::new(),
            default_link: Link::perfect(),
            links: HashMap::new(),
            in_flight: Vec::new(),
            last_reliable_delivery: HashMap::new(),
            next_seq: HashMap::new(),
            frame: 0,
            link_rng: SeededRng::new(0),
            content_rng: SeededRng::new(0),
            frame_duration,
            netsim: NetPreset::Off,
            trace: None,
            sent: HashMap::new(),
            pending_drops: HashMap::new(),
            pending_corruptions: HashMap::new(),
            known_clients: HashMap::new(),
        };
        network.seed(0x2545_f491_4f6c_dd1d);
        network
    }

    /// Pick the run. Two networks with the same seed, peers and inputs produce the same trace.
    pub fn seed(&mut self, seed: u64) {
        self.link_rng = SeededRng::new(seed);
        // A different stream from the link's, so drawing test content does not perturb which
        // packets the link drops.
        self.content_rng = SeededRng::new(seed.rotate_left(32) ^ 0x9e37_79b9_7f4a_7c15);
    }

    /// A generator for the test's own content, seeded with the network.
    pub fn rng(&mut self) -> &mut SeededRng {
        &mut self.content_rng
    }

    fn duration_to_frames(&self, duration: Duration) -> u64 {
        (duration.as_secs_f64() / self.frame_duration.as_secs_f64()).round() as u64
    }

    // ---- attaching peers -------------------------------------------------------------------

    /// Attach `app` as the host, spawning the `(Lobby, Host)` entity a real backend would.
    ///
    /// The app must already have [`LoopbackTransportPlugin`]. Everything else about it — its
    /// plugins, its starting world — belongs to the game.
    pub fn add_host(&mut self, uuid: PlayerUUID, mut app: App) -> PeerId {
        let lobby = app.world_mut().spawn((Lobby, Host)).id();
        app.world_mut().insert_resource(HostUuid(uuid));
        self.push_peer(app, uuid, true, Some(lobby), Attachment::Connected)
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
        self.tell_who_the_host_is(&mut app);
        let peer = self.push_peer(app, uuid, false, Some(lobby), Attachment::Connected);
        self.spawn_lobby_client(uuid);
        peer
    }

    /// What a backend does as part of joining: the client learns its host's identity before
    /// any packet is decoded, which is what host-only messages are checked against.
    fn tell_who_the_host_is(&self, app: &mut App) {
        let host = self.peers[self.host().0].uuid;
        app.world_mut().insert_resource(HostUuid(host));
    }

    /// Attach `app` as a client whose join is not finished: the data channel is up, packets
    /// flow, but the client holds a [`PendingLobby`] and the host has no [`LobbyClient`] for it.
    ///
    /// This is the window a real backend has between the transport connecting and the lobby
    /// being promoted, and it is where a game that keys "am I in a session" on `With<Lobby>`
    /// applies the host's world while still identifying as solo. [`promote`](Self::promote)
    /// closes it.
    pub fn add_pending_client(&mut self, uuid: PlayerUUID, mut app: App) -> PeerId {
        let lobby = app.world_mut().spawn(PendingLobby).id();
        self.tell_who_the_host_is(&mut app);
        self.push_peer(app, uuid, false, Some(lobby), Attachment::Pending)
    }

    /// Finish a pending client's join: the client's lobby becomes a [`Lobby`], the host gets its
    /// [`LobbyClient`]. Exactly what `promote_client_lobby_on_host_handshake` does in the WebRTC
    /// backend, on the frame the test chooses.
    pub fn promote(&mut self, peer: PeerId) {
        assert_eq!(
            self.peers[peer.0].attachment,
            Attachment::Pending,
            "only a pending client can be promoted"
        );
        let uuid = self.peers[peer.0].uuid;
        let lobby = self.peers[peer.0]
            .lobby
            .expect("a pending client has a pending lobby");
        self.peers[peer.0]
            .app
            .world_mut()
            .entity_mut(lobby)
            .remove::<PendingLobby>()
            .insert(Lobby);
        self.peers[peer.0].attachment = Attachment::Connected;
        self.spawn_lobby_client(uuid);
    }

    /// Set or clear this peer's `LocalMultiplayerPlayerId` after construction.
    ///
    /// `None` is the window before the identity is known: a real backend does not know the
    /// peer's identity until the signalling server or Steam says so, and code that reads the
    /// resource before then reads nothing. A test that wants that window builds the app without
    /// the resource and calls this when the "backend" would have.
    pub fn set_local_id(&mut self, peer: PeerId, id: Option<PlayerUUID>) {
        let world = self.peers[peer.0].app.world_mut();
        match id {
            Some(id) => world.insert_resource(LocalMultiplayerPlayerId(id)),
            None => {
                world.remove_resource::<LocalMultiplayerPlayerId>();
            }
        }
    }

    fn push_peer(
        &mut self,
        app: App,
        uuid: PlayerUUID,
        is_host: bool,
        lobby: Option<Entity>,
        attachment: Attachment,
    ) -> PeerId {
        let peer = PeerId(self.peers.len());
        self.peers.push(Peer {
            app,
            uuid,
            is_host,
            lobby,
            attachment,
        });
        self.apply_netsim(peer);
        peer
    }

    fn spawn_lobby_client(&mut self, uuid: PlayerUUID) {
        let host = self.host();
        let host_lobby = self.peers[host.0]
            .lobby
            .expect("the host has a lobby while it is hosting");
        self.peers[host.0].app.world_mut().spawn((
            LobbyClient,
            LobbyClientPlayerUuid(uuid),
            LobbyParticipantOf(host_lobby),
        ));
    }

    fn despawn_lobby_client(&mut self, uuid: PlayerUUID) {
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

    // ---- detaching, reattaching ------------------------------------------------------------

    /// Drop a peer off the network. Its app keeps running; nothing reaches it any more.
    ///
    /// Despawning the host's `LobbyClient` is what a backend does on disconnect, and it is what
    /// makes `on_lobby_client_removed` tell everyone else. The client's own `Lobby` is left
    /// standing, as it would be until the client noticed.
    pub fn disconnect(&mut self, peer: PeerId) {
        let uuid = self.peers[peer.0].uuid;
        self.peers[peer.0].attachment = Attachment::Disconnected;
        self.in_flight.retain(|packet| packet.to != peer);
        self.despawn_lobby_client(uuid);
    }

    /// A peer that still receives everything and is heard by nobody.
    ///
    /// The shape of an expired NAT binding, a frozen process whose OS still acks, or a
    /// backgrounded tab: from every other peer's point of view it has gone silent, and nothing
    /// in the transport has said so. Whether the session notices is what a test with this wants
    /// to know.
    pub fn half_open(&mut self, peer: PeerId) {
        self.peers[peer.0].attachment = Attachment::HalfOpen;
        self.in_flight.retain(|packet| packet.from != peer);
    }

    /// Undo [`disconnect`](Self::disconnect) or [`half_open`](Self::half_open): the same app, the
    /// same uuid, both directions open. Respawns the host's `LobbyClient` if it was dropped.
    pub fn reconnect(&mut self, peer: PeerId) {
        assert!(
            !matches!(self.peers[peer.0].attachment, Attachment::Left),
            "a peer that left must rejoin, not reconnect"
        );
        let uuid = self.peers[peer.0].uuid;
        let was_disconnected = self.peers[peer.0].attachment == Attachment::Disconnected;
        self.peers[peer.0].attachment = Attachment::Connected;
        if was_disconnected && !self.peers[peer.0].is_host {
            self.spawn_lobby_client(uuid);
        }
    }

    /// The peer presses Leave. Its `Lobby` is despawned, the host's `LobbyClient` for it is
    /// despawned, and nothing flows either way. The app is still attached so it can
    /// [`rejoin`](Self::rejoin).
    pub fn leave(&mut self, peer: PeerId) {
        assert!(
            !self.peers[peer.0].is_host,
            "the host does not leave; see rehost"
        );
        let uuid = self.peers[peer.0].uuid;
        self.peers[peer.0].attachment = Attachment::Left;
        self.in_flight
            .retain(|packet| packet.to != peer && packet.from != peer);
        if let Some(lobby) = self.peers[peer.0].lobby.take() {
            self.peers[peer.0].app.world_mut().despawn(lobby);
        }
        self.peers[peer.0]
            .app
            .world_mut()
            .remove_resource::<HostUuid>();
        self.despawn_lobby_client(uuid);
    }

    /// A peer that [`left`](Self::leave) — or was left behind by a [`rehost`](Self::rehost) —
    /// comes back with the same uuid: a fresh `Lobby` on the client, a fresh `LobbyClient` on the
    /// host.
    pub fn rejoin(&mut self, peer: PeerId) {
        assert_eq!(
            self.peers[peer.0].attachment,
            Attachment::Left,
            "only a peer that left can rejoin"
        );
        let uuid = self.peers[peer.0].uuid;
        let host = self.peers[self.host().0].uuid;
        let world = self.peers[peer.0].app.world_mut();
        let lobby = world.spawn(Lobby).id();
        world.insert_resource(HostUuid(host));
        self.peers[peer.0].lobby = Some(lobby);
        self.peers[peer.0].attachment = Attachment::Connected;
        self.spawn_lobby_client(uuid);
    }

    /// The host quits and `new_host` hosts instead.
    ///
    /// The old host's `(Lobby, Host)` goes, and with it every `LobbyClient` (they are its
    /// participants); every client's `Lobby` goes, as it does when a host disappears; then
    /// `new_host` spawns its own `(Lobby, Host)`. Everyone else has [left](Self::leave) and has
    /// to [`rejoin`](Self::rejoin) — there is no host migration in this stack, and this models
    /// exactly that.
    pub fn rehost(&mut self, new_host: PeerId) {
        let old_host = self.host();
        assert_ne!(old_host, new_host, "already the host");
        for index in 0..self.peers.len() {
            let peer = &mut self.peers[index];
            if let Some(lobby) = peer.lobby.take()
                && let Ok(entity) = peer.app.world_mut().get_entity_mut(lobby)
            {
                entity.despawn();
            }
            peer.is_host = false;
            peer.attachment = Attachment::Left;
            peer.app.world_mut().remove_resource::<HostUuid>();
        }
        self.in_flight.clear();
        self.last_reliable_delivery.clear();
        self.next_seq.clear();
        self.known_clients.clear();

        let new_uuid = self.peers[new_host.0].uuid;
        let world = self.peers[new_host.0].app.world_mut();
        let lobby = world.spawn((Lobby, Host)).id();
        world.insert_resource(HostUuid(new_uuid));
        self.peers[new_host.0].lobby = Some(lobby);
        self.peers[new_host.0].is_host = true;
        self.peers[new_host.0].attachment = Attachment::Connected;
    }

    // ---- looking around --------------------------------------------------------------------

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

    /// This peer's `Lobby` (or `PendingLobby`) entity.
    ///
    /// # Panics
    ///
    /// If the peer has [left](Self::leave) and not rejoined.
    pub fn lobby(&self, peer: PeerId) -> Entity {
        self.peers[peer.0]
            .lobby
            .expect("this peer has left and has no lobby")
    }

    pub fn try_lobby(&self, peer: PeerId) -> Option<Entity> {
        self.peers[peer.0].lobby
    }

    /// Attached with both directions open. Pending, half-open, disconnected and departed peers
    /// all answer `false`.
    pub fn is_connected(&self, peer: PeerId) -> bool {
        self.peers[peer.0].attachment == Attachment::Connected
    }

    pub fn is_pending(&self, peer: PeerId) -> bool {
        self.peers[peer.0].attachment == Attachment::Pending
    }

    pub fn is_half_open(&self, peer: PeerId) -> bool {
        self.peers[peer.0].attachment == Attachment::HalfOpen
    }

    pub fn has_left(&self, peer: PeerId) -> bool {
        self.peers[peer.0].attachment == Attachment::Left
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

    pub fn frame_duration(&self) -> Duration {
        self.frame_duration
    }

    // ---- links -----------------------------------------------------------------------------

    /// The default link, used for every direction that has no override.
    pub fn link(&self) -> Link {
        self.default_link
    }

    /// Replace the default link for every pair and clear every per-direction override. Takes
    /// effect on the next [`step`](Self::step).
    pub fn set_link(&mut self, link: Link) {
        self.default_link = link;
        self.links.clear();
    }

    /// Override one direction of one pair. Every other direction keeps what it had.
    pub fn set_link_between(&mut self, from: PeerId, to: PeerId, link: Link) {
        self.links.insert((from.0, to.0), link);
    }

    /// Override both directions of one pair.
    pub fn set_link_pair(&mut self, a: PeerId, b: PeerId, a_to_b: Link, b_to_a: Link) {
        self.set_link_between(a, b, a_to_b);
        self.set_link_between(b, a, b_to_a);
    }

    /// The link a packet from `from` to `to` crosses.
    pub fn link_between(&self, from: PeerId, to: PeerId) -> Link {
        self.links
            .get(&(from.0, to.0))
            .copied()
            .unwrap_or(self.default_link)
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

    // ---- observing and interfering ---------------------------------------------------------

    /// Start or stop recording every packet handed to the network. Off by default; the record
    /// grows without bound while on.
    pub fn trace_packets(&mut self, on: bool) {
        match (on, self.trace.is_some()) {
            (true, false) => self.trace = Some(Vec::new()),
            (false, true) => self.trace = None,
            _ => {}
        }
    }

    /// Everything recorded since tracing was switched on, in the order it was sent.
    pub fn trace(&self) -> &[SentPacket] {
        self.trace.as_deref().unwrap_or(&[])
    }

    /// Take the record, leaving tracing on with an empty one.
    pub fn take_trace(&mut self) -> Vec<SentPacket> {
        self.trace.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// Bytes handed to the network from `from` to `to` since creation, delivered or not. Counted
    /// whether or not tracing is on.
    pub fn bytes_sent(&self, from: PeerId, to: PeerId) -> u64 {
        self.sent
            .get(&(from.0, to.0))
            .map_or(0, |(_, bytes)| *bytes)
    }

    /// Packets handed to the network from `from` to `to` since creation.
    pub fn packets_sent(&self, from: PeerId, to: PeerId) -> u64 {
        self.sent
            .get(&(from.0, to.0))
            .map_or(0, |(packets, _)| *packets)
    }

    /// Lose exactly the next `count` **unreliable** packets from `from` to `to`, whatever the
    /// link would have done. Reliable packets pass untouched: a reliable transport retransmits
    /// rather than loses, and [`Link::loss`] already models what that costs.
    pub fn drop_next(&mut self, from: PeerId, to: PeerId, count: usize) {
        *self.pending_drops.entry((from.0, to.0)).or_insert(0) += count;
    }

    /// Rewrite the next packet from `from` to `to` before it is scheduled. Queues: two calls
    /// corrupt the next two.
    pub fn corrupt_next(
        &mut self,
        from: PeerId,
        to: PeerId,
        rewrite: impl FnOnce(&mut Vec<u8>) + 'static,
    ) {
        self.pending_corruptions
            .entry((from.0, to.0))
            .or_default()
            .push_back(Box::new(rewrite));
    }

    /// Put `bytes` straight into `to`'s inbox for the next frame, as if `sender` had sent them.
    ///
    /// For crafting a control message the game would never send, or feeding a decoder garbage.
    /// Bypasses the links, the trace and the counters.
    pub fn deliver_raw(&mut self, to: PeerId, sender: PlayerUUID, bytes: Vec<u8>) {
        let from = self.peer_by_uuid(sender).unwrap_or(to);
        let seq = self.next_seq(from.0, to.0);
        self.in_flight.push(Packet {
            seq,
            to,
            from,
            from_uuid: sender,
            bytes,
            reliable: true,
            deliver_at_frame: self.frame + 1,
        });
    }

    // ---- the frame loop --------------------------------------------------------------------

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

    /// Update one peer's app once.
    pub fn update_peer(&mut self, peer: PeerId) {
        self.peers[peer.0].app.update();
    }

    /// Advance every peer by exactly one frame.
    pub fn step(&mut self) {
        self.advance(1);
        self.update_all();
        self.collect_outbound();
    }

    /// One frame in which only `peers` run. The rest are frozen: no update, so nothing sent, and
    /// what arrives for them waits in their inbox. A backgrounded tab, from the outside.
    pub fn step_only(&mut self, peers: &[PeerId]) {
        self.advance(1);
        for peer in peers {
            self.peers[peer.0].app.update();
        }
        self.collect_outbound();
    }

    /// One frame in which `each` decides what every peer does — several updates, none, or
    /// something to the world first.
    pub fn step_with(&mut self, mut each: impl FnMut(PeerId, &mut App)) {
        self.advance(1);
        for index in 0..self.peers.len() {
            each(PeerId(index), &mut self.peers[index].app);
        }
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
        let host = self.host();
        let netsim = (self.netsim != NetPreset::Off).then(|| self.netsim.config());

        let mut per_client: Vec<(PlayerUUID, f64, f64)> = Vec::new();
        for index in 0..self.peers.len() {
            let peer = PeerId(index);
            if self.peers[index].is_host || !self.peers[index].attachment.can_receive() {
                continue;
            }
            let up = self.link_between(peer, host);
            let down = self.link_between(host, peer);
            // One sample of the same distribution `schedule` draws from, per direction.
            let up_jitter = self.sample_jitter(up);
            let down_jitter = self.sample_jitter(down);
            let mut round_trip = (up.delay + down.delay).as_secs_f64() + up_jitter + down_jitter;

            // Netsim delays each peer's *inbound* path, so a round trip crosses two impaired
            // paths — which is why its one-way `delay_ms` shows up as roughly double in
            // `PeerRtt`. Leaving this out is not a small inaccuracy: a game that sizes its buffer
            // from `PeerRtt` would keep a buffer for a perfect link and stall on the first packet
            // that missed its tick.
            if let Some(config) = netsim {
                let sampled = self.link_rng.unit() * config.jitter_ms;
                round_trip += 2.0 * f64::from(config.delay_ms + sampled) / 1000.0;
            }

            // Published alongside the round trip, because a consumer sizing a playout buffer
            // needs the spread as well as the mean, and because the alternative is worse than it
            // looks: a harness that publishes only `PeerRtt` leaves that consumer deriving jitter
            // from a pre-smoothed series, which is what a lockstep adaptive buffer was doing on
            // real transports while every loopback test said its jitter headroom worked.
            // Modelling the same *signals* a real backend produces is what makes the harness able
            // to catch that class of bug at all.
            //
            // The expectation of one uniform draw is half the jitter; over two directions the
            // mean absolute deviation the real estimator converges to comes out at a quarter of
            // the sum.
            let mean_jitter = (up.jitter + down.jitter).as_secs_f64() / 4.0;
            per_client.push((self.peers[index].uuid, round_trip, mean_jitter));
        }

        {
            let world = self.peers[host.0].app.world_mut();
            let clients: Vec<(Entity, PlayerUUID)> = world
                .query_filtered::<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>()
                .iter(world)
                .map(|(entity, uuid)| (entity, uuid.0))
                .collect();
            for (entity, uuid) in clients {
                if let Some((_, round_trip, jitter)) =
                    per_client.iter().find(|(client, _, _)| *client == uuid)
                {
                    world
                        .entity_mut(entity)
                        .insert((PeerRtt(*round_trip), PeerRttJitter(*jitter)));
                }
            }
        }

        for (uuid, round_trip, jitter) in per_client {
            let Some(peer) = self.peer_by_uuid(uuid) else {
                continue;
            };
            let Some(lobby) = self.peers[peer.0].lobby else {
                continue;
            };
            if let Ok(mut lobby) = self.peers[peer.0].app.world_mut().get_entity_mut(lobby) {
                lobby.insert((PeerRtt(round_trip), PeerRttJitter(jitter)));
            }
        }
    }

    fn sample_jitter(&mut self, link: Link) -> f64 {
        let jitter_frames = self.duration_to_frames(link.jitter);
        self.link_rng.below(jitter_frames + 1) as f64 * self.frame_duration.as_secs_f64()
    }

    /// Drain every peer's outbox and schedule what it holds.
    pub fn collect_outbound(&mut self) {
        // Resolve each peer's outbox into (from, to, bytes, mode) before scheduling, because
        // scheduling needs `&mut self`.
        let mut resolved: Vec<(usize, usize, Vec<u8>, SendMode)> = Vec::new();
        let host = self.host().0;

        {
            let world = self.peers[host].app.world_mut();
            let current: Vec<(Entity, PlayerUUID)> = world
                .query_filtered::<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>()
                .iter(world)
                .map(|(entity, uuid)| (entity, uuid.0))
                .collect();
            self.known_clients.extend(current);
        }

        for index in 0..self.peers.len() {
            let packets =
                std::mem::take(&mut self.peers[index].app.world_mut().resource_mut::<Outbox>().0);
            for (entity, bytes, send_mode) in packets {
                let destination = if self.peers[index].is_host {
                    // On a host the packet is addressed to one `LobbyClient` entity — possibly
                    // one that was despawned since the packet was encoded, which is what a kick
                    // notification always is.
                    let Some(target_uuid) = self.known_clients.get(&entity).copied() else {
                        continue;
                    };
                    self.peers.iter().position(|peer| peer.uuid == target_uuid)
                } else {
                    Some(host)
                };
                let Some(destination) = destination else {
                    continue;
                };
                resolved.push((index, destination, bytes, send_mode));
            }
        }

        for (from, to, bytes, send_mode) in resolved {
            self.schedule(from, to, bytes, send_mode);
        }
    }

    fn next_seq(&mut self, from: usize, to: usize) -> u64 {
        let seq = self.next_seq.entry((from, to)).or_insert(0);
        *seq += 1;
        *seq
    }

    fn record(&mut self, from: usize, to: usize, mode: SendMode, bytes: &[u8], fate: PacketFate) {
        let entry = self.sent.entry((from, to)).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += bytes.len() as u64;
        if let Some(trace) = &mut self.trace {
            trace.push(SentPacket {
                frame: self.frame,
                from: PeerId(from),
                to: PeerId(to),
                mode,
                bytes: bytes.to_vec(),
                fate,
            });
        }
    }

    fn schedule(&mut self, from: usize, to: usize, mut bytes: Vec<u8>, send_mode: SendMode) {
        if let Some(queue) = self.pending_corruptions.get_mut(&(from, to))
            && let Some(rewrite) = queue.pop_front()
        {
            rewrite(&mut bytes);
        }

        if !self.peers[from].attachment.can_send() || !self.peers[to].attachment.can_receive() {
            self.record(from, to, send_mode, &bytes, PacketFate::Unreachable);
            return;
        }

        let link = self.link_between(PeerId(from), PeerId(to));
        let reliable = send_mode.is_reliable();

        if let Some(limit) = link.max_message_size
            && bytes.len() > limit
        {
            if reliable {
                warn!(
                    "loopback: a reliable packet of {} bytes from peer {from} to peer {to} \
                     exceeds the link's {limit}-byte message size; a real transport fragments \
                     and delivers it, which is what happens here",
                    bytes.len()
                );
            } else {
                warn!(
                    "loopback: dropping an unreliable packet of {} bytes from peer {from} to \
                     peer {to}: over the link's {limit}-byte message size, and an unreliable \
                     datagram past the fragment limit is lost whole",
                    bytes.len()
                );
                self.record(from, to, send_mode, &bytes, PacketFate::Oversize);
                return;
            }
        }

        let mut delay_frames = self.duration_to_frames(link.delay);
        if link.jitter > Duration::ZERO {
            let jitter_frames = self.duration_to_frames(link.jitter);
            delay_frames += self.link_rng.below(jitter_frames + 1);
        }

        let forced_drop = match self.pending_drops.get_mut(&(from, to)) {
            Some(remaining) if *remaining > 0 && !reliable => {
                *remaining -= 1;
                true
            }
            _ => false,
        };
        if forced_drop || (link.loss > 0.0 && self.link_rng.unit() < link.loss) {
            // A reliable channel retransmits. That costs a round trip; it does not lose the
            // message. Modelling it as a drop would test a failure the game cannot have.
            // Unreliable really is fire-and-forget.
            //
            // Asked of the mode rather than matched variant by variant: what this models is
            // delivery, and `ReliableNoDelay` differs from `Reliable` only in whether the
            // transport packs it with the next message, which nothing here simulates.
            if reliable {
                delay_frames += self.duration_to_frames(link.delay * 2);
            } else {
                self.record(from, to, send_mode, &bytes, PacketFate::Dropped);
                return;
            }
        }

        // Never same-frame: a packet always takes at least until the next frame, as it would
        // through a real socket poll.
        let mut deliver_at_frame = self.frame + delay_frames.max(1);

        if reliable {
            // Preserve per-link ordering. Jitter may only ever push a packet later.
            let last = self.last_reliable_delivery.entry((from, to)).or_insert(0);
            deliver_at_frame = deliver_at_frame.max(*last);
            *last = deliver_at_frame;
        }

        let mut seq = self.next_seq(from, to);
        let from_uuid = self.peers[from].uuid;
        let mut fate = PacketFate::Delivered {
            at_frame: deliver_at_frame,
        };

        if !reliable {
            if link.reorder > 0.0
                && self.link_rng.unit() < link.reorder
                && let Some(previous) = self
                    .in_flight
                    .iter_mut()
                    .filter(|packet| {
                        !packet.reliable
                            && packet.from.0 == from
                            && packet.to.0 == to
                            && packet.seq < seq
                    })
                    .max_by_key(|packet| packet.seq)
            {
                // Overtake the packet immediately before this one in the link's wire order: the
                // two swap places, in sequence and in delivery frame, so the later one arrives
                // first and nothing else moves.
                std::mem::swap(&mut previous.seq, &mut seq);
                std::mem::swap(&mut previous.deliver_at_frame, &mut deliver_at_frame);
                fate = PacketFate::Delivered {
                    at_frame: deliver_at_frame,
                };
            }

            if link.duplicate > 0.0 && self.link_rng.unit() < link.duplicate {
                let copy_seq = self.next_seq(from, to);
                self.in_flight.push(Packet {
                    seq: copy_seq,
                    to: PeerId(to),
                    from: PeerId(from),
                    from_uuid,
                    bytes: bytes.clone(),
                    reliable: false,
                    deliver_at_frame: deliver_at_frame + 1,
                });
                fate = PacketFate::Duplicated {
                    at_frames: [deliver_at_frame, deliver_at_frame + 1],
                };
            }
        }

        self.record(from, to, send_mode, &bytes, fate);
        self.in_flight.push(Packet {
            seq,
            to: PeerId(to),
            from: PeerId(from),
            from_uuid,
            bytes,
            reliable,
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

        // Within a frame, a link delivers in its wire order — send order, except where an
        // overtake swapped two sequence numbers. Links are ordered among themselves only so the
        // result is deterministic.
        due.sort_by_key(|packet| (packet.deliver_at_frame, packet.from, packet.to, packet.seq));

        for packet in due {
            if !self.peers[packet.to.0].attachment.can_receive() {
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
