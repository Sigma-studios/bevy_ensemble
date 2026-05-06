use std::sync::{Arc, Mutex};

use async_tungstenite::tungstenite::Message;
use bevy::log::{error, warn};
use bevy_ensemble_sockets::PeerSignal;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::protocol::{ClientMessage, ServerMessage, decode, encode};

use super::{LobbyEvent, dispatch_server_message};

/// Builder that starts the WebSocket handler task (native).
#[derive(Debug)]
pub(crate) struct WsHandlerBuilder {
    pub display_name: String,
    pub lobby_event_tx: mpsc::UnboundedSender<LobbyEvent>,
    pub lobby_command_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<ClientMessage>>>>,
    pub signal_tx: mpsc::UnboundedSender<(u128, PeerSignal)>,
    pub runtime_handle: tokio::runtime::Handle,
}

impl WsHandlerBuilder {
    pub fn start(self, server_url: String) {
        let lobby_command_rx = self
            .lobby_command_rx
            .lock()
            .unwrap()
            .take()
            .expect("WsHandlerBuilder::start called more than once");

        let lobby_event_tx = self.lobby_event_tx;
        let signal_tx = self.signal_tx;
        let display_name = self.display_name;

        self.runtime_handle.spawn(async move {
            let ws_result = async_tungstenite::tokio::connect_async(&server_url).await;
            let (ws_stream, _) = match ws_result {
                Ok(pair) => pair,
                Err(e) => {
                    error!("Failed to connect to signaling server at {server_url}: {e}");
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
                .send(Message::Binary(auth_bytes.into()))
                .await
                .is_err()
            {
                error!("Failed to send authentication message");
                return;
            }

            loop {
                tokio::select! {
                    Some(msg) = ws_source.next() => {
                        let Ok(msg) = msg else { break; };
                        let bytes = match msg {
                            Message::Binary(b) => b,
                            Message::Close(_) => break,
                            _ => continue,
                        };
                        let Ok(server_msg) = decode::<ServerMessage>(&bytes) else {
                            warn!("Failed to decode server message");
                            continue;
                        };
                        dispatch_server_message(server_msg, &signal_tx, &lobby_event_tx);
                    }

                    Some(cmd) = lobby_command_rx.recv() => {
                        let Some(bytes) = encode(&cmd) else {
                            warn!("Failed to serialize command");
                            continue;
                        };
                        if ws_sink.send(Message::Binary(bytes.into())).await.is_err() {
                            break;
                        }
                    }

                    else => break,
                }
            }
        });
    }
}
