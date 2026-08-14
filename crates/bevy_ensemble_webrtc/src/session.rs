//! Servicing `bevy_ensemble`'s backend-neutral session requests.
//!
//! Thin by design: each of these turns one neutral request into the WebRTC-specific thing that
//! already existed, so a game can drive a session without naming this crate. The behaviour is
//! unchanged — what moves is which side of the seam the translation happens on.

use bevy::prelude::*;
use bevy_ensemble::{JoinLobby, LeaveLobby, Lobby, PendingLobby, RefreshLobbies};

use crate::{JoinWebrtcLobby, RefreshLobbyList};

pub(crate) fn refresh_lobbies(
    mut requests: MessageReader<RefreshLobbies>,
    mut refresh: MessageWriter<RefreshLobbyList>,
) {
    if requests.read().next().is_some() {
        refresh.write(RefreshLobbyList);
    }
}

pub(crate) fn join_lobby(
    mut requests: MessageReader<JoinLobby>,
    mut joins: MessageWriter<JoinWebrtcLobby>,
) {
    for request in requests.read() {
        joins.write(JoinWebrtcLobby(request.0));
    }
}

/// Despawning the lobby entity *is* the teardown here.
///
/// `detect_lobby_leave` watches for `LobbyWebrtcId` going away, tells the signalling server, drops
/// every peer connection and rebuilds the socket, so the next host or join starts clean. Nothing
/// else to do, and doing it twice would race that rebuild.
pub(crate) fn leave_lobby(
    mut commands: Commands,
    mut requests: MessageReader<LeaveLobby>,
    lobbies: Query<Entity, With<Lobby>>,
    pending_lobbies: Query<Entity, With<PendingLobby>>,
) {
    if requests.read().next().is_none() {
        return;
    }

    for entity in pending_lobbies.iter().chain(lobbies.iter()) {
        commands.entity(entity).try_despawn();
    }
}
