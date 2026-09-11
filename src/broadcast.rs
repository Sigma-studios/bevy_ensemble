use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    Host, Lobby, LocalMultiplayerPlayerId, PlayerUUID, SendMode,
    messages::{EnsembleAppExt, EnsembleMessage, LobbyMessage, MessageAuthority, ReceivedEnsembleMessage},
    registry::{EnsembleMessageRegistry, decode_verified_packet, encode_ensemble_message, packet_index, refuse},
};

/// Internal envelope that wraps a serialized broadcast message with its original sender.
///
/// When a client triggers a [`BroadcastLobbyMessage`], the message is encoded into
/// this envelope and sent to the host via the normal [`LobbyMessage`] pipeline.
/// The host then relays the envelope to all clients.
///
/// # `sender` is what the host says it is
///
/// A client fills the field in, and the host **overwrites it with the transport sender** before
/// relaying or reading the payload. The field used to be believed as written, so a client could
/// put the host's uuid in it and have every peer — the host included — deliver the payload as
/// the host's. Clients accept the envelope itself only from their host (it is
/// [`MessageAuthority::HostOnly`]), which is what makes the stamped value trustworthy on arrival.
#[doc(hidden)]
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub struct LobbyBroadcastEnvelope {
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
    fn register_broadcast_message<T: EnsembleMessage>(&mut self, wire_name: &'static str) -> &mut Self;
}

impl LobbyBroadcastAppExt for App {
    fn register_broadcast_message<T: EnsembleMessage>(&mut self, wire_name: &'static str) -> &mut Self {
        self.register_ensemble_message_type::<T>(wire_name)
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
        // Second receive stage: decode broadcast envelopes into their inner messages. Runs
        // in PreUpdate right after the socket drain so the inner `ReceivedEnsembleMessage`s
        // reach Update readers the same frame — matching direct (non-broadcast) messages.
        app.register_control_message_type::<LobbyBroadcastEnvelope>(
            "bevy_ensemble/BroadcastEnvelope",
            MessageAuthority::HostOnly,
        )
            .add_systems(
                PreUpdate,
                relay_broadcast_envelopes.after(crate::EnsembleSet::ReceivePackets),
            );
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
    let Some(sender) = local_player.map(|p| p.0) else {
        // Nothing to sign it with. A zero used to go out here, which every peer then took to
        // be a real player; a message from nobody is better not sent than sent as nobody.
        warn!(
            "not broadcasting a `{}`: this peer has no identity yet",
            std::any::type_name::<T>()
        );
        return;
    };
    let payload = encode_ensemble_message(&registry, &message.message);
    let send_mode = message.send_mode;

    let is_host = host_lobbies.get(message.entity).is_ok();

    if is_host {
        // Locally self-delivered (never touched the socket): it arrived now.
        local_writer.write(ReceivedEnsembleMessage {
            sender: Some(sender),
            message: message.message.clone(),
            received_at: crate::Instant::now(),
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
/// - On the **host**: stamps the envelope with the transport sender, refuses to relay a payload
///   whose type is a control message, relays it to all clients via [`LobbyMessage`], then decodes
///   the inner payload for local delivery under the stamped sender.
/// - On a **client**: decodes the inner payload for local delivery. The envelope was accepted
///   only from the host, so the sender it carries is the host's word.
///
/// The inner payload goes through [`decode_ensemble_packet`] like any other packet, so a
/// [`MessageAuthority::HostOnly`] type inside an envelope from a client is still refused on
/// every client that receives the relay.
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
        let received_at = envelope.received_at;
        let Some(lobby) = host_entity else {
            // The envelope came from the verified host, which vouches for the inner sender;
            // the payload is not held for a verification the inner sender will never get here.
            decode_verified_packet(
                world,
                Some(envelope.message.sender),
                &envelope.message.payload,
                received_at,
            );
            continue;
        };

        // The host. Who sent this is what the transport says, not what the envelope says.
        let Some(sender) = envelope.sender else {
            continue;
        };
        let relayable = world
            .resource::<EnsembleMessageRegistry>()
            .is_relayable_packet(&envelope.message.payload);
        if !relayable {
            let index = packet_index(&envelope.message.payload).unwrap_or(u16::MAX);
            let type_name = world
                .resource::<EnsembleMessageRegistry>()
                .type_name_of_index(index)
                .unwrap_or("<unregistered>");
            refuse(world, index, type_name, Some(sender));
            continue;
        }

        let mut relay = envelope.message.clone();
        relay.sender = sender;
        let send_mode = relay.send_mode;
        world.commands().entity(lobby).trigger(move |entity| LobbyMessage {
            entity,
            message: relay,
            send_mode,
        });

        decode_verified_packet(world, Some(sender), &envelope.message.payload, received_at);
    }
}
