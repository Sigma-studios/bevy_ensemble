use bevy::prelude::*;
use std::{
    any::{TypeId, type_name},
    collections::HashMap,
    sync::OnceLock,
};

use crate::{
    HandshakeVerified, Host, HostUuid, Instant, Lobby, LobbyClient, LobbyClientPlayerUuid,
    PendingLobby, PlayerUUID,
    messages::{EnsembleMessage, MessageAuthority, ReceivedEnsembleMessage},
};

const MESSAGE_TYPE_INDEX_BYTES: usize = std::mem::size_of::<u16>();

/// The type index that marks a packet as a frame of several messages rather than one message.
///
/// Reserved, as is [`HANDSHAKE_INDEX`]: a registry can hold at most `u16::MAX - 2` sorted types,
/// so no message ever travels under either.
const FRAME_INDEX: u16 = u16::MAX;

/// The type index the protocol handshake travels under, on every peer, whatever else it
/// registered.
///
/// The handshake is how two peers find out that their sorted indices disagree. It cannot itself
/// be one of those indices: a peer with one extra name registered shifts every rank by one, and
/// its handshake would arrive at the other side as some other type and be refused or misread —
/// exactly the failure it exists to report. So it is pinned outside the sorted space.
pub const HANDSHAKE_INDEX: u16 = u16::MAX - 1;

/// The largest number of sorted types a registry holds.
const MAX_SORTED_TYPES: usize = (u16::MAX - 2) as usize;

/// Bumped when the wire format of the core itself changes: the framing, the handshake, the
/// header. Folded into [`EnsembleMessageRegistry::wire_hash`], so two builds that register the
/// same names but frame them differently still refuse each other.
pub const PROTOCOL_VERSION: u32 = 2;

/// Runtime registry that maps message types to compact indices for network serialization.
///
/// Each registered message type travels under a `u16` index that is prepended to its postcard
/// payload. The index is the type's **rank among the sorted wire names**, computed once the
/// registry is first used and never again — so the order in which plugins happen to register
/// is irrelevant, and two peers agree on every index if and only if they registered the same
/// set of names. [`wire_hash`](Self::wire_hash) is the number they compare to find out.
///
/// You don't interact with this resource directly — use
/// [`register_ensemble_message_type`](crate::EnsembleAppExt::register_ensemble_message_type)
/// during app setup and the encoding/decoding functions handle the rest.
///
/// # Registration is over once a message has been sent
///
/// The sorted order is frozen the first time a packet is encoded or decoded. Registering after
/// that would renumber every type already on the wire, so it panics with the name of the
/// latecomer. Register in plugin `build`, as every shipped plugin does.
#[derive(Resource, Default)]
pub struct EnsembleMessageRegistry {
    /// In registration order. Wire indices come from `frozen`, not from position here.
    entries: Vec<RegisteredEnsembleMessage>,
    /// Type → position in `entries`.
    entry_of_type: HashMap<TypeId, usize>,
    frozen: OnceLock<Frozen>,
}

struct RegisteredEnsembleMessage {
    wire_name: &'static str,
    type_name: &'static str,
    dispatch: fn(&mut World, Option<PlayerUUID>, &[u8], Instant) -> bool,
    authority: MessageAuthority,
    /// Whether the broadcast relay may carry this type. Control messages may not: a client that
    /// could wrap a roster change in an envelope would have the host announce it to everyone.
    relayable: bool,
    /// Decoded before the sender's protocol has been compared. Only a backend's own ready
    /// handshake: the message that promotes a pending lobby, which is what the protocol
    /// handshake is announced on. Everything else waits.
    pre_verification: bool,
    /// A pinned index outside the sorted space, for the handshake. `None` for everything else.
    fixed_index: Option<u16>,
}

/// The sorted view, built once.
struct Frozen {
    /// Wire index → position in `entries`.
    entry_of_index: Vec<usize>,
    /// Position in `entries` → wire index.
    index_of_entry: Vec<u16>,
    hash: u64,
}

/// How many refusals of one kind are logged at `warn!` before the rest go to `debug!`. A peer
/// that sends what it may not send does so at frame rate, and a log that says so at frame rate
/// says nothing else.
const REFUSALS_LOGGED_LOUDLY: u32 = 3;

/// Packets refused at the trust boundary, by wire index. Counted so a test can assert on them
/// and a diagnostic can show them; only the first few of each are loud.
#[derive(Resource, Default, Debug)]
pub struct RefusedPackets {
    pub by_type: HashMap<u16, u64>,
}

