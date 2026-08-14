//! Servicing `bevy_ensemble`'s backend-neutral session requests.
//!
//! Two of these are translations of something that already existed. The third,
//! [`leave_lobby`], is the reason this seam is worth having at all: leaving a Steam session is
//! not "despawn the lobby entity" the way it is over WebRTC — the P2P sessions have to be closed
//! and the Steam lobby left, in that order — and until now that knowledge lived in each *game*
//! that used this backend, which is exactly the wrong place for it.

use bevy::prelude::*;
use bevy_ensemble::{
    Host, JoinLobby, LeaveLobby, Lobby, LobbyClient, PendingLobby, PublicLobbies, PublicLobbyInfo,
    RefreshLobbies,
};
use bevy_steamworks::Client;

use crate::{
    JoinSteamLobby, LobbyClientSteamId, LobbySteamId, MAX_LOBBY_PLAYERS, SteamFriendLobbies,
};

/// Discovery restarts by dropping the cached list: `populate_friend_lobbies` refills it the
/// moment neither it nor its pending half is present.
pub(crate) fn refresh_lobbies(
    mut commands: Commands,
    mut requests: MessageReader<RefreshLobbies>,
    mut public_lobbies: ResMut<PublicLobbies>,
) {
    if requests.read().next().is_none() {
        return;
    }

    public_lobbies.0.clear();
    commands.remove_resource::<SteamFriendLobbies>();
}

/// Publish the friend-lobby list in the shape every backend publishes it in.
///
/// [`SteamFriendLobbies`] stays as it was — a game already written against it keeps working — but
/// a game that only wants "what can I join" should read [`PublicLobbies`] and not know which
/// backend answered. The `code` field is empty because Steam has no join codes; the id is the
/// lobby's raw bits, which is what [`JoinLobby`] expects back.
pub(crate) fn publish_public_lobbies(
    friend_lobbies: Option<Res<SteamFriendLobbies>>,
    mut public_lobbies: ResMut<PublicLobbies>,
) {
    let Some(friend_lobbies) = friend_lobbies else {
        return;
    };
    if !friend_lobbies.is_changed() {
        return;
    }

    public_lobbies.0 = friend_lobbies
        .0
        .iter()
        .map(|lobby| PublicLobbyInfo {
            lobby_id: lobby.lobby_id.raw(),
            code: String::new(),
            host_name: lobby.host_name.clone(),
            player_count: lobby.member_count as u32,
            max_players: MAX_LOBBY_PLAYERS,
        })
        .collect();
}

pub(crate) fn join_lobby(
    mut requests: MessageReader<JoinLobby>,
    mut joins: MessageWriter<JoinSteamLobby>,
) {
    for request in requests.read() {
        joins.write(JoinSteamLobby(crate::LobbyId::from_raw(request.0)));
    }
}

/// Close the P2P sessions, leave the Steam lobby, then despawn the entities.
///
/// Order matters: `leave_lobby` on a lobby we still hold open sessions into leaves those sessions
/// dangling until Steam times them out, and a peer that rejoins inside that window inherits a
/// half-dead connection.
pub(crate) fn leave_lobby(
    mut commands: Commands,
    mut requests: MessageReader<LeaveLobby>,
    steam_client: Res<Client>,
    host_lobbies: Query<(Entity, &LobbySteamId), (With<Lobby>, With<Host>)>,
    client_lobbies: Query<(Entity, &LobbySteamId), (With<Lobby>, Without<Host>)>,
    pending_lobbies: Query<Entity, With<PendingLobby>>,
    lobby_clients: Query<(Entity, &LobbyClientSteamId), With<LobbyClient>>,
) {
    if requests.read().next().is_none() {
        return;
    }

    for entity in pending_lobbies.iter() {
        commands.entity(entity).try_despawn();
    }

    if let Some((host_entity, lobby_id)) = host_lobbies.iter().next() {
        for (client_entity, client_steam_id) in lobby_clients.iter() {
            steam_client
                .networking()
                .close_p2p_session(client_steam_id.0);
            commands.entity(client_entity).try_despawn();
        }
        steam_client.matchmaking().leave_lobby(lobby_id.0);
        commands.entity(host_entity).try_despawn();
    }

    if let Some((client_entity, lobby_id)) = client_lobbies.iter().next() {
        let host = steam_client.matchmaking().lobby_owner(lobby_id.0);
        steam_client.networking().close_p2p_session(host);
        steam_client.matchmaking().leave_lobby(lobby_id.0);
        commands.entity(client_entity).try_despawn();
    }
}
