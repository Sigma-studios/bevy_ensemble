pub mod protocol;

#[cfg(feature = "server")]
pub mod server;

/// The `axum` the signalling server is written against.
///
/// Use it — `bevy_ensemble_webrtc::axum::extract::ws::WebSocket` — rather than depending on
/// `axum` yourself. [`server::handle_socket`] takes *this* crate's `WebSocket`, and two axum
/// versions in one graph are two unrelated types, so a host binary that pins its own copy gets a
/// mismatch at the one call it exists to make. Going through this re-export means the version is
/// this crate's business, which is where it belongs.
#[cfg(feature = "server")]
pub use axum;

#[cfg(feature = "client")]
mod connection;
#[cfg(feature = "client")]
mod handshake;
#[cfg(feature = "client")]
mod session;
#[cfg(feature = "client")]
mod systems;


#[cfg(feature = "client")]
use bevy::prelude::*;
#[cfg(feature = "client")]
use bevy_ensemble::{EnsembleAppExt, EnsembleTransportAppExt};
#[cfg(feature = "client")]
pub use bevy_ensemble_sockets::{IceServer, IceServers};
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
    /// The name this peer is *first* advertised under.
    ///
    /// Only the starting value: it is inserted as [`SignallingDisplayName`], and changing that
    /// resource re-sends it. A menu that lets a player type their name wants the resource, not
    /// this field.
    pub display_name: String,
    pub max_players: u32,
    /// The ICE servers peer connections gather candidates from.
    ///
    /// Defaults to the public STUN servers this crate has always hardcoded. Two peers on the same
    /// machine need none of them and wait on them anyway, so a local run or a test can pass
    /// [`IceServers::none()`] and skip straight to host candidates.
    pub ice_servers: IceServers,
}

#[cfg(feature = "client")]
impl Default for BevyEnsembleWebrtcPlugin {
    fn default() -> Self {
        Self {
            server_url: "ws://localhost:9090/ws".into(),
            display_name: "Player".into(),
            max_players: 8,
            ice_servers: IceServers::default(),
        }
    }
}

/// Log directives that silence ICE gathering noise, for a consumer's `LogPlugin` filter.
///
/// # Why this is a string and not a fix
///
/// A WebRTC connection emits roughly a dozen `WARN`s that mean nothing. Measured over one
/// two-peer session — 15 warning lines in total, of which **14 come from `webrtc_ice` and none
/// from this crate**:
///
/// | lines | message | cause |
/// |---|---|---|
/// | 8 | `could not listen udp fe80::…: Can't assign requested address` | link-local IPv6 addresses it enumerates and cannot bind |
/// | 4 | `pingAllCandidates called with no candidate pairs` | gathering, before any pair exists |
/// | 2 | `failed to resolve stun host: stun.l.google.com` | no IPv6 route to the public STUN servers |
///
/// They are `log::warn!` calls inside `webrtc-ice`, so nothing in `bevy_ensemble` can downgrade
/// them at the source — which is what makes "zero warnings from a clean run" unusable as a pass
/// bar, and that is the cheapest netcode check there is. What this crate *can* do is stop every
/// consumer rediscovering the target names by grepping a log.
///
/// The last two rows go away on their own if you pass [`IceServers::none()`](IceServers::none),
/// which is right for loopback and a LAN. The other twelve do not.
///
/// ```rust,ignore
/// DefaultPlugins.set(LogPlugin {
///     filter: format!("{},{}", default_filter, bevy_ensemble_webrtc::QUIET_ICE_LOG_FILTER),
///     ..default()
/// })
/// ```
#[cfg(feature = "client")]
pub const QUIET_ICE_LOG_FILTER: &str = "webrtc_ice::agent::agent_gather=error,\
     webrtc_ice::agent::agent_internal=error,\
     webrtc_ice::mdns=error";

/// The name this peer is advertised under in the lobby list, live.
///
/// Insert or change it and the new name is sent to the signalling server, which updates the
/// listing of any lobby this peer is hosting. Before this existed the name was fixed when the
/// socket was built — process start, for most consumers — so what a lobby was listed as had
/// nothing to do with what the player had called themselves.
///
/// Distinct from `PlayerData`, which is how a name reaches the *other players* in a session and
/// travels over the data channel. This one is only the signalling server's listing.
#[cfg(feature = "client")]
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct SignallingDisplayName(pub String);

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
    ice_servers: IceServers,
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
        let socket = bevy_ensemble_sockets::EnsembleSocket::new(self.runtime.handle().clone())
            .with_ice_servers(self.ice_servers.clone());
        #[cfg(target_arch = "wasm32")]
        let socket =
            bevy_ensemble_sockets::EnsembleSocket::new().with_ice_servers(self.ice_servers.clone());

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
            ice_servers: self.ice_servers.clone(),
        };

        let (socket, lobby_connection) = webrtc_runtime.build_socket();

        app.claim_transport("bevy_ensemble_webrtc")
            .insert_resource(webrtc_runtime)
            .insert_resource(lobby_connection)
            .insert_resource(socket)
            .insert_resource(SignallingDisplayName(self.display_name.clone()))
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
                    systems::publish_display_name,
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
            // Drain the socket in PreUpdate so every Update reader (core and game) sees
            // this frame's packets the same frame they arrived.
            .add_systems(
                PreUpdate,
                systems::read_peer_messages.in_set(bevy_ensemble::EnsembleSet::ReceivePackets),
            )
            // `bevy_ensemble`'s backend-neutral session requests, so a game does not have to
            // name this crate to host, list, join or leave. See `session`.
            .add_systems(
                Update,
                (
                    session::refresh_lobbies,
                    session::join_lobby,
                    session::leave_lobby,
                ),
            )
            .add_observer(systems::send_serialized_lobby_packet)
            .add_observer(systems::disconnect_removed_lobby_client);
    }
}
