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
//! - [Lobbies](https://github.com/Sigma-studios/bevy_ensemble/blob/master/docs/lobbies.md) — creating, joining, and leaving lobbies
//! - [Participants](https://github.com/Sigma-studios/bevy_ensemble/blob/master/docs/participants.md) — player roster and ownership
//! - [Messaging](https://github.com/Sigma-studios/bevy_ensemble/blob/master/docs/messaging.md) — sending and receiving custom messages
//! - [Writing a Backend](https://github.com/Sigma-studios/bevy_ensemble/blob/master/docs/backends.md) — implementing a platform backend
//!
//! # Testing a session
//!
//! `bevy_ensemble_loopback` is an in-process backend: N apps in one process on a clock you
//! control, over a link whose latency, jitter and loss are parameters. It is a real backend
//! rather than a mock — messages are encoded, routed by entity and decoded through the registry
//! exactly as they are on a socket — so netcode can have ordinary automated tests instead of two
//! windows and a stopwatch.

use bevy::prelude::*;

mod broadcast;
mod components;
mod messages;
#[cfg(feature = "netdebug")]
mod netdebug;
#[cfg(feature = "netmetrics")]
mod netmetrics;
#[cfg(feature = "netdebug")]
pub mod netsim;
pub(crate) mod observers;
mod ping;
mod player_data;
pub mod prelude;
pub mod registry;
mod session;
mod systems;
mod transport;
mod types;

pub use broadcast::{BroadcastLobbyMessage, LobbyBroadcastAppExt, LobbyBroadcastPlugin};
pub use components::*;
pub use messages::*;
#[cfg(feature = "netdebug")]
pub use netdebug::{NetDebugConfig, NetDebugExtras, NetDebugPlugin};
#[cfg(feature = "netmetrics")]
pub use netmetrics::{NetMetrics, NetMetricsPlugin};
#[cfg(feature = "netdebug")]
pub use netsim::{ChannelModel, NetPreset, NetSim, NetSimClock, NetSimConfig, NetSimPlugin};
pub use ping::{PeerLastPong, PeerRtt, PeerRttJitter, PeerWireRtt};
pub use player_data::{PlayerData, PlayerDataPlugin, SetPlayerData};
pub use registry::{EnsembleMessageRegistry, decode_ensemble_packet, encode_ensemble_message};
pub use session::{JoinLobby, LeaveLobby, RefreshLobbies};
pub use transport::{EnsembleTransportAppExt, TransportBackend};
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

/// System sets exposed by [`EnsemblePlugin`] so backends and games can order work around
/// the packet-receive boundary.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnsembleSet {
    /// The backend system that pulls bytes off the socket and dispatches them as
    /// [`ReceivedEnsembleMessage`](crate::ReceivedEnsembleMessage)s. Each transport backend
    /// puts its receive system in this set and runs it in [`PreUpdate`], so every
    /// [`Update`] reader — core handlers and game systems alike — sees a packet the same
    /// frame it arrives rather than a frame later. Order a system relative to this set to
    /// tighten it further if needed.
    ReceivePackets,
}

impl Plugin for EnsemblePlugin {
    fn build(&self, app: &mut App) {
        session::register_session_messages(app);

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
                    // Packets are drained in PreUpdate (see `EnsembleSet::ReceivePackets`),
                    // so these Update handlers already see this frame's pings/pongs.
                    ping::respond_to_pings,
                    ping::receive_pongs,
                    ping::tick_last_pong,
                ),
            );

        // The network debug overlay (+ its condition simulator and metrics) is opt-OUT:
        // installed automatically since the `netdebug` feature is on by default (release
        // included). It starts closed — press the toggle key (F3) to open it — and the
        // simulator sits on `NetPreset::Off` until you pick a profile in the panel, so it
        // does nothing until asked. To customize (key, starting preset, start visible),
        // add your own `NetDebugPlugin` *before* `EnsemblePlugin`; the guard below then
        // steps aside. To drop it entirely, build with `default-features = false`.
        //
        // (Metrics-only builds — `netmetrics` without `netdebug` — stay opt-in: add
        // `NetMetricsPlugin` yourself. `NetDebugPlugin` pulls the metrics in on its own.)
        #[cfg(feature = "netdebug")]
        if !app.is_plugin_added::<netdebug::NetDebugPlugin>() {
            app.add_plugins(netdebug::NetDebugPlugin::default());
        }
    }
}