impl RefusedPackets {
    pub fn total(&self) -> u64 {
        self.by_type.values().sum()
    }
}

/// Packets from a peer whose protocol has not been verified yet, kept until it is.
///
/// Nothing a peer says before its handshake has been compared can be read: with a differing
/// registry, its bytes decode as some other type — a roster sync read as a kick was the first
/// thing this caught. So everything but the handshake waits here, per sender, and is replayed
/// through the decoder the moment [`HandshakeVerified`] lands, or discarded if it never does.
/// Bounded per sender, so an unverified peer cannot fill memory.
#[derive(Resource, Default, Debug)]
pub struct HeldUntilVerified {
    by_sender: HashMap<PlayerUUID, Vec<(Vec<u8>, Instant)>>,
}

/// Bytes held per unverified sender before the oldest packets are dropped. A join's worth of
/// roster and state is a few kilobytes; this is memory an unverified peer may not fill.
const HELD_BYTES_PER_SENDER: usize = 256 * 1024;

impl HeldUntilVerified {
    fn hold(&mut self, sender: PlayerUUID, packet: &[u8], received_at: Instant) {
        let held = self.by_sender.entry(sender).or_default();
        held.push((packet.to_vec(), received_at));
        let mut bytes: usize = held.iter().map(|(p, _)| p.len()).sum();
        while bytes > HELD_BYTES_PER_SENDER && held.len() > 1 {
            bytes -= held.remove(0).0.len();
        }
    }

    /// Everything held for `sender`, oldest first, and nothing more held for it.
    pub fn take(&mut self, sender: PlayerUUID) -> Vec<(Vec<u8>, Instant)> {
        self.by_sender.remove(&sender).unwrap_or_default()
    }

    pub fn discard(&mut self, sender: PlayerUUID) {
        self.by_sender.remove(&sender);
    }

