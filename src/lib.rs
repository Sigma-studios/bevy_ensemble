//! # bevy_ensemble
//!
//! An ECS-native networking abstraction for [Bevy](https://bevyengine.org/).
//!
//! `bevy_ensemble` provides a lobby-based multiplayer framework where all networking
//! state is expressed as Bevy components, relationships, and messages. Platform-specific
//! networking (Steam, Epic, raw sockets, etc.) is handled by separate backend crates
//! that implement the transport layer.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────┐
//! │         Your Game Code          │
//! │  (reads components, sends msgs) │
//! └──────────────┬──────────────────┘
//!                │
//! ┌──────────────▼──────────────────┐
//! │        bevy_ensemble            │
//! │  (lobby lifecycle, participant  │
//! │   sync, message serialization)  │
//! └──────────────┬──────────────────┘
//!                │
//! ┌──────────────▼──────────────────┐
//! │     Platform Backend Crate      │
//! │  (e.g. bevy_ensemble_steam)     │
//! │  (network I/O, handshakes)      │
//! └─────────────────────────────────┘
//! ```
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use bevy::prelude::*;
//! use bevy_ensemble::prelude::*;
//!
//! // 1. Define a custom message
//! #[derive(Message, Clone, Debug, serde::Serialize, serde::Deserialize)]
//! struct ChatMessage {
//!     text: String,
//! }
//!
//! // 2. Register it during app setup
//! fn main() {
//!     App::new()
//!         .add_plugins((DefaultPlugins, EnsemblePlugin))
//!         // Add your platform backend plugin here
//!         .register_ensemble_message_type::<ChatMessage>()
//!         .add_systems(Update, (send_chat, receive_chat))
//!         .run();
//! }
//!
//! // 3. Send messages by triggering LobbyMessage on a lobby entity
//! fn send_chat(mut commands: Commands, lobby: Single<Entity, With<Lobby>>) {
//!     commands.entity(*lobby).trigger(|entity| LobbyMessage {
//!         entity,
//!         message: ChatMessage { text: "Hello!".into() },
//!     });
//! }
//!
//! // 4. Receive messages via MessageReader
//! fn receive_chat(mut messages: MessageReader<ReceivedEnsembleMessage<ChatMessage>>) {
//!     for msg in messages.read() {
//!         println!("From {:?}: {}", msg.sender, msg.message.text);
//!     }
//! }
//! ```
//!
//! # Guides
//!
//! For detailed usage guides, see the `docs/` directory:
//! - [Lobbies](https://github.com/TODO/bevy_ensemble/blob/main/docs/lobbies.md) — creating, joining, and leaving lobbies
//! - [Participants](https://github.com/TODO/bevy_ensemble/blob/main/docs/participants.md) — player roster and ownership
//! - [Messaging](https://github.com/TODO/bevy_ensemble/blob/main/docs/messaging.md) — sending and receiving custom messages
//! - [Writing a Backend](https://github.com/TODO/bevy_ensemble/blob/main/docs/backends.md) — implementing a platform backend

use bevy::prelude::*;

mod broadcast;
mod components;
mod messages;
pub(crate) mod observers;
mod ping;
mod player_data;
pub mod prelude;
pub mod registry;
mod systems;
mod types;

pub use broadcast::{BroadcastLobbyMessage, LobbyBroadcastAppExt, LobbyBroadcastPlugin};
pub use components::*;
pub use messages::*;
pub use ping::{PeerLastPong, PeerRtt};
pub use player_data::{PlayerData, PlayerDataPlugin, SetPlayerData};
pub use registry::{EnsembleMessageRegistry, decode_ensemble_packet, encode_ensemble_message};
pub use types::*;

/// Core plugin that sets up the ensemble lobby and participant systems.
///
/// Add this plugin to your app alongside a platform backend plugin (e.g.
/// `BevyEnsembleSteamPlugin`). It registers the internal message types and
/// systems that manage lobby participants.
///
/// # Example
///
/// ```rust,ignore
/// App::new()
///     .add_plugins((DefaultPlugins, EnsemblePlugin))
///     .add_plugins(BevyEnsembleSteamPlugin::default())
///     .register_ensemble_message_type::<MyMessage>()
///     .run();
/// ```
pub struct EnsemblePlugin;

impl Plugin for EnsemblePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EnsembleMessageRegistry>()
            .add_message::<StartHosting>()
            .register_ensemble_message_type::<SyncLobbyParticipant>()
            .register_ensemble_message_type::<RemoveLobbyParticipant>()
            .register_ensemble_message_type::<ping::EnsemblePing>()
            .register_ensemble_message_type::<ping::EnsemblePong>()
            .add_observer(observers::on_lobby_client_removed)
            .add_systems(
                Update,
                (
                    systems::spawn_host_lobby,
                    systems::add_host_lobby_participant,
                    systems::sync_host_lobby_participant_identity,
                    systems::add_remote_lobby_participants,
                    systems::sync_existing_participants_to_new_lobby_clients
                        .after(systems::add_remote_lobby_participants),
                    systems::broadcast_changed_lobby_participants
                        .after(systems::add_remote_lobby_participants),
                    systems::apply_received_lobby_participants,
                    systems::apply_removed_lobby_participants,
                    ping::send_pings,
                    ping::respond_to_pings,
                    ping::receive_pongs,
                    ping::tick_last_pong,
                ),
            );
    }
}
