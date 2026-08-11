use bevy::prelude::*;
use bevy_ensemble::{
    EnsembleMessageRegistry, Host, Lobby, LobbyClient, LobbyClientPlayerUuid,
    LobbyParticipantOf, PendingLobby, ReceivedEnsembleMessage, encode_ensemble_message,
};

use crate::{EnsembleSocketRes, LobbyClientWebrtcUuid, LobbyWebrtcId, PendingWebrtcLobbyClient};

/// How often a peer restates its readiness handshake, in seconds.
///
/// Only the *repeat* rate. The first one goes out on the frame a lobby appears.
const HANDSHAKE_INTERVAL: f32 = 0.5;

/// Internal handshake message exchanged over data channels to confirm readiness.
#[derive(Message, Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct WebrtcReadyHandshake {
    pub from_host: bool,
}

/// Sends client handshakes to the host peer.
///
/// IMPORTANT: This must keep sending even after the client lobby is promoted from
/// `PendingLobby` to `Lobby`. The host creates its `PendingWebrtcLobbyClient` entity
/// from the `PlayerJoined` signaling event, which can arrive AFTER the host's own
/// handshake has already promoted the client. If we stop sending here, the host may
/// never receive a client handshake to promote with.
pub(crate) fn send_client_handshakes(
    registry: Res<EnsembleMessageRegistry>,
    socket: ResMut<EnsembleSocketRes>,
    client_lobbies: Query<
        &LobbyWebrtcId,
        (Without<Host>, Or<(With<PendingLobby>, With<Lobby>)>),
    >,
    time: Res<Time>,
    mut cooldown: Local<f32>,
) {
    // Emptiness first, cooldown second. The other way round spends the timer while
    // there is nothing to send, so on the frame a lobby finally appears the timer is
    // mid-cycle and the first handshake waits for up to a full period. That period is
    // exactly the window in which the data channel is up but the lobby is not yet
    // promoted -- so every "am I in a session yet" test written against `With<Lobby>`
    // is false while game traffic is already flowing.
    if client_lobbies.is_empty() {
        return;
    }
    *cooldown -= time.delta_secs();
    if *cooldown > 0.0 {
        return;
    }
    *cooldown = HANDSHAKE_INTERVAL;

    let packet = encode_ensemble_message(&registry, &WebrtcReadyHandshake { from_host: false });
    let data: Box<[u8]> = packet.into_boxed_slice();

    let peers: Vec<u128> = socket.connected_peers().collect();
    for peer in peers {
        socket.send(data.clone(), peer);
    }
}

pub(crate) fn send_host_handshakes(
    registry: Res<EnsembleMessageRegistry>,
    socket: ResMut<EnsembleSocketRes>,
    host_lobbies: Query<&LobbyWebrtcId, (With<Lobby>, With<Host>)>,
    time: Res<Time>,
    mut cooldown: Local<f32>,
) {
    // Emptiness first, cooldown second. The other way round spends the timer while
    // there is nothing to send, so on the frame a lobby finally appears the timer is
    // mid-cycle and the first handshake waits for up to a full period. That period is
    // exactly the window in which the data channel is up but the lobby is not yet
    // promoted -- so every "am I in a session yet" test written against `With<Lobby>`
    // is false while game traffic is already flowing.
    if host_lobbies.is_empty() {
        return;
    }
    *cooldown -= time.delta_secs();
    if *cooldown > 0.0 {
        return;
    }
    *cooldown = HANDSHAKE_INTERVAL;

    let packet = encode_ensemble_message(&registry, &WebrtcReadyHandshake { from_host: true });
    let data: Box<[u8]> = packet.into_boxed_slice();

    let peers: Vec<u128> = socket.connected_peers().collect();
    for peer in peers {
        socket.send(data.clone(), peer);
    }
}

pub(crate) fn promote_client_lobby_on_host_handshake(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<WebrtcReadyHandshake>>,
    pending_client_lobbies: Query<Entity, (With<PendingLobby>, Without<Lobby>, Without<Host>)>,
) {
    for message in messages.read() {
        if !message.message.from_host {
            continue;
        }

        let Some(entity) = pending_client_lobbies.iter().next() else {
            continue;
        };

        commands
            .entity(entity)
            .remove::<PendingLobby>()
            .insert(Lobby);
    }
}

pub(crate) fn promote_host_client_on_client_handshake(
    mut commands: Commands,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    mut messages: MessageReader<ReceivedEnsembleMessage<WebrtcReadyHandshake>>,
    pending_clients: Query<
        (Entity, &LobbyClientPlayerUuid, &LobbyParticipantOf),
        (With<PendingWebrtcLobbyClient>, With<LobbyClientWebrtcUuid>),
    >,
) {
    let Some(host_lobby) = host_lobby else {
        return;
    };

    for message in messages.read() {
        if message.message.from_host {
            continue;
        }

        let Some(sender) = message.sender else {
            continue;
        };

        let Some((entity, _, _)) =
            pending_clients
                .iter()
                .find(|(_, player_uuid, participant_of)| {
                    participant_of.0 == *host_lobby && player_uuid.0 == sender
                })
        else {
            continue;
        };

        commands
            .entity(entity)
            .remove::<PendingWebrtcLobbyClient>()
            .insert(LobbyClient);
    }
}