    /// Packets currently held for `sender`.
    pub fn held_for(&self, sender: PlayerUUID) -> usize {
        self.by_sender.get(&sender).map_or(0, Vec::len)
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv_fold(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

impl EnsembleMessageRegistry {
    pub(crate) fn register<T: EnsembleMessage>(
        &mut self,
        wire_name: &'static str,
        authority: MessageAuthority,
        relayable: bool,
    ) {
        self.register_inner::<T>(wire_name, authority, relayable, None, false);
    }

    /// Register a type under a pinned index outside the sorted space. Only the handshake.
    pub(crate) fn register_fixed<T: EnsembleMessage>(
        &mut self,
        wire_name: &'static str,
        index: u16,
        authority: MessageAuthority,
    ) {
        assert!(
            index == HANDSHAKE_INDEX,
            "the only pinned index is the handshake's"
        );
        self.register_inner::<T>(wire_name, authority, false, Some(index), false);
    }

    /// Register a backend's ready handshake: never relayed, and decoded before the sender's
    /// protocol is compared, because it is the message that leads to the comparison.
    pub fn register_pre_verification<T: EnsembleMessage>(
        &mut self,
        wire_name: &'static str,
        authority: MessageAuthority,
    ) {
        self.register_inner::<T>(wire_name, authority, false, None, true);
    }

    /// Whether the type at `index` is decoded from a peer whose protocol is not yet verified.
    pub fn is_pre_verification(&self, index: u16) -> bool {
        self.entry(index)
            .is_some_and(|entry| entry.pre_verification)
    }

    fn register_inner<T: EnsembleMessage>(
        &mut self,
        wire_name: &'static str,
        authority: MessageAuthority,
        relayable: bool,
        fixed_index: Option<u16>,
        pre_verification: bool,
    ) {
        let type_id = TypeId::of::<T>();
        let type_name = type_name::<T>();

        if self.frozen.get().is_some() {
            panic!(
                "Ensemble message type `{type_name}` (wire name `{wire_name}`) was registered \
                 after the registry was frozen by the first message on the wire; register every \
                 type in a plugin's `build`"
            );
        }
        if self.entry_of_type.contains_key(&type_id) {
            panic!("Ensemble message type `{type_name}` was registered more than once");
        }
        if let Some(taken) = self
            .entries
            .iter()
            .find(|entry| entry.wire_name == wire_name)
        {
            panic!(
                "wire name `{wire_name}` is already taken by `{}`; `{type_name}` needs its own",
                taken.type_name
            );
        }
        if self.entries.len() >= MAX_SORTED_TYPES {
            panic!("Too many ensemble message types registered: maximum is {MAX_SORTED_TYPES}");
        }

        self.entry_of_type.insert(type_id, self.entries.len());
        self.entries.push(RegisteredEnsembleMessage {
            wire_name,
            type_name,
            dispatch: dispatch_message::<T>,
            authority,
            relayable,
            pre_verification,
            fixed_index,
        });
    }

    fn frozen(&self) -> &Frozen {
        self.frozen.get_or_init(|| {
            let mut order: Vec<usize> = (0..self.entries.len())
                .filter(|entry| self.entries[*entry].fixed_index.is_none())
                .collect();
            order.sort_by_key(|entry| self.entries[*entry].wire_name);
            let mut index_of_entry = vec![0u16; self.entries.len()];
            for (index, entry) in order.iter().enumerate() {
                index_of_entry[*entry] = index as u16;
            }
            for (position, entry) in self.entries.iter().enumerate() {
                if let Some(fixed) = entry.fixed_index {
                    index_of_entry[position] = fixed;
                }
            }
            let hash = order.iter().fold(
                fnv_fold(FNV_OFFSET, &PROTOCOL_VERSION.to_le_bytes()),
                |hash, entry| {
                    let hash = fnv_fold(hash, self.entries[*entry].wire_name.as_bytes());
                    fnv_fold(hash, &[0])
                },
            );
            Frozen {
                entry_of_index: order,
                index_of_entry,
                hash,
            }
        })
    }

    /// The wire index `T` travels under, or `None` if it was never registered.
    ///
    /// Freezes the registry.
    pub fn index_of<T: EnsembleMessage>(&self) -> Option<u16> {
        let entry = *self.entry_of_type.get(&TypeId::of::<T>())?;
        Some(self.frozen().index_of_entry[entry])
    }

    fn entry(&self, index: u16) -> Option<&RegisteredEnsembleMessage> {
        if index == HANDSHAKE_INDEX {
            return self
                .entries
                .iter()
                .find(|entry| entry.fixed_index == Some(index));
        }
        let entry = *self.frozen().entry_of_index.get(usize::from(index))?;
        self.entries.get(entry)
    }

    /// Every registered wire name, in wire-index order. This list **is** the wire format.
    pub fn wire_names(&self) -> Vec<&'static str> {
        self.frozen()
            .entry_of_index
            .iter()
            .map(|entry| self.entries[*entry].wire_name)
            .collect()
    }

    /// A hash of the sorted wire names and the [`PROTOCOL_VERSION`].
    ///
    /// Two peers whose hashes agree read every packet as the same type; two whose hashes differ
    /// read at least one type as another, silently, and must not talk. The core exchanges this
    /// at join (see [`ProtocolHandshake`](crate::ProtocolHandshake)).
    pub fn wire_hash(&self) -> u64 {
        self.frozen().hash
    }

    /// Number of registered types.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The authority a registered wire index was given.
    pub fn authority_of_index(&self, index: u16) -> Option<MessageAuthority> {
        self.entry(index).map(|entry| entry.authority)
    }

    /// Whether the broadcast relay may carry the type at `index`. `false` for control messages
    /// and for anything that was never registered.
    pub fn is_relayable(&self, index: u16) -> bool {
        self.entry(index).is_some_and(|entry| entry.relayable)
    }

    /// Whether the broadcast relay may carry `packet`, judged by the type index at its front.
    /// A frame of several messages is never relayable.
    pub fn is_relayable_packet(&self, packet: &[u8]) -> bool {
        packet_index(packet).is_some_and(|index| index != FRAME_INDEX && self.is_relayable(index))
    }

    /// The wire name of the type at `index`, for log lines.
    pub fn wire_name_of_index(&self, index: u16) -> Option<&'static str> {
        self.entry(index).map(|entry| entry.wire_name)
    }

    /// The Rust type name of the type at `index`, for log lines.
    pub fn type_name_of_index(&self, index: u16) -> Option<&'static str> {
        self.entry(index).map(|entry| entry.type_name)
    }
}

/// Serializes a message into a network packet.
///
/// The returned `Vec<u8>` contains a 2-byte little-endian type index followed by
/// the postcard-encoded message payload.
///
/// # Panics
///
/// Panics if the message type was not registered via
/// [`register_ensemble_message_type`](crate::EnsembleAppExt::register_ensemble_message_type),
/// or if serialization fails.
pub fn encode_ensemble_message<T: EnsembleMessage>(
    registry: &EnsembleMessageRegistry,
    message: &T,
) -> Vec<u8> {
    let index = registry.index_of::<T>().unwrap_or_else(|| {
        panic!(
            "Ensemble message type `{}` was sent without being registered",
            type_name::<T>()
        )
    });

    let packet = Vec::from(index.to_le_bytes().as_slice());
    postcard::to_extend(message, packet).unwrap_or_else(|error| {
        panic!(
            "Failed to serialize ensemble message type `{}`: {error}",
            type_name::<T>()
        )
    })
}

