use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    Host, Lobby, LocalMultiplayerPlayerId, PlayerUUID, SendMode,
    messages::{EnsembleAppExt, EnsembleMessage, LobbyMessage, ReceivedEnsembleMessage},
    registry::{EnsembleMessageRegistry, encode_ensemble_message, decode_ensemble_packet},
};

/// Internal envelope that wraps a serialized broadcast message with its original sender.
///
/// When a client triggers a [`BroadcastLobbyMessage`], the message is encoded into
/// this envelope and sent to the host via the normal [`LobbyMessage`] pipeline.
/// The host then relays the envelope to all clients.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LobbyBroadcastEnvelope {
    pub sender: PlayerUUID,
    pub payload: Vec<u8>,
    pub send_mode: SendMode,
}

/// Entity event to broadcast a message to all lobby members.
///
/// Trigger this on a lobby entity to send a message to **every** participant,
/// including the sender. This differs from [`LobbyMessage`] which only sends
/// host→clients or client→host without relay.
///
/// - On a **host** lobby: the message is sent to all clients and delivered locally.
/// - On a **client** lobby: the message is sent to the host, which relays it to
///   all clients (including back to the sender) and delivers it locally.
///
/// # Example
///
/// ```rust,ignore
/// fn send_chat(mut commands: Commands, lobby: Single<Entity, With<Lobby>>) {
///     commands.entity(*lobby).trigger(|entity| BroadcastLobbyMessage::new(
///         entity,
///         ChatMessage { text: "Hello!".into() },
///     ));
/// }
/// ```
#[derive(EntityEvent, Debug)]
pub struct BroadcastLobbyMessage<T: EnsembleMessage> {
    pub entity: Entity,
    pub message: T,
    pub send_mode: SendMode,
}

impl<T: EnsembleMessage> BroadcastLobbyMessage<T> {
    pub fn new(entity: Entity, message: T) -> Self {
        Self {
            entity,
            message,
            send_mode: SendMode::Reliable,
        }
    }

    pub fn new_unreliable(entity: Entity, message: T) -> Self {
        Self {
            entity,
            message,
            send_mode: SendMode::Unreliable,
        }
    }
}

/// Extension trait for registering broadcast message types.
///
/// Call this during app setup for every message type you want to broadcast
/// to all lobby members via [`BroadcastLobbyMessage`].
///
/// # Example
///
/// ```rust,ignore
/// app.add_plugins(LobbyBroadcastPlugin)
///    .register_broadcast_message::<ChatMessage>();
/// ```
pub trait LobbyBroadcastAppExt {
    fn register_broadcast_message<T: EnsembleMessage>(&mut self) -> &mut Self;
}

impl LobbyBroadcastAppExt for App {
    fn register_broadcast_message<T: EnsembleMessage>(&mut self) -> &mut Self {
        self.register_ensemble_message_type::<T>()
            .add_observer(encode_broadcast_message::<T>);
        self
    }
}

/// Plugin that enables lobby-wide broadcast messaging.
///
/// Add this alongside [`EnsemblePlugin`](crate::EnsemblePlugin) to allow
/// any lobby member to broadcast messages to all other members. Messages
/// from clients are automatically relayed through the host.
///
/// # Example
///
/// ```rust,ignore
/// app.add_plugins((EnsemblePlugin, LobbyBroadcastPlugin))
///    .register_broadcast_message::<ChatMessage>();
/// ```
pub struct LobbyBroadcastPlugin;

impl Plugin for LobbyBroadcastPlugin {
    fn build(&self, app: &mut App) {
        app.register_ensemble_message_type::<LobbyBroadcastEnvelope>()
            .add_systems(Update, relay_broadcast_envelopes);
    }
}

/// Observer that encodes a [`BroadcastLobbyMessage<T>`] into a [`LobbyBroadcastEnvelope`]
/// and routes it through the normal message pipeline.
///
/// On the host, also delivers the message locally via [`MessageWriter`].
fn encode_broadcast_message<T: EnsembleMessage>(
    message: On<BroadcastLobbyMessage<T>>,
    registry: Res<EnsembleMessageRegistry>,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    mut commands: Commands,
    mut local_writer: MessageWriter<ReceivedEnsembleMessage<T>>,
) {
    let sender = local_player.map(|p| p.0).unwrap_or(0);
    let payload = encode_ensemble_message(&registry, &message.message);
    let send_mode = message.send_mode;

    let is_host = host_lobbies.get(message.entity).is_ok();

    if is_host {
        local_writer.write(ReceivedEnsembleMessage {
            sender: Some(sender),
            message: message.message.clone(),
        });
    }

    let envelope = LobbyBroadcastEnvelope {
        sender,
        payload,
        send_mode,
    };
    commands
        .entity(message.entity)
        .trigger(move |entity| LobbyMessage {
            entity,
            message: envelope,
            send_mode,
        });
}

/// Exclusive system that handles received broadcast envelopes.
///
/// - On the **host**: relays the envelope to all clients via [`LobbyMessage`],
///   then decodes the inner payload for local delivery.
/// - On a **client**: decodes the inner payload for local delivery.
fn relay_broadcast_envelopes(world: &mut World) {
    let envelopes: Vec<_> = world
        .resource_mut::<Messages<ReceivedEnsembleMessage<LobbyBroadcastEnvelope>>>()
        .drain()
        .collect();

    if envelopes.is_empty() {
        return;
    }

    let host_entity = {
        let mut query = world.query_filtered::<Entity, (With<Lobby>, With<Host>)>();
        query.iter(world).next()
    };

    for envelope in &envelopes {
        if let Some(lobby) = host_entity {
            let relay = envelope.message.clone();
            let send_mode = relay.send_mode;
            world.commands().entity(lobby).trigger(move |entity| LobbyMessage {
                entity,
                message: relay,
                send_mode,
            });
        }

        decode_ensemble_packet(world, Some(envelope.message.sender), &envelope.message.payload);
    }
}
