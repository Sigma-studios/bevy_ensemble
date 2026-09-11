use bevy::prelude::*;
use std::{
    any::{TypeId, type_name},
    collections::HashMap,
};

use crate::{
    Host, HostUuid, Lobby, PendingLobby, PlayerUUID,
    messages::{EnsembleMessage, MessageAuthority, ReceivedEnsembleMessage},
};

const MESSAGE_TYPE_INDEX_BYTES: usize = std::mem::size_of::<u16>();

/// Runtime registry that maps message types to compact indices for network serialization.
///
/// Each registered message type is assigned a sequential `u16` index. When a message
/// is sent over the network, this index is prepended to the serialized payload so the
/// receiving side knows which type to deserialize into.
///
/// You don't interact with this resource directly — use
/// [`register_ensemble_message_type`](crate::EnsembleAppExt::register_ensemble_message_type)
/// during app setup and the encoding/decoding functions handle the rest.
///
/// # Important
///
/// Message types must be registered in the **same order** on all peers, since the
/// indices are assigned sequentially. Using the same plugin setup code on all peers
/// guarantees this.
#[derive(Resource, Default)]
pub struct EnsembleMessageRegistry {
    entries: Vec<RegisteredEnsembleMessage>,
    type_indices: HashMap<TypeId, u16>,
}

struct RegisteredEnsembleMessage {
    type_name: &'static str,
    dispatch: fn(&mut World, Option<PlayerUUID>, &[u8]) -> bool,
    authority: MessageAuthority,
    /// Whether the broadcast relay may carry this type. Control messages may not: a client that
    /// could wrap a roster change in an envelope would have the host announce it to everyone.
    relayable: bool,
}

/// How many refusals of one kind are logged at `warn!` before the rest go to `debug!`. A peer
/// that sends what it may not send does so at frame rate, and a log that says so at frame rate
/// says nothing else.
const REFUSALS_LOGGED_LOUDLY: u32 = 3;

/// Packets refused at the trust boundary, by registered type index. Counted so a test can
/// assert on them and a diagnostic can show them; only the first few of each are loud.
#[derive(Resource, Default, Debug)]
pub struct RefusedPackets {
    pub by_type: HashMap<u16, u64>,
}

impl RefusedPackets {
    pub fn total(&self) -> u64 {
        self.by_type.values().sum()
    }
}

impl EnsembleMessageRegistry {
    pub(crate) fn register<T: EnsembleMessage>(
        &mut self,
        authority: MessageAuthority,
        relayable: bool,
    ) {
        let type_id = TypeId::of::<T>();
        let type_name = type_name::<T>();

        if self.type_indices.contains_key(&type_id) {
            panic!("Ensemble message type `{type_name}` was registered more than once");
        }

        let next_index = u16::try_from(self.entries.len()).unwrap_or_else(|_| {
            panic!(
                "Too many ensemble message types registered: maximum is {}",
                u16::MAX
            )
        });

        self.entries.push(RegisteredEnsembleMessage {
            type_name,
            dispatch: dispatch_message::<T>,
            authority,
            relayable,
        });
        self.type_indices.insert(type_id, next_index);
    }

    /// The authority a registered type index was given.
    pub fn authority_of_index(&self, index: u16) -> Option<MessageAuthority> {
        self.entry(index).map(|entry| entry.authority)
    }

    /// Whether the broadcast relay may carry the type at `index`. `false` for control messages
    /// and for anything that was never registered.
    pub fn is_relayable(&self, index: u16) -> bool {
        self.entry(index).is_some_and(|entry| entry.relayable)
    }

    /// Whether the broadcast relay may carry `packet`, judged by the type index at its front.
    pub fn is_relayable_packet(&self, packet: &[u8]) -> bool {
        packet_index(packet).is_some_and(|index| self.is_relayable(index))
    }

