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
    type_name: &'static str,
    dispatch: fn(&mut World, Option<PlayerUUID>, &[u8]),
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
            type_name,
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
/// the CBOR-encoded message payload.
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
    let payload = serde_cbor::to_vec(message).unwrap_or_else(|error| {
        panic!(
            "Failed to serialize ensemble message type `{}`: {error}",
            type_name::<T>()
        )
    });

    let mut packet = Vec::with_capacity(MESSAGE_TYPE_INDEX_BYTES + payload.len());
    packet.extend_from_slice(&index.to_le_bytes());
    packet.extend_from_slice(&payload);
    packet
}

/// Deserializes a network packet and dispatches it as a [`ReceivedEnsembleMessage`].
///
/// Reads the 2-byte type index from the front of `packet`, looks up the registered
/// dispatch function, deserializes the CBOR payload, and writes the resulting
/// [`ReceivedEnsembleMessage<T>`](ReceivedEnsembleMessage) to the world's message buffer.
///
/// # Panics
///
/// Panics if the packet is too short, the type index is unregistered, or
/// deserialization fails.
pub fn decode_ensemble_packet(world: &mut World, sender: Option<PlayerUUID>, packet: &[u8]) {
    assert!(
        packet.len() >= MESSAGE_TYPE_INDEX_BYTES,
        "Received ensemble packet that was too short to contain a type index"
    );

    let index = u16::from_le_bytes([packet[0], packet[1]]);
    let dispatch = {
        let registry = world.resource::<EnsembleMessageRegistry>();
        let entry = registry.entry(index).unwrap_or_else(|| {
            panic!("Received ensemble packet for unregistered type index {index}")
        });
        entry.dispatch
    };

    dispatch(world, sender, &packet[MESSAGE_TYPE_INDEX_BYTES..]);
}

fn dispatch_message<T: EnsembleMessage>(
    world: &mut World,
    sender: Option<PlayerUUID>,
    payload: &[u8],
) {
    let message = serde_cbor::from_slice::<T>(payload).unwrap_or_else(|error| {
        panic!(
            "Failed to deserialize ensemble message type `{}`: {error}",
            type_name::<T>()
        )
    });

    world
        .write_message(ReceivedEnsembleMessage::<T> { sender, message })
        .unwrap_or_else(|| {
            panic!(
                "Message buffer for ensemble message type `{}` is missing",
                type_name::<T>()
            )
        });
}