// ---- frames -----------------------------------------------------------------------------------

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Pack several encoded messages into one packet: the reserved [`FRAME_INDEX`], the count, then
/// each message's length and bytes. One message is never framed — it goes as it is, so a single
/// message costs what it always did.
pub fn frame_packets(packets: &[Vec<u8>]) -> Vec<u8> {
    if packets.len() == 1 {
        return packets[0].clone();
    }
    let mut frame = Vec::from(FRAME_INDEX.to_le_bytes().as_slice());
    write_varint(&mut frame, packets.len() as u64);
    for packet in packets {
        write_varint(&mut frame, packet.len() as u64);
        frame.extend_from_slice(packet);
    }
    frame
}

/// The messages inside `packet`, or `None` if it is not a frame. A frame that does not add up
/// (a length past the end, a count that is not there) yields what it holds up to the fault.
pub fn unframe_packet(packet: &[u8]) -> Option<Vec<&[u8]>> {
    if packet_index(packet)? != FRAME_INDEX {
        return None;
    }
    let mut at = MESSAGE_TYPE_INDEX_BYTES;
    let count = read_varint(packet, &mut at)?;
    let mut messages = Vec::with_capacity(count.min(1024) as usize);
    for _ in 0..count {
        let Some(len) = read_varint(packet, &mut at) else {
            break;
        };
        let Some(end) = at.checked_add(len as usize) else {
            break;
        };
        let Some(bytes) = packet.get(at..end) else {
            break;
        };
        messages.push(bytes);
        at = end;
    }
    Some(messages)
}

/// The two-byte type index at the front of a packet, if it has one.
pub fn packet_index(packet: &[u8]) -> Option<u16> {
    (packet.len() >= MESSAGE_TYPE_INDEX_BYTES).then(|| u16::from_le_bytes([packet[0], packet[1]]))
}

// ---- decoding ---------------------------------------------------------------------------------

/// Deserializes a network packet and dispatches it as one or more [`ReceivedEnsembleMessage`]s.
///
/// Reads the 2-byte type index from the front of `packet`; if it is the frame marker, unpacks
/// the frame and dispatches every message in it, otherwise looks up the registered dispatch
/// function, deserializes the postcard payload, and writes the resulting
/// [`ReceivedEnsembleMessage<T>`](ReceivedEnsembleMessage) to the world's message buffer.
///
/// `received_at` is when the bytes came off the socket, stamped by the backend as early as it
/// can — on the receiving task, not when the app got round to draining it. It is what the ping
/// machinery measures dwell and round trips from.
///
/// Returns `true` if the packet was decoded and every message in it dispatched, `false`
/// otherwise. Malformed packets are logged as warnings and skipped rather than panicking.
///
/// When the `netmetrics`/`netdebug` features are enabled this is the inbound
/// measurement + simulation seam: bytes are counted, and (under `netdebug`) the
/// packet may be dropped or delayed by the network simulator before it is decoded.
/// See [`crate::netsim`]. With those features off, this is a direct call into the
/// decode path with no added cost.
pub fn decode_ensemble_packet(
    world: &mut World,
    sender: Option<PlayerUUID>,
    packet: &[u8],
    received_at: Instant,
) -> bool {
    #[cfg(feature = "netmetrics")]
    if let Some(mut metrics) = world.get_resource_mut::<crate::netmetrics::NetMetrics>() {
        metrics.rx_bytes += packet.len() as u64;
        metrics.rx_packets += 1;
    }

    #[cfg(feature = "netdebug")]
    match crate::netsim::offer_inbound(world, sender, packet, received_at) {
        crate::netsim::SimVerdict::PassThrough => {}
        crate::netsim::SimVerdict::Dropped => {
            if let Some(mut metrics) = world.get_resource_mut::<crate::netmetrics::NetMetrics>() {
                metrics.sim_dropped += 1;
            }
            return false;
        }
        crate::netsim::SimVerdict::Delayed { duplicated } => {
            if let Some(mut metrics) = world.get_resource_mut::<crate::netmetrics::NetMetrics>() {
                metrics.sim_duplicated += duplicated as u64;
            }
            // Packet is queued; it will be decoded later by `drain_netsim`.
            return true;
        }
    }

    decode_ensemble_packet_now(world, sender, packet, received_at)
}

