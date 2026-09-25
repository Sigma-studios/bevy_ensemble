use std::sync::Mutex;

use bevy::prelude::*;
use bevy_ensemble_sockets::PeerSignal;
use tokio::sync::mpsc;

use crate::protocol::{ClientMessage, ServerMessage};

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod wasm;

#[cfg(target_arch = "wasm32")]
pub(crate) use self::wasm::WsHandlerBuilder;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::WsHandlerBuilder;

/// Lobby-specific events forwarded to Bevy systems (not signaling events).
#[derive(Message, Debug)]
pub(crate) enum LobbyEvent {
    Welcome {
        player_uuid: u128,
    },
    LobbyCreated {
        lobby_id: u64,
        code: String,
    },
    LobbyJoined {
        lobby_id: u64,
        /// Who runs the lobby that was joined. The one fact a client's trust rests on: it is the
        /// only peer whose offer is answered, whose packets are read, and whose loss ends the
        /// session.
        host_uuid: u128,
        /// The other members at the time of joining. Informational: a client connects to its
        /// host only, never to them.
        existing_members: Vec<u128>,
    },
    LobbyError {
        reason: String,
    },
    PlayerJoined {
        player_uuid: u128,
    },
    PlayerLeft {
        player_uuid: u128,
    },
    LobbyList {
        lobbies: Vec<crate::protocol::LobbyInfo>,
    },
    Disconnected {
        reason: String,
    },
    /// The lobby this peer created or joined outlives its host. See
    /// [`ServerMessage::LobbyMigratable`].
    LobbyMigratable {
        idle_timeout_secs: u32,
    },
    /// The host left and `new_host` hosts the lobby now. See [`ServerMessage::HostChanged`].
    HostChanged {
        lobby_id: u64,
        previous_host: u128,
        new_host: u128,
        code: String,
        members: Vec<u128>,
    },
    /// The WebSocket to the signalling server is gone: it closed, errored, or never opened.
    ///
    /// Not sent when this side dropped the connection itself (a rebuild after leaving a lobby)
    /// — that is a teardown already under way, not a loss.
    SignallingClosed,
}

/// Resource for lobby-level communication with the signaling server.
///
/// Lobby commands (create, join, leave, list) flow through this resource.
/// Incoming WebRTC signals arrive via `signal_rx`.
#[derive(Resource)]
pub struct LobbyConnection {
    pub command_tx: mpsc::UnboundedSender<ClientMessage>,
    pub event_rx: Mutex<mpsc::UnboundedReceiver<LobbyEvent>>,
    pub signal_rx: Mutex<mpsc::UnboundedReceiver<(u128, PeerSignal)>>,
    pub local_player_uuid: Option<u128>,
    /// Set once [`LobbyEvent::SignallingClosed`] has been seen: this connection can carry nothing
    /// more, and a fresh one has to be built before hosting or joining again.
    pub signalling_lost: bool,
    /// The name this connection authenticated with: what the server believes, on this socket, until
    /// a `SetDisplayName` changes it.
    ///
    /// Here rather than in a `Local` beside the system that publishes it, because it is a property
    /// of the connection and should die with it. A `Local` outlives the socket, and both of the
    /// ways this connection gets rebuilt — leaving a lobby, recovering a dropped signalling socket
    /// — then leave it describing a conversation that is over, so the name is never re-sent and
    /// the server keeps whatever the fresh socket authenticated with.
    pub announced_name: String,
}

/// Dispatch a decoded server message to the lobby event and/or signal channels.
pub(crate) fn dispatch_server_message(
    server_msg: ServerMessage,
    signal_tx: &mpsc::UnboundedSender<(u128, PeerSignal)>,
    lobby_event_tx: &mpsc::UnboundedSender<LobbyEvent>,
) {
    match server_msg {
        ServerMessage::Welcome { player_uuid } => {
            let _ = lobby_event_tx.send(LobbyEvent::Welcome { player_uuid });
        }
        ServerMessage::PlayerJoined { player_uuid } => {
            let _ = lobby_event_tx.send(LobbyEvent::PlayerJoined { player_uuid });
        }
        ServerMessage::PlayerLeft { player_uuid } => {
            let _ = lobby_event_tx.send(LobbyEvent::PlayerLeft { player_uuid });
        }

        ServerMessage::Signal { sender_uuid, data } => {
            match serde_json::from_str::<PeerSignal>(&data) {
                Ok(peer_signal) => {
                    let _ = signal_tx.send((sender_uuid, peer_signal));
                }
                Err(e) => {
                    warn!("Failed to parse PeerSignal from {sender_uuid}: {e}");
                }
            }
        }

        ServerMessage::LobbyCreated { lobby_id, code } => {
            let _ = lobby_event_tx.send(LobbyEvent::LobbyCreated { lobby_id, code });
        }
        ServerMessage::LobbyJoined {
            lobby_id,
            host_uuid,
            existing_members,
        } => {
            let _ = lobby_event_tx.send(LobbyEvent::LobbyJoined {
                lobby_id,
                host_uuid,
                existing_members,
            });
        }
        ServerMessage::LobbyError { reason } => {
            let _ = lobby_event_tx.send(LobbyEvent::LobbyError { reason });
        }
        ServerMessage::LobbyList { lobbies } => {
            let _ = lobby_event_tx.send(LobbyEvent::LobbyList { lobbies });
        }
        ServerMessage::Disconnected { reason } => {
            let _ = lobby_event_tx.send(LobbyEvent::Disconnected { reason });
        }
        ServerMessage::LobbyMigratable {
            idle_timeout_secs, ..
        } => {
            let _ = lobby_event_tx.send(LobbyEvent::LobbyMigratable { idle_timeout_secs });
        }
        ServerMessage::HostChanged {
            lobby_id,
            previous_host,
            new_host,
            code,
            members,
        } => {
            let _ = lobby_event_tx.send(LobbyEvent::HostChanged {
                lobby_id,
                previous_host,
                new_host,
                code,
                members,
            });
        }
    }
}
