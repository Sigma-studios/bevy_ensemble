use bevy::prelude::*;
use bevy_ensemble::{
    EnsembleMessageRegistry, Host, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyParticipantOf,
    PendingLobby, ReceivedEnsembleMessage, encode_ensemble_message,
};

use crate::{
    EnsembleSocketRes, LobbyClientWebrtcUuid, LobbyHostUuid, LobbyWebrtcId,
    PendingWebrtcLobbyClient,
};

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
    client_lobbies: Query<&LobbyWebrtcId, (Without<Host>, Or<(With<PendingLobby>, With<Lobby>)>)>,
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

/// Promote a client's pending lobby once its *host* says it is ready.
///
/// "Its host" is the peer the signalling server named in `LobbyJoined`, held on the lobby as
/// [`LobbyHostUuid`]. A handshake claiming `from_host` from any other peer is ignored: the claim
/// is a byte in a packet anybody can send, and acting on it would let whoever sent it first
/// decide when this client believes it is in a session. The registry refuses such a packet before
/// it is decoded (`HostOnly`); this is the same rule applied where the decision is made.
pub(crate) fn promote_client_lobby_on_host_handshake(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<WebrtcReadyHandshake>>,
    pending_client_lobbies: Query<
        (Entity, Option<&LobbyHostUuid>),
        (With<PendingLobby>, Without<Lobby>, Without<Host>),
    >,
) {
    for message in messages.read() {
        if !message.message.from_host {
            continue;
        }

        let Some((entity, host)) = pending_client_lobbies.iter().next() else {
            continue;
        };
        let Some(host) = host else {
            debug!(
                "ignoring a host handshake from {:#x?}: this client has not been told who its \
                 host is yet",
                message.sender
            );
            continue;
        };
        if !handshake_is_from_host(host.0, message.sender) {
            warn!(
                "ignoring a handshake claiming to be from the host: it came from {:#x?} and the \
                 host is {:#x}",
                message.sender, host.0
            );
            continue;
        }

        commands
            .entity(entity)
            .remove::<PendingLobby>()
            .insert(Lobby);
    }
}

/// Whether a handshake that claims `from_host` actually came from `host`.
///
/// `None` is a locally self-delivered message, which is not the host either.
pub(crate) fn handshake_is_from_host(host: u128, sender: Option<u128>) -> bool {
    sender == Some(host)
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

#[cfg(test)]
mod tests {
    use bevy::prelude::*;
    use bevy_ensemble::{
        EnsembleAppExt, EnsemblePlugin, Host, HostUuid, Lobby, MessageAuthority, PendingLobby,
        ReceivedEnsembleMessage,
    };

    use super::{
        WebrtcReadyHandshake, handshake_is_from_host, promote_client_lobby_on_host_handshake,
    };
    use crate::{LobbyHostUuid, LobbyWebrtcId};

    const HOST: u128 = 0xA;
    const OTHER: u128 = 0xB;

    /// The promotion path with no socket under it: the handshake is written straight into the
    /// message queue, as the decode step would after a packet came off a data channel.
    fn app_with_a_pending_join() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, EnsemblePlugin))
            .register_backend_handshake_message_type::<WebrtcReadyHandshake>(
                "bevy_ensemble_webrtc/ReadyHandshake",
                MessageAuthority::HostOnly,
            )
            .add_systems(Update, promote_client_lobby_on_host_handshake)
            .insert_resource(HostUuid(HOST));
        app.world_mut()
            .spawn((PendingLobby, LobbyWebrtcId(1), LobbyHostUuid(HOST)));
        app
    }

    fn write_host_handshake(app: &mut App, sender: u128) {
        app.world_mut().write_message(ReceivedEnsembleMessage {
            sender: Some(sender),
            message: WebrtcReadyHandshake { from_host: true },
            received_at: bevy_ensemble::Instant::now(),
        });
    }

    fn lobby_is_promoted(app: &mut App) -> bool {
        let world = app.world_mut();
        let promoted = world
            .query_filtered::<(), (With<Lobby>, Without<PendingLobby>, Without<Host>)>()
            .iter(world)
            .count();
        let pending = world
            .query_filtered::<(), (With<PendingLobby>, Without<Host>)>()
            .iter(world)
            .count();
        assert_eq!(
            promoted + pending,
            1,
            "the lobby entity should still exist, once"
        );
        promoted == 1
    }

    #[test]
    fn a_handshake_claiming_host_from_another_peer_is_ignored() {
        let mut app = app_with_a_pending_join();

        write_host_handshake(&mut app, OTHER);
        app.update();
        assert!(
            !lobby_is_promoted(&mut app),
            "a `from_host` handshake from a peer that is not the host must not promote the lobby"
        );

        write_host_handshake(&mut app, HOST);
        app.update();
        assert!(
            lobby_is_promoted(&mut app),
            "the host's own handshake promotes it"
        );
    }

    #[test]
    fn the_host_check_is_on_the_sender_not_the_claim() {
        assert!(handshake_is_from_host(HOST, Some(HOST)));
        assert!(!handshake_is_from_host(HOST, Some(OTHER)));
        assert!(!handshake_is_from_host(HOST, None));
    }
}