/// The actual decode path, with no metrics or simulation. Called directly when the
/// simulator is inactive, and by `drain_netsim` when a delayed packet comes due.
///
/// This is the transport seam: a message from a peer whose protocol is not yet verified is
/// held (see [`HeldUntilVerified`]) unless it is the handshake itself. Frames are opened here
/// so that a handshake sharing a datagram with anything else is still read — it usually does,
/// since a host announces the roster in the same frame it announces its protocol.
pub(crate) fn decode_ensemble_packet_now(
    world: &mut World,
    sender: Option<PlayerUUID>,
    packet: &[u8],
    received_at: Instant,
) -> bool {
    let Some(sender) = sender else {
        return decode_verified_packet(world, None, packet, received_at);
    };
    let verified = peer_is_verified(world, sender);
    let mut all = true;
    let messages = unframe_packet(packet).unwrap_or_else(|| vec![packet]);
    for message in messages {
        // A backend's ready handshake is decoded before the protocol is compared: the WebRTC
        // backend promotes a pending lobby on it, and the protocol handshake is announced on
        // the promotion. Holding the one behind the other deadlocked every real join.
        let index = packet_index(message);
        let handshake = index.is_some_and(|index| {
            index == HANDSHAKE_INDEX
                || world
                    .resource::<EnsembleMessageRegistry>()
                    .is_pre_verification(index)
        });
        debug!(
            "packet from {sender:#x}: {:?}, handshake {handshake}, sender verified {verified}",
            index.and_then(|index| {
                world
                    .resource::<EnsembleMessageRegistry>()
                    .entry(index)
                    .map(|entry| entry.type_name)
            })
        );
        if handshake || verified {
            all &= decode_one(world, Some(sender), message, received_at);
        } else {
            world
                .get_resource_or_insert_with(HeldUntilVerified::default)
                .hold(sender, message, received_at);
        }
    }
    all
}

/// Decode a packet from a peer whose protocol is known to match — or a payload the host has
/// already vouched for, as with a relayed broadcast. Frames are unpacked here.
pub(crate) fn decode_verified_packet(
    world: &mut World,
    sender: Option<PlayerUUID>,
    packet: &[u8],
    received_at: Instant,
) -> bool {
    if let Some(messages) = unframe_packet(packet) {
        let mut all = true;
        for message in messages {
            all &= decode_one(world, sender, message, received_at);
        }
        return all;
    }
    decode_one(world, sender, packet, received_at)
}

/// Whether `sender`'s protocol handshake has been compared and matched.
///
/// On a host: the `LobbyClient` standing for `sender` carries [`HandshakeVerified`]. On a
/// client: `sender` is the host and the client's lobby carries it. A peer with no lobby of any
/// kind verifies nobody.
fn peer_is_verified(world: &mut World, sender: PlayerUUID) -> bool {
    let is_host = {
        let mut hosts =
            world.query_filtered::<(), (With<Host>, Or<(With<Lobby>, With<PendingLobby>)>)>();
        hosts.iter(world).next().is_some()
    };
    if is_host {
        let mut clients = world
            .query_filtered::<&LobbyClientPlayerUuid, (With<LobbyClient>, With<HandshakeVerified>)>(
            );
        return clients.iter(world).any(|uuid| uuid.0 == sender);
    }
    let host_is_sender = world
        .get_resource::<HostUuid>()
        .is_some_and(|host| host.0 == sender);
    if !host_is_sender {
        return false;
    }
    let mut lobbies = world.query_filtered::<(), (
        Or<(With<Lobby>, With<PendingLobby>)>,
        Without<Host>,
        With<HandshakeVerified>,
    )>();
    lobbies.iter(world).next().is_some()
}

