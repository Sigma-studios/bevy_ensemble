use bevy::prelude::*;

use crate::{
    Host, Lobby, LobbyClient, LobbyClientPlayerUuid, ReceivedEnsembleMessage,
    messages::{LobbyClientMessage, LobbyMessage},
};

const PING_INTERVAL_SECS: f32 = 1.0;

/// Internal ping message sent over data channels to measure RTT.
#[derive(Message, Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct EnsemblePing {
    /// Monotonic timestamp (seconds) on the sender's clock when the ping was sent.
    pub timestamp: f64,
}

/// Internal pong response echoing back the original ping timestamp.
#[derive(Message, Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct EnsemblePong {
    /// The original timestamp from the ping, echoed back unchanged.
    pub timestamp: f64,
}

/// Round-trip time to a connected peer, in seconds.
///
/// Added to `LobbyClient` entities on the host (one per peer) and to the
/// lobby entity on clients (single connection to the host).
#[derive(Component, Debug, Clone, Copy)]
pub struct PeerRtt(pub f64);

/// Both host and client send pings to all connected peers every 1 second.
///
/// Uses the standard [`LobbyMessage`] pipeline so pings are routed through
/// whichever transport backend is active.
pub(crate) fn send_pings(
    mut commands: Commands,
    lobbies: Query<Entity, With<Lobby>>,
    time: Res<Time>,
    mut cooldown: Local<f32>,
) {
    *cooldown -= time.delta_secs();
    if *cooldown > 0.0 {
        return;
    }
    *cooldown = PING_INTERVAL_SECS;

    let timestamp = time.elapsed_secs_f64();
    for lobby in lobbies.iter() {
        commands
            .entity(lobby)
            .trigger(move |entity| LobbyMessage {
                entity,
                message: EnsemblePing { timestamp },
            });
    }
}

/// When we receive a ping, immediately echo it back as a pong.
///
/// On the host: sends a targeted [`LobbyClientMessage`] back to the specific
/// client that sent the ping.
/// On a client: sends a [`LobbyMessage`] which routes to the host.
pub(crate) fn respond_to_pings(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<EnsemblePing>>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    lobby_clients: Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
) {
    for message in messages.read() {
        let Some(sender) = message.sender else {
            continue;
        };
        let pong = EnsemblePong {
            timestamp: message.message.timestamp,
        };

        if host_lobby.is_some() {
            // Host: respond to the specific client that sent this ping
            if let Some((client_entity, _)) =
                lobby_clients.iter().find(|(_, uuid)| uuid.0 == sender)
            {
                let pong = pong;
                commands
                    .entity(client_entity)
                    .trigger(move |entity| LobbyClientMessage {
                        entity,
                        message: pong,
                    });
            }
        } else if let Some(lobby) = client_lobby.as_ref() {
            // Client: respond to host via lobby message
            commands
                .entity(**lobby)
                .trigger(move |entity| LobbyMessage {
                    entity,
                    message: pong,
                });
        }
    }
}

/// When we receive a pong, compute RTT and store it.
///
/// On the host: updates `PeerRtt` on the `LobbyClient` entity for that peer.
/// On clients: updates `PeerRtt` on the lobby entity itself.
pub(crate) fn receive_pongs(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<EnsemblePong>>,
    time: Res<Time>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    lobby_clients: Query<(Entity, &LobbyClientPlayerUuid, Option<&PeerRtt>), With<LobbyClient>>,
    client_lobby_rtt: Query<Option<&PeerRtt>, (With<Lobby>, Without<Host>)>,
) {
    let now = time.elapsed_secs_f64();

    for message in messages.read() {
        let rtt = now - message.message.timestamp;
        if rtt < 0.0 {
            continue;
        }

        let Some(sender) = message.sender else {
            continue;
        };

        // Host side: find the LobbyClient entity for this sender
        if host_lobby.is_some() {
            if let Some((entity, _, existing_rtt)) = lobby_clients
                .iter()
                .find(|(_, uuid, _)| uuid.0 == sender)
            {
                let smoothed = match existing_rtt {
                    Some(prev) => 0.8 * prev.0 + 0.2 * rtt,
                    None => rtt,
                };
                commands.entity(entity).insert(PeerRtt(smoothed));
            }
        }

        // Client side: store on the lobby entity
        if let Some(lobby_entity) = client_lobby.as_ref() {
            let existing = client_lobby_rtt.get(**lobby_entity).ok().flatten();
            let smoothed = match existing {
                Some(prev) => 0.8 * prev.0 + 0.2 * rtt,
                None => rtt,
            };
            commands.entity(**lobby_entity).insert(PeerRtt(smoothed));
        }
    }
}
