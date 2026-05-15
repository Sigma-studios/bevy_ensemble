use bevy::prelude::*;
use bevy_ensemble::{
    EnsembleAppExt, EnsembleMessageRegistry, Host, Lobby, LobbyClient, ReceivedEnsembleMessage,
    encode_ensemble_message,
};

use crate::EnsembleSocketRes;
use crate::LobbyClientWebrtcUuid;

const PING_INTERVAL_SECS: f32 = 1.0;

/// Internal ping message sent over data channels to measure RTT.
#[derive(Message, Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct WebrtcPing {
    /// Monotonic timestamp (seconds) on the sender's clock when the ping was sent.
    pub timestamp: f64,
}

/// Internal pong response echoing back the original ping timestamp.
#[derive(Message, Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct WebrtcPong {
    /// The original timestamp from the ping, echoed back unchanged.
    pub timestamp: f64,
}

/// Round-trip time to a connected peer, in seconds.
///
/// Added to `LobbyClient` entities on the host (one per peer) and to the
/// lobby entity on clients (single connection to the host).
#[derive(Component, Debug, Clone, Copy)]
pub struct PeerRtt(pub f64);

pub(crate) struct PingPlugin;

impl Plugin for PingPlugin {
    fn build(&self, app: &mut App) {
        app.register_ensemble_message_type::<WebrtcPing>()
            .register_ensemble_message_type::<WebrtcPong>()
            .add_systems(
                Update,
                (send_pings, respond_to_pings, receive_pongs),
            );
    }
}

/// Both host and client send pings to all connected peers every 1 second.
fn send_pings(
    registry: Res<EnsembleMessageRegistry>,
    socket: ResMut<EnsembleSocketRes>,
    lobbies: Query<(), Or<(With<Lobby>, With<bevy_ensemble::PendingLobby>)>>,
    time: Res<Time>,
    mut cooldown: Local<f32>,
) {
    *cooldown -= time.delta_secs();
    if *cooldown > 0.0 {
        return;
    }
    *cooldown = PING_INTERVAL_SECS;

    if lobbies.is_empty() {
        return;
    }

    let ping = WebrtcPing {
        timestamp: time.elapsed_secs_f64(),
    };
    let packet = encode_ensemble_message(&registry, &ping);
    let data: Box<[u8]> = packet.into_boxed_slice();

    let peers: Vec<u128> = socket.connected_peers().collect();
    for peer in peers {
        socket.send(data.clone(), peer);
    }
}

/// When we receive a ping, immediately echo it back as a pong.
fn respond_to_pings(
    registry: Res<EnsembleMessageRegistry>,
    socket: ResMut<EnsembleSocketRes>,
    mut messages: MessageReader<ReceivedEnsembleMessage<WebrtcPing>>,
) {
    for message in messages.read() {
        let Some(sender) = message.sender else {
            continue;
        };
        let pong = WebrtcPong {
            timestamp: message.message.timestamp,
        };
        let packet = encode_ensemble_message(&registry, &pong);
        socket.send(packet.into_boxed_slice(), sender);
    }
}

/// When we receive a pong, compute RTT and store it.
///
/// On the host: updates `PeerRtt` on the `LobbyClient` entity for that peer.
/// On clients: updates `PeerRtt` on the lobby entity itself.
fn receive_pongs(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<WebrtcPong>>,
    time: Res<Time>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    lobby_clients: Query<(Entity, &LobbyClientWebrtcUuid, Option<&PeerRtt>), With<LobbyClient>>,
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
            let existing = client_lobby_rtt
                .get(**lobby_entity)
                .ok()
                .flatten();
            let smoothed = match existing {
                Some(prev) => 0.8 * prev.0 + 0.2 * rtt,
                None => rtt,
            };
            commands.entity(**lobby_entity).insert(PeerRtt(smoothed));
        }
    }
}
