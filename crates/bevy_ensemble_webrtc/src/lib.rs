pub mod protocol;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "client")]
mod connection;
#[cfg(feature = "client")]
mod handshake;
#[cfg(feature = "client")]
mod systems;


#[cfg(feature = "client")]
use bevy::prelude::*;
#[cfg(feature = "client")]
use bevy_ensemble::EnsembleAppExt;
#[cfg(feature = "client")]
pub use bevy_ensemble::PeerRtt;

/// Bevy plugin for WebRTC P2P networking via a signaling server.
///
/// Connects to a signaling server for lobby management, then uses
/// bevy_ensemble_sockets for cross-platform WebRTC data channels
/// (works on both native and WASM).
#[cfg(feature = "client")]
pub struct BevyEnsembleWebrtcPlugin {
    pub server_url: String,
    pub display_name: String,
    pub max_players: u32,
}

#[cfg(feature = "client")]
impl Default for BevyEnsembleWebrtcPlugin {
    fn default() -> Self {
        Self {
            server_url: "ws://localhost:9090/ws".into(),
            display_name: "Player".into(),
            max_players: 8,
        }
    }
}

/// Marks a lobby entity with its server-assigned lobby ID.
#[cfg(feature = "client")]
#[derive(Component)]
pub struct LobbyWebrtcId(pub u64);

/// Marks a lobby client entity with the remote peer's player UUID.
#[cfg(feature = "client")]
#[derive(Component)]
pub struct LobbyClientWebrtcUuid(pub u128);

/// Temporary marker for lobby client entities awaiting handshake completion.
#[cfg(feature = "client")]
#[derive(Component)]
pub struct PendingWebrtcLobbyClient;

/// Write this message to request a lobby list refresh from the signaling server.
#[cfg(feature = "client")]
#[derive(Message, Clone, Copy, Debug)]
pub struct RefreshLobbyList;

/// Write this message to join a lobby by its server-assigned ID.
#[cfg(feature = "client")]
#[derive(Message, Clone, Copy, Debug)]
pub struct JoinWebrtcLobby(pub u64);

/// Write this message to join a lobby by its 4-letter code.
#[cfg(feature = "client")]
#[derive(Message, Clone, Debug)]
pub struct JoinWebrtcLobbyByCode(pub String);

/// Stores the lobby's short join code, assigned by the signaling server.
#[cfg(feature = "client")]
#[derive(Component)]
pub struct LobbyWebrtcCode(pub String);


/// Newtype wrapper around [`bevy_ensemble_sockets::EnsembleSocket`] so it can be used as a Bevy Resource.
#[cfg(feature = "client")]
#[derive(Resource, Deref, DerefMut)]
pub(crate) struct EnsembleSocketRes(bevy_ensemble_sockets::EnsembleSocket);

/// Holds the Tokio runtime (native only) and plugin config so the socket can be recreated
/// when leaving and rejoining lobbies.
#[cfg(feature = "client")]
#[derive(Resource)]
pub(crate) struct WebrtcRuntime {
    #[cfg(not(target_arch = "wasm32"))]
    runtime: tokio::runtime::Runtime,
    server_url: String,
    display_name: String,
    pub(crate) max_players: u32,
}

#[cfg(feature = "client")]
impl WebrtcRuntime {
    /// Build a fresh EnsembleSocket + lobby connection and start the WS handler task.
    /// Called at init and again each time the player leaves a lobby.
    pub(crate) fn build_socket(
        &self,
    ) -> (EnsembleSocketRes, connection::LobbyConnection) {
        use std::sync::{Arc, Mutex};

        use connection::WsHandlerBuilder;
        use tokio::sync::mpsc;

        let (lobby_event_tx, lobby_event_rx) = mpsc::unbounded_channel();
        let (lobby_command_tx, lobby_command_rx) = mpsc::unbounded_channel();
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();

        let ws_builder = WsHandlerBuilder {
            display_name: self.display_name.clone(),
            lobby_event_tx,
            lobby_command_rx: Arc::new(Mutex::new(Some(lobby_command_rx))),
            signal_tx,
            #[cfg(not(target_arch = "wasm32"))]
            runtime_handle: self.runtime.handle().clone(),
        };

        ws_builder.start(self.server_url.clone());

        // Create the EnsembleSocket
        #[cfg(not(target_arch = "wasm32"))]
        let socket = bevy_ensemble_sockets::EnsembleSocket::new(self.runtime.handle().clone());
        #[cfg(target_arch = "wasm32")]
        let socket = bevy_ensemble_sockets::EnsembleSocket::new();

        let lobby_connection = connection::LobbyConnection {
            command_tx: lobby_command_tx,
            event_rx: std::sync::Mutex::new(lobby_event_rx),
            signal_rx: std::sync::Mutex::new(signal_rx),
            local_player_uuid: None,
        };

        (EnsembleSocketRes(socket), lobby_connection)
    }
}

#[cfg(feature = "client")]
impl Plugin for BevyEnsembleWebrtcPlugin {
    fn build(&self, app: &mut App) {
        let webrtc_runtime = WebrtcRuntime {
            #[cfg(not(target_arch = "wasm32"))]
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Failed to create Tokio runtime"),
            server_url: self.server_url.clone(),
            display_name: self.display_name.clone(),
            max_players: self.max_players,
        };

        let (socket, lobby_connection) = webrtc_runtime.build_socket();

        app.insert_resource(webrtc_runtime)
            .insert_resource(lobby_connection)
            .insert_resource(socket)
            .add_message::<connection::LobbyEvent>()
            .add_message::<JoinWebrtcLobby>()
            .add_message::<JoinWebrtcLobbyByCode>()
            .add_message::<RefreshLobbyList>()
            .register_ensemble_message_type::<handshake::WebrtcReadyHandshake>()
            .add_systems(
                Update,
                (
                    systems::flush_lobby_events,
                    systems::apply_lobby_events,
                )
                    .chain(),
            )
            .add_systems(
                Update,
                (
                    systems::create_lobby,
                    systems::join_requested_lobbies,
                    systems::join_requested_lobbies_by_code,
                    systems::refresh_lobby_list,
                    systems::poll_socket_peers,
                    systems::pump_socket_signals,
                    handshake::send_client_handshakes,
                    handshake::send_host_handshakes,
                    handshake::promote_client_lobby_on_host_handshake,
                    handshake::promote_host_client_on_client_handshake,
                    systems::detect_lobby_leave,
                ),
            )
            .add_systems(Update, systems::read_peer_messages)
            .add_observer(systems::send_serialized_lobby_packet)
            .add_observer(systems::disconnect_removed_lobby_client);
    }
}
