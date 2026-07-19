use bevy::prelude::*;
use std::{
    any::{TypeId, type_name},
    collections::HashMap,
};

use crate::{PlayerUUID, messages::{EnsembleMessage, ReceivedEnsembleMessage}};

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
    _type_name: &'static str,
    dispatch: fn(&mut World, Option<PlayerUUID>, &[u8]) -> bool,
}

impl EnsembleMessageRegistry {
    pub(crate) fn register<T: EnsembleMessage>(&mut self) {
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
            _type_name: type_name,
            dispatch: dispatch_message::<T>,
        });
        self.type_indices.insert(type_id, next_index);
    }

    fn index_of<T: EnsembleMessage>(&self) -> Option<u16> {
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
    let dispatch = {
        let registry = world.resource::<EnsembleMessageRegistry>();
        let Some(entry) = registry.entry(index) else {
            warn!("Received ensemble packet for unregistered type index {index}");
            return false;
        };
        entry.dispatch
    };

    dispatch(world, sender, &packet[MESSAGE_TYPE_INDEX_BYTES..])
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
