use std::sync::Mutex;

use bevy::prelude::*;
use bevy_ensemble_sockets::PeerSignal;
use tokio::sync::mpsc;

use crate::protocol::{ClientMessage, ServerMessage};

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod wasm;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::WsHandlerBuilder;
#[cfg(target_arch = "wasm32")]
pub(crate) use self::wasm::WsHandlerBuilder;

/// Lobby-specific events forwarded to Bevy systems (not signaling events).
#[derive(Message, Debug)]
pub(crate) enum LobbyEvent {
    Welcome { player_uuid: u128 },
    LobbyCreated { lobby_id: u64 },
    LobbyJoined { lobby_id: u64 },
    LobbyError { reason: String },
    PlayerJoined { player_uuid: u128 },
    PlayerLeft { player_uuid: u128 },
    LobbyList { lobbies: Vec<crate::protocol::LobbyInfo> },
    Disconnected { reason: String },
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

        ServerMessage::LobbyCreated { lobby_id } => {
            let _ = lobby_event_tx.send(LobbyEvent::LobbyCreated { lobby_id });
        }
        ServerMessage::LobbyJoined { lobby_id, .. } => {
            let _ = lobby_event_tx.send(LobbyEvent::LobbyJoined { lobby_id });
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
    }
}
