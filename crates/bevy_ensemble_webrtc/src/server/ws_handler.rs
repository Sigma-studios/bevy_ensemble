use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::protocol::{ClientMessage, ServerMessage, decode, encode};

use super::lobby;
use super::state::{ConnectionHandle, ServerState};

pub async fn handle_socket(socket: WebSocket, state: Arc<ServerState>) {
    let (mut ws_sink, mut ws_stream) = socket.split();

    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

    let write_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let Some(bytes) = encode(&msg) else {
                error!("Failed to serialize server message: {msg:?}");
                continue;
            };
            if ws_sink.send(Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
    });

    let mut player_uuid: Option<u128> = None;

    while let Some(Ok(msg)) = ws_stream.next().await {
        let bytes = match msg {
            Message::Binary(b) => b,
            Message::Close(_) => break,
            _ => continue,
        };

        let Ok(client_msg) = decode::<ClientMessage>(&bytes) else {
            error!("Failed to decode client message");
            continue;
        };

        match client_msg {
            ClientMessage::Authenticate { display_name } => {
                if player_uuid.is_some() {
                    let _ = tx.send(ServerMessage::LobbyError {
                        reason: "Already authenticated".into(),
                    });
                    continue;
                }

                let uuid = Uuid::new_v4().as_u128();
                player_uuid = Some(uuid);

                state.connections.insert(
                    uuid,
                    ConnectionHandle {
                        display_name,
                        lobby_id: None,
                        sender: tx.clone(),
                    },
                );

                info!("Player authenticated: {uuid}");
                let _ = tx.send(ServerMessage::Welcome { player_uuid: uuid });
            }

            ClientMessage::CreateLobby { max_players } => {
                let Some(uuid) = player_uuid else {
                    let _ = tx.send(ServerMessage::LobbyError {
                        reason: "Not authenticated".into(),
                    });
                    continue;
                };

                let response = lobby::create_lobby(&state, uuid, max_players);
                let _ = tx.send(response);
            }

            ClientMessage::JoinLobby { lobby_id } => {
                let Some(uuid) = player_uuid else {
                    let _ = tx.send(ServerMessage::LobbyError {
                        reason: "Not authenticated".into(),
                    });
                    continue;
                };

                let response = lobby::join_lobby(&state, uuid, lobby_id);
                let _ = tx.send(response);
            }

            ClientMessage::JoinLobbyByCode { code } => {
                let Some(uuid) = player_uuid else {
                    let _ = tx.send(ServerMessage::LobbyError {
                        reason: "Not authenticated".into(),
                    });
                    continue;
                };

                let response = lobby::join_lobby_by_code(&state, uuid, &code);
                let _ = tx.send(response);
            }

            ClientMessage::LeaveLobby => {
                if let Some(uuid) = player_uuid {
                    lobby::leave_lobby(&state, uuid);
                }
            }

            ClientMessage::ListLobbies => {
                let response = lobby::list_lobbies(&state);
                let _ = tx.send(response);
            }

            ClientMessage::Signal {
                receiver_uuid,
                data,
            } => {
                let Some(from_uuid) = player_uuid else {
                    continue;
                };

                // Validate that sender and receiver are in the same lobby
                let sender_lobby = state
                    .connections
                    .get(&from_uuid)
                    .and_then(|c| c.lobby_id);
                let receiver_lobby = state
                    .connections
                    .get(&receiver_uuid)
                    .and_then(|c| c.lobby_id);

                match (sender_lobby, receiver_lobby) {
                    (Some(s), Some(r)) if s == r => {
                        info!("Relaying signal from {from_uuid} to {receiver_uuid}");
                        state.send_to(
                            receiver_uuid,
                            ServerMessage::Signal {
                                sender_uuid: from_uuid,
                                data,
                            },
                        );
                    }
                    _ => {
                        warn!(
                            "Rejecting signal relay from {from_uuid} to {receiver_uuid}: not in same lobby"
                        );
                    }
                }
            }

            ClientMessage::KeepAlive => {}
        }
    }

    if let Some(uuid) = player_uuid {
        info!("Player disconnected: {uuid}");
        lobby::leave_lobby(&state, uuid);
        state.connections.remove(&uuid);
    }

    write_task.abort();
}
