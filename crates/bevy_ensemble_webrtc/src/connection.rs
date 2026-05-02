use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use futures_util::{SinkExt, StreamExt};
use matchbox_protocol::PeerId;
use matchbox_socket::{
    PeerEvent, PeerRequest, PeerSignal, SignalingError, Signaller, SignallerBuilder,
};
use tokio::sync::mpsc;
use bevy::log::{error, info, warn};
use uuid::Uuid;

use crate::protocol::{ClientMessage, ServerMessage, decode, encode};

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
/// Signaling (SDP/ICE) flows through the `Signaller` trait impl inside matchbox.
#[derive(Resource)]
pub struct LobbyConnection {
    pub command_tx: mpsc::UnboundedSender<ClientMessage>,
    pub event_rx: Mutex<mpsc::UnboundedReceiver<LobbyEvent>>,
    pub local_player_uuid: Option<u128>,
}

/// Our custom signaller that bridges the signaling server to matchbox.
pub(crate) struct EnsembleSignaller {
    signal_rx: mpsc::UnboundedReceiver<PeerEvent>,
    command_tx: mpsc::UnboundedSender<ClientMessage>,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Signaller for EnsembleSignaller {
    async fn send(&mut self, request: PeerRequest) -> Result<(), SignalingError> {
        match &request {
            PeerRequest::Signal { receiver, data } => {
                info!("[signaller] Sending signal to {receiver}: {data:?}");
            }
            PeerRequest::KeepAlive => {}
        }
        let msg = match request {
            PeerRequest::Signal { receiver, data } => {
                let data_json = serde_json::to_string(&data)
                    .map_err(|e| SignalingError::UserImplementationError(e.to_string()))?;
                ClientMessage::Signal {
                    receiver_uuid: receiver.0.as_u128(),
                    data: data_json,
                }
            }
            PeerRequest::KeepAlive => ClientMessage::KeepAlive,
        };
        self.command_tx
            .send(msg)
            .map_err(|_| SignalingError::StreamExhausted)?;
        Ok(())
    }

    async fn next_message(&mut self) -> Result<PeerEvent, SignalingError> {
        let event = self
            .signal_rx
            .recv()
            .await
            .ok_or(SignalingError::StreamExhausted)?;
        match &event {
            PeerEvent::IdAssigned(id) => info!("[signaller] IdAssigned: {id}"),
            PeerEvent::NewPeer(id) => info!("[signaller] NewPeer: {id}"),
            PeerEvent::PeerLeft(id) => info!("[signaller] PeerLeft: {id}"),
            PeerEvent::Signal { sender, data } => {
                info!("[signaller] Signal from {sender}: {data:?}");
            }
        }
        Ok(event)
    }
}

/// Builder that creates our custom signaller and starts the WebSocket task.
#[derive(Debug)]
pub(crate) struct EnsembleSignallerBuilder {
    pub display_name: String,
    pub lobby_event_tx: mpsc::UnboundedSender<LobbyEvent>,
    pub lobby_command_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<ClientMessage>>>>,
    pub runtime_handle: tokio::runtime::Handle,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl SignallerBuilder for EnsembleSignallerBuilder {
    async fn new_signaller(
        &self,
        _attempts: Option<u16>,
        room_url: String,
    ) -> Result<Box<dyn Signaller>, SignalingError> {
        let (signal_tx, signal_rx) = mpsc::unbounded_channel::<PeerEvent>();
        let (ws_command_tx, mut ws_command_rx) = mpsc::unbounded_channel::<ClientMessage>();

        // Take the lobby command receiver (only happens once)
        let lobby_command_rx = self
            .lobby_command_rx
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| {
                SignalingError::UserImplementationError(
                    "SignallerBuilder::new_signaller called more than once".into(),
                )
            })?;

        let lobby_event_tx = self.lobby_event_tx.clone();
        let display_name = self.display_name.clone();

        // Spawn the WebSocket task on the shared Tokio runtime
        self.runtime_handle.spawn(async move {
            let ws_result = tokio_tungstenite::connect_async(&room_url).await;
            let (ws_stream, _) = match ws_result {
                Ok(pair) => pair,
                Err(e) => {
                    error!("Failed to connect to signaling server at {room_url}: {e}");
                    return;
                }
            };

            let (mut ws_sink, mut ws_source) = ws_stream.split();
            let mut lobby_command_rx = lobby_command_rx;

            // Authenticate
            let Some(auth_bytes) = encode(&ClientMessage::Authenticate { display_name }) else {
                error!("Failed to serialize authentication message");
                return;
            };
            if ws_sink
                .send(tokio_tungstenite::tungstenite::Message::Binary(
                    auth_bytes.into(),
                ))
                .await
                .is_err()
            {
                error!("Failed to send authentication message");
                return;
            }

            loop {
                tokio::select! {
                    Some(msg) = ws_source.next() => {
                        let Ok(msg) = msg else {
                            break;
                        };
                        let bytes = match msg {
                            tokio_tungstenite::tungstenite::Message::Binary(b) => b,
                            tokio_tungstenite::tungstenite::Message::Close(_) => break,
                            _ => continue,
                        };
                        let Ok(server_msg) = decode::<ServerMessage>(&bytes) else {
                            warn!("Failed to decode server message");
                            continue;
                        };

                        match server_msg {
                            // Goes to BOTH signaller and lobby
                            ServerMessage::Welcome { player_uuid } => {
                                let peer_id = PeerId(Uuid::from_u128(player_uuid));
                                let _ = signal_tx.send(PeerEvent::IdAssigned(peer_id));
                                let _ = lobby_event_tx.send(LobbyEvent::Welcome { player_uuid });
                            }
                            ServerMessage::PlayerJoined { player_uuid } => {
                                let peer_id = PeerId(Uuid::from_u128(player_uuid));
                                let _ = signal_tx.send(PeerEvent::NewPeer(peer_id));
                                let _ = lobby_event_tx.send(LobbyEvent::PlayerJoined { player_uuid });
                            }
                            ServerMessage::PlayerLeft { player_uuid } => {
                                let peer_id = PeerId(Uuid::from_u128(player_uuid));
                                let _ = signal_tx.send(PeerEvent::PeerLeft(peer_id));
                                let _ = lobby_event_tx.send(LobbyEvent::PlayerLeft { player_uuid });
                            }

                            // Goes to signaller only
                            ServerMessage::Signal { sender_uuid, data } => {
                                let peer_id = PeerId(Uuid::from_u128(sender_uuid));
                                if let Ok(peer_signal) = serde_json::from_str::<PeerSignal>(&data) {
                                    let _ = signal_tx.send(PeerEvent::Signal {
                                        sender: peer_id,
                                        data: peer_signal,
                                    });
                                }
                            }

                            // Goes to lobby only
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

                    // Signaling commands from matchbox (SDP/ICE)
                    Some(cmd) = ws_command_rx.recv() => {
                        let Some(bytes) = encode(&cmd) else {
                            warn!("Failed to serialize signaling command");
                            continue;
                        };
                        if ws_sink
                            .send(tokio_tungstenite::tungstenite::Message::Binary(bytes.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }

                    // Lobby commands from Bevy systems
                    Some(cmd) = lobby_command_rx.recv() => {
                        let Some(bytes) = encode(&cmd) else {
                            warn!("Failed to serialize lobby command");
                            continue;
                        };
                        if ws_sink
                            .send(tokio_tungstenite::tungstenite::Message::Binary(bytes.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }

                    else => break,
                }
            }
        });

        Ok(Box::new(EnsembleSignaller {
            signal_rx,
            command_tx: ws_command_tx,
        }))
    }
}
