use bevy::prelude::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{PlayerUUID, observers, registry::EnsembleMessageRegistry};

/// Controls how a message is delivered over the network.
///
/// - [`Reliable`](SendMode::Reliable): guaranteed delivery and ordering (default), and the
///   transport may hold it briefly to pack it in with whatever is sent next. Chat, lobby
///   management, bulk catch-up — anything where a few milliseconds buys fewer packets.
/// - [`ReliableNoDelay`](SendMode::ReliableNoDelay): the same guarantees, put on the wire now.
///   For the messages a simulation is *waiting on*.
/// - [`Unreliable`](SendMode::Unreliable): fire-and-forget with no delivery guarantee, and never
///   held. Frequently-updated, loss-tolerant data like position updates.
///
/// # Why reliable delivery has two modes
///
/// The coalescing is Nagle's algorithm, and whether it helps depends entirely on how far apart
/// the messages are. It buffers a small message for a few milliseconds — Steam's default is 5 ms
/// — hoping another arrives to share the packet.
///
/// For a stream of one message every simulation tick, another one *never* arrives in time: at
/// 64 Hz they are 15.6 ms apart. So the wait always costs latency and never once coalesces, in
/// both directions, on the critical path of a lockstep session that is already blocking on it.
///
/// For a burst — a joining client's catch-up, which can be hundreds of small messages in one
/// frame — it is the opposite. Those genuinely do pack together, and sending each as its own
/// datagram risks the loss that a reliable ordered channel answers with head-of-line blocking,
/// at exactly the moment a client is trying to catch up.
///
/// Neither answer is right for both, which is why the sender picks rather than the backend.
///
/// # What a backend does with it
///
/// Only a transport that coalesces has anything to do here — [`ReliableNoDelay`] maps to Steam's
/// `RELIABLE_NO_NAGLE`. Elsewhere it is [`Reliable`] with the same guarantees, which is why
/// backends should ask [`is_reliable`](SendMode::is_reliable) rather than compare against a
/// variant: the question almost all of them have is about delivery, not about packing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SendMode {
    #[default]
    Reliable,
    ReliableNoDelay,
    Unreliable,
}

impl SendMode {
    /// Whether delivery and ordering are guaranteed.
    ///
    /// The question a backend usually means when it reaches for `== SendMode::Reliable`, and the
    /// one that keeps answering correctly when a mode is added — that comparison silently
    /// reclassified [`ReliableNoDelay`](SendMode::ReliableNoDelay) as unreliable everywhere it
    /// appeared.
    pub fn is_reliable(&self) -> bool {
        !matches!(self, SendMode::Unreliable)
    }

    /// Whether the transport may hold this message to pack it with the next one.
    pub fn coalesces(&self) -> bool {
        matches!(self, SendMode::Reliable)
    }
}

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
    /// Local [`Time`](bevy::time::Time) elapsed when this packet came off the socket
    /// (i.e. at the inbound decode seam). This is the earliest moment the app can observe
    /// the packet — used by the ping/RTT machinery to measure how long a peer held a
    /// message before replying. `Duration::ZERO` for locally self-delivered messages.
    pub received_at: std::time::Duration,
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
///     commands.entity(*lobby).trigger(move |entity| LobbyMessage::new(entity, message));
/// }
/// ```
#[derive(EntityEvent, Debug)]
pub struct LobbyMessage<T: EnsembleMessage> {
    pub entity: Entity,
    pub message: T,
    pub send_mode: SendMode,
}

impl<T: EnsembleMessage> LobbyMessage<T> {
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

    /// Reliable, and not held back to be packed with the next message. See [`SendMode`].
    pub fn new_no_delay(entity: Entity, message: T) -> Self {
        Self {
            entity,
            message,
            send_mode: SendMode::ReliableNoDelay,
        }
    }
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
    pub send_mode: SendMode,
}

impl<T: EnsembleMessage> LobbyClientMessage<T> {
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

    /// Reliable, and not held back to be packed with the next message. See [`SendMode`].
    pub fn new_no_delay(entity: Entity, message: T) -> Self {
        Self {
            entity,
            message,
            send_mode: SendMode::ReliableNoDelay,
        }
    }
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
    pub send_mode: SendMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_reliable_modes_are_reliable() {
        // The trap this guards. Adding a mode does not break a `== SendMode::Reliable` test — it
        // makes it quietly wrong, and "reliable" is the half a transport decides delivery on. A
        // loopback that read `ReliableNoDelay` as unreliable would drop it on a lossy link, and a
        // WebRTC channel would send it on an unordered one, with nothing failing to compile.
        assert!(SendMode::Reliable.is_reliable());
        assert!(SendMode::ReliableNoDelay.is_reliable());
        assert!(!SendMode::Unreliable.is_reliable());
    }

    #[test]
    fn only_plain_reliable_waits_to_be_packed() {
        assert!(SendMode::Reliable.coalesces());
        assert!(
            !SendMode::ReliableNoDelay.coalesces(),
            "the entire point of the mode is that it is not held"
        );
        assert!(!SendMode::Unreliable.coalesces());
    }

    #[test]
    fn the_default_is_still_plain_reliable() {
        // Every existing caller takes the default, and the safe end of the trade for a message
        // nobody has thought about is the one that costs bandwidth rather than latency.
        assert_eq!(SendMode::default(), SendMode::Reliable);
    }
}