fn decode_one(
    world: &mut World,
    sender: Option<PlayerUUID>,
    packet: &[u8],
    received_at: Instant,
) -> bool {
    let Some(index) = packet_index(packet) else {
        warn!(
            "Received ensemble packet too short to contain a type index ({} bytes)",
            packet.len()
        );
        return false;
    };
    if index == FRAME_INDEX {
        warn!("Received a frame inside a frame; not decoding it");
        return false;
    }

    #[cfg(feature = "netmetrics")]
    if let Some(mut metrics) = world.get_resource_mut::<crate::netmetrics::NetMetrics>() {
        metrics.rx_messages += 1;
    }

    let (dispatch, authority, type_name) = {
        let registry = world.resource::<EnsembleMessageRegistry>();
        let Some(entry) = registry.entry(index) else {
            warn!("Received ensemble packet for unregistered type index {index}");
            return false;
        };
        (entry.dispatch, entry.authority, entry.type_name)
    };

    if authority == MessageAuthority::HostOnly && !sender_is_trusted_for_host_only(world, sender) {
        refuse(world, index, type_name, sender);
        return false;
    }

    dispatch(
        world,
        sender,
        &packet[MESSAGE_TYPE_INDEX_BYTES..],
        received_at,
    )
}

/// Whether `sender` may deliver a [`MessageAuthority::HostOnly`] message to this peer.
///
/// On a host, anyone: the host is the authority and decides what to do with what it is sent.
/// On a client, only the peer named by [`HostUuid`]. A client that does not yet know who its
/// host is trusts nobody with an authoritative message — a backend sets `HostUuid` as part of
/// joining, before the data channel carries anything.
fn sender_is_trusted_for_host_only(world: &mut World, sender: Option<PlayerUUID>) -> bool {
    let is_client = {
        let mut lobbies =
            world.query_filtered::<(), (Or<(With<Lobby>, With<PendingLobby>)>, Without<Host>)>();
        lobbies.iter(world).next().is_some()
    };
    if !is_client {
        return true;
    }
    match (sender, world.get_resource::<HostUuid>()) {
        (Some(sender), Some(host)) => sender == host.0,
        _ => false,
    }
}

pub(crate) fn refuse(
    world: &mut World,
    index: u16,
    type_name: &'static str,
    sender: Option<PlayerUUID>,
) {
    let count = {
        let mut refused = world.get_resource_or_insert_with(RefusedPackets::default);
        let count = refused.by_type.entry(index).or_insert(0);
        *count += 1;
        *count
    };
    let host = world.get_resource::<HostUuid>().map(|host| host.0);
    if count <= u64::from(REFUSALS_LOGGED_LOUDLY) {
        warn!(
            "refused a `{type_name}` from {sender:#x?}: not something this peer takes from that \
             sender (host is {host:#x?}); {count} so far, later ones at debug level"
        );
    } else {
        debug!("refused a `{type_name}` from {sender:#x?} ({count} so far)");
    }
}

fn dispatch_message<T: EnsembleMessage>(
    world: &mut World,
    sender: Option<PlayerUUID>,
    payload: &[u8],
    received_at: Instant,
) -> bool {
    let message = match postcard::from_bytes::<T>(payload) {
        Ok(msg) => msg,
        Err(error) => {
            warn!(
                "Failed to deserialize ensemble message type `{}`: {error}",
                type_name::<T>()
            );
            return false;
        }
    };

    if world
        .write_message(ReceivedEnsembleMessage::<T> {
            sender,
            message,
            received_at,
        })
        .is_none()
    {
        warn!(
            "Message buffer for ensemble message type `{}` is missing",
            type_name::<T>()
        );
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips() {
        let packets = vec![vec![1, 0, 9, 9], vec![2, 0], vec![3, 0, 1, 2, 3, 4, 5]];
        let frame = frame_packets(&packets);
        let back = unframe_packet(&frame).expect("a frame");
        assert_eq!(back, packets.iter().map(Vec::as_slice).collect::<Vec<_>>());
    }

    #[test]
    fn a_single_message_is_not_framed() {
        let packets = vec![vec![1, 0, 9]];
        assert_eq!(frame_packets(&packets), packets[0]);
        assert!(unframe_packet(&packets[0]).is_none());
    }

    #[test]
    fn a_truncated_frame_yields_what_it_holds_and_never_panics() {
        let packets = vec![vec![1, 0, 9, 9], vec![2, 0, 7]];
        let frame = frame_packets(&packets);
        for cut in 0..frame.len() {
            let _ = unframe_packet(&frame[..cut]);
        }
        let half = unframe_packet(&frame[..frame.len() - 1]).expect("still a frame");
        assert_eq!(half, vec![packets[0].as_slice()]);
    }

    #[test]
    fn varints_round_trip() {
        for value in [0u64, 1, 127, 128, 300, 65_535, 1 << 40] {
            let mut out = Vec::new();
            write_varint(&mut out, value);
            let mut at = 0;
            assert_eq!(read_varint(&out, &mut at), Some(value));
            assert_eq!(at, out.len());
        }
    }
}
