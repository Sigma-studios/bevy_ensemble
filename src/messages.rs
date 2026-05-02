use bevy::prelude::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{PlayerUUID, observers, registry::EnsembleMessageRegistry};

/// Marker trait for types that can be sent over the network as ensemble messages.
///
/// Automatically implemented for any type that satisfies all bounds. You don't need
/// to implement this manually — just derive the required traits:
///
/// ```rust,ignore
/// #[derive(Message, Clone, Debug, serde::Serialize, serde::Deserialize)]
/// struct MyMessage {
///     data: String,
/// }
/// ```
pub trait EnsembleMessage:
    Message + Serialize + DeserializeOwned + Clone + Send + Sync + 'static
{
}

impl<T> EnsembleMessage for T where
    T: Message + Serialize + DeserializeOwned + Clone + Send + Sync + 'static
{
}

/// Extension trait for registering custom message types with the ensemble system.
///
/// Call this during app setup for every message type you want to send or receive
/// over the network.
///
/// # Example
///
/// ```rust,ignore
/// app.register_ensemble_message_type::<MyMessage>()
///     .register_ensemble_message_type::<AnotherMessage>();
/// ```
pub trait EnsembleAppExt {
    fn register_ensemble_message_type<T: EnsembleMessage>(&mut self) -> &mut Self;
}

impl EnsembleAppExt for App {
    fn register_ensemble_message_type<T: EnsembleMessage>(&mut self) -> &mut Self {
        self.init_resource::<EnsembleMessageRegistry>()
            .add_message::<ReceivedEnsembleMessage<T>>()
            .add_observer(observers::encode_lobby_message::<T>)
            .add_observer(observers::encode_lobby_client_message::<T>);

        let mut registry = self
            .world_mut()
            .resource_mut::<EnsembleMessageRegistry>();
        registry.register::<T>();

        self
    }
}

/// Request to start hosting a new lobby.
///
/// Write this message to create a new lobby with the local player as host.
/// The system will spawn an entity with [`PendingLobby`](crate::PendingLobby),
/// [`RequestLobby`](crate::RequestLobby), and [`Host`](crate::Host) components,
/// which the platform backend then picks up and creates on the network.
///
/// Only one hosted lobby is allowed at a time — additional messages are ignored
/// while a host lobby exists.
///
/// # Example
///
/// ```rust,ignore
/// fn start_hosting(mut writer: MessageWriter<StartHosting>) {
///     writer.write(StartHosting);
/// }
/// ```
#[derive(Message)]
pub struct StartHosting;

/// Internal message used to synchronize participant data from host to clients.
///
/// Sent automatically when participants are added or changed on the host.
/// Clients receive this as a [`ReceivedEnsembleMessage<SyncLobbyParticipant>`]
/// and create or update their local [`LobbyParticipant`](crate::LobbyParticipant) entities.
///
/// You generally don't need to interact with this directly.
#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncLobbyParticipant {
    pub player_uuid: PlayerUUID,
    pub is_host: bool,
}

/// Internal message used to remove a participant from client lobbies.
///
/// Sent by the host when a player disconnects. Clients receive this as a
/// [`ReceivedEnsembleMessage<RemoveLobbyParticipant>`] and despawn the
/// corresponding [`LobbyParticipant`](crate::LobbyParticipant) entity.
///
/// You generally don't need to interact with this directly.
#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveLobbyParticipant {
    pub player_uuid: PlayerUUID,
}

/// A deserialized message received from the network.
///
/// Read these using a [`MessageReader`] to process incoming network messages.
/// The `sender` field contains the [`PlayerUUID`] of the remote player who sent
/// the message, or `None` if the sender is unknown.
///
/// # Example
///
/// ```rust,ignore
/// fn handle_chat(mut messages: MessageReader<ReceivedEnsembleMessage<ChatMessage>>) {
///     for msg in messages.read() {
///         println!("From {:?}: {}", msg.sender, msg.message.text);
///     }
/// }
/// ```
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ReceivedEnsembleMessage<T: EnsembleMessage> {
    pub sender: Option<PlayerUUID>,
    pub message: T,
}

/// Entity event to broadcast a message from a lobby.
///
/// Trigger this on a lobby entity to send a message to participants:
/// - On a **host** lobby: the message is forwarded to all connected [`LobbyClient`](crate::LobbyClient) entities.
/// - On a **client** lobby: the message is sent to the host.
///
/// # Example
///
/// ```rust,ignore
/// fn send_chat(mut commands: Commands, lobby: Single<Entity, With<Lobby>>) {
///     let message = ChatMessage { text: "Hello!".into() };
///     commands.entity(*lobby).trigger(move |entity| LobbyMessage {
///         entity,
///         message,
///     });
/// }
/// ```
#[derive(EntityEvent, Debug)]
pub struct LobbyMessage<T: EnsembleMessage> {
    pub entity: Entity,
    pub message: T,
}

/// Entity event to send a serialized message through a specific client connection.
///
/// This is an intermediate step in the message encoding pipeline. You typically
/// don't trigger this directly — it is created automatically when a
/// [`LobbyMessage`] is processed.
///
/// The observer for this event serializes the message and triggers a
/// [`SerializedLobbyPacket`] on the same entity.
#[derive(EntityEvent, Debug)]
pub struct LobbyClientMessage<T: EnsembleMessage> {
    pub entity: Entity,
    pub message: T,
}

/// Entity event carrying a fully serialized network packet.
///
/// Triggered after a message has been serialized by the encoding pipeline. Platform
/// backends observe this event and transmit the raw bytes over the network.
///
/// # For backend implementors
///
/// Add an observer for this event and send `packet` over your network transport:
///
/// ```rust,ignore
/// app.add_observer(|packet: On<SerializedLobbyPacket>| {
///     // Send packet.packet bytes to the appropriate remote peer(s)
/// });
/// ```
#[derive(EntityEvent, Debug)]
pub struct SerializedLobbyPacket {
    pub entity: Entity,
    pub packet: Vec<u8>,
}
