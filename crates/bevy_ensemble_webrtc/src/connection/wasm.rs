use std::sync::{Arc, Mutex};

use bevy::log::{error, warn};
use bevy_ensemble_sockets::PeerSignal;
use futures_util::{FutureExt, SinkExt, StreamExt};
use tokio::sync::mpsc;
use ws_stream_wasm::{WsMessage, WsMeta};

use crate::protocol::{ClientMessage, ServerMessage, decode, encode};

use super::{LobbyEvent, dispatch_server_message};

/// Builder that starts the WebSocket handler task (WASM).
#[derive(Debug)]
pub(crate) struct WsHandlerBuilder {
    pub display_name: String,
    pub lobby_event_tx: mpsc::UnboundedSender<LobbyEvent>,
    pub lobby_command_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<ClientMessage>>>>,
    pub signal_tx: mpsc::UnboundedSender<(u128, PeerSignal)>,
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

        wasm_bindgen_futures::spawn_local(async move {
            let ws_result = WsMeta::connect(&server_url, None).await;
            let (_, ws_stream) = match ws_result {
                Ok(pair) => pair,
                Err(e) => {
                    error!("Failed to connect to signaling server at {server_url}: {e}");
                    let _ = lobby_event_tx.send(LobbyEvent::SignallingClosed);
                    return;
                }
            };

            let (mut ws_sink, ws_source) = ws_stream.split();
            let mut ws_source = ws_source.fuse();
            let mut lobby_command_rx = lobby_command_rx;

            // Authenticate
            let Some(auth_bytes) = encode(&ClientMessage::Authenticate { display_name }) else {
                error!("Failed to serialize authentication message");
                return;
            };
            if ws_sink.send(WsMessage::Binary(auth_bytes)).await.is_err() {
                error!("Failed to send authentication message");
                let _ = lobby_event_tx.send(LobbyEvent::SignallingClosed);
                return;
            }

            // Whether the loop ended because the server went away, as opposed to the plugin
            // dropping its end of the command channel to rebuild. Only the former is news.
            let mut lost = false;
            loop {
                futures_util::select! {
                    msg = ws_source.next() => {
                        // `None` is the socket closing, however it closed.
                        let Some(msg) = msg else { lost = true; break; };
                        let bytes = match msg {
                            WsMessage::Binary(b) => b,
                            WsMessage::Text(_) => continue,
                        };
                        let Ok(server_msg) = decode::<ServerMessage>(&bytes) else {
                            warn!("Failed to decode server message");
                            continue;
                        };
                        dispatch_server_message(server_msg, &signal_tx, &lobby_event_tx);
                    }

                    cmd = lobby_command_rx.recv().fuse() => {
                        let Some(cmd) = cmd else { break; };
                        let Some(bytes) = encode(&cmd) else {
                            warn!("Failed to serialize command");
                            continue;
                        };
                        if ws_sink.send(WsMessage::Binary(bytes)).await.is_err() {
                            lost = true;
                            break;
                        }
                    }

                    complete => break,
                }
            }
            if lost {
                warn!("the connection to the signaling server at {server_url} was lost");
                let _ = lobby_event_tx.send(LobbyEvent::SignallingClosed);
            }
        });
    }
}