    /// The registered name of the type at `index`, for log lines.
    pub fn type_name_of_index(&self, index: u16) -> Option<&'static str> {
        self.entry(index).map(|entry| entry.type_name)
    }

    /// The wire index `T` travels under, or `None` if it was never registered.
    ///
    /// Public so a test can pin a registry's shape without decoding it off the wire: one consumer
    /// encoded a value of every type and read the two-byte prefix back, because this was private.
    pub fn index_of<T: EnsembleMessage>(&self) -> Option<u16> {
        self.type_indices.get(&TypeId::of::<T>()).copied()
    }

    fn entry(&self, index: u16) -> Option<&RegisteredEnsembleMessage> {
        self.entries.get(index as usize)
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

/// Deserializes a network packet and dispatches it as a [`ReceivedEnsembleMessage`].
///
/// Reads the 2-byte type index from the front of `packet`, looks up the registered
/// dispatch function, deserializes the postcard payload, and writes the resulting
/// [`ReceivedEnsembleMessage<T>`](ReceivedEnsembleMessage) to the world's message buffer.
///
/// Returns `true` if the packet was successfully decoded and dispatched, `false` otherwise.
/// Malformed packets are logged as warnings and skipped rather than panicking.
///
/// When the `netmetrics`/`netdebug` features are enabled this is the inbound
/// measurement + simulation seam: bytes are counted, and (under `netdebug`) the
/// packet may be dropped or delayed by the network simulator before it is decoded.
/// See [`crate::netsim`]. With those features off, this is a direct call into the
/// decode path with no added cost.
pub fn decode_ensemble_packet(world: &mut World, sender: Option<PlayerUUID>, packet: &[u8]) -> bool {
    #[cfg(feature = "netmetrics")]
    if let Some(mut metrics) = world.get_resource_mut::<crate::netmetrics::NetMetrics>() {
        metrics.rx_bytes += packet.len() as u64;
        metrics.rx_packets += 1;
    }

    #[cfg(feature = "netdebug")]
    match crate::netsim::offer_inbound(world, sender, packet) {
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

    decode_ensemble_packet_now(world, sender, packet)
}

/// The actual decode path, with no metrics or simulation. Called directly when the
/// simulator is inactive, and by `drain_netsim` when a delayed packet comes due.
pub(crate) fn decode_ensemble_packet_now(
    world: &mut World,
    sender: Option<PlayerUUID>,
    packet: &[u8],
) -> bool {
    if packet.len() < MESSAGE_TYPE_INDEX_BYTES {
        warn!("Received ensemble packet too short to contain a type index ({} bytes)", packet.len());
        return false;
    }

    let index = u16::from_le_bytes([packet[0], packet[1]]);
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

    dispatch(world, sender, &packet[MESSAGE_TYPE_INDEX_BYTES..])
}

/// The two-byte type index at the front of a packet, if it has one.
pub fn packet_index(packet: &[u8]) -> Option<u16> {
    (packet.len() >= MESSAGE_TYPE_INDEX_BYTES).then(|| u16::from_le_bytes([packet[0], packet[1]]))
}

/// Whether `sender` may deliver a [`MessageAuthority::HostOnly`] message to this peer.
///
/// On a host, anyone: the host is the authority and decides what to do with what it is sent.
/// On a client, only the peer named by [`HostUuid`]. A client that does not yet know who its
/// host is trusts nobody with an authoritative message — a backend sets `HostUuid` as part of
/// joining, before the data channel carries anything.
fn sender_is_trusted_for_host_only(world: &mut World, sender: Option<PlayerUUID>) -> bool {
    let is_client = {
        let mut lobbies = world.query_filtered::<(), (Or<(With<Lobby>, With<PendingLobby>)>, Without<Host>)>();
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

pub(crate) fn refuse(world: &mut World, index: u16, type_name: &'static str, sender: Option<PlayerUUID>) {
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

    // Stamp the receive time at the seam: this is the first frame the app can see the
    // packet. `Time` is frame-granular, which is exactly the resolution that matters here.
    let received_at = world
        .get_resource::<Time>()
        .map(|time| time.elapsed())
        .unwrap_or_default();

    if world
        .write_message(ReceivedEnsembleMessage::<T> { sender, message, received_at })
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
