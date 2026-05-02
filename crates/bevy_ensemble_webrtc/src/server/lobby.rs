use std::collections::HashSet;

use crate::protocol::{LobbyInfo, ServerMessage};

use super::state::{LobbyState, ServerState};

pub fn create_lobby(state: &ServerState, host_uuid: u128, max_players: u32) -> ServerMessage {
    let conn = state.connections.get(&host_uuid);
    let Some(conn) = conn else {
        return ServerMessage::LobbyError {
            reason: "Not authenticated".into(),
        };
    };

    if conn.lobby_id.is_some() {
        return ServerMessage::LobbyError {
            reason: "Already in a lobby".into(),
        };
    }

    let host_name = conn.display_name.clone();
    drop(conn);

    let lobby_id = state.next_lobby_id();
    let mut members = HashSet::new();
    members.insert(host_uuid);

    state.lobbies.insert(
        lobby_id,
        LobbyState {
            lobby_id,
            host_uuid,
            host_name,
            members,
            max_players,
        },
    );

    if let Some(mut conn) = state.connections.get_mut(&host_uuid) {
        conn.lobby_id = Some(lobby_id);
    }

    ServerMessage::LobbyCreated { lobby_id }
}

pub fn join_lobby(state: &ServerState, player_uuid: u128, lobby_id: u64) -> ServerMessage {
    let conn = state.connections.get(&player_uuid);
    let Some(conn) = conn else {
        return ServerMessage::LobbyError {
            reason: "Not authenticated".into(),
        };
    };

    if conn.lobby_id.is_some() {
        return ServerMessage::LobbyError {
            reason: "Already in a lobby".into(),
        };
    }
    drop(conn);

    let Some(mut lobby) = state.lobbies.get_mut(&lobby_id) else {
        return ServerMessage::LobbyError {
            reason: "Lobby not found".into(),
        };
    };

    if lobby.members.len() as u32 >= lobby.max_players {
        return ServerMessage::LobbyError {
            reason: "Lobby is full".into(),
        };
    }

    let host_uuid = lobby.host_uuid;
    let existing_members: Vec<u128> = lobby.members.iter().copied().collect();
    lobby.members.insert(player_uuid);
    let members_snapshot = lobby.members.clone();
    drop(lobby);

    if let Some(mut conn) = state.connections.get_mut(&player_uuid) {
        conn.lobby_id = Some(lobby_id);
    }

    state.send_to_all_except(
        &members_snapshot,
        player_uuid,
        ServerMessage::PlayerJoined { player_uuid },
    );

    ServerMessage::LobbyJoined {
        lobby_id,
        host_uuid,
        existing_members,
    }
}

pub fn leave_lobby(state: &ServerState, player_uuid: u128) {
    let lobby_id = {
        let Some(mut conn) = state.connections.get_mut(&player_uuid) else {
            return;
        };
        let Some(lobby_id) = conn.lobby_id.take() else {
            return;
        };
        lobby_id
    };

    let Some(mut lobby) = state.lobbies.get_mut(&lobby_id) else {
        return;
    };

    // Collect the peers the leaving player was connected to,
    // so we can send PeerLeft events to them AND to the leaving player.
    let peers_of_leaving: Vec<u128> = lobby
        .members
        .iter()
        .copied()
        .filter(|&uuid| uuid != player_uuid)
        .collect();

    lobby.members.remove(&player_uuid);

    if lobby.host_uuid == player_uuid {
        let remaining = lobby.members.clone();
        drop(lobby);
        state.lobbies.remove(&lobby_id);

        for &uuid in &remaining {
            state.send_to(
                uuid,
                ServerMessage::Disconnected {
                    reason: "Host left the lobby".into(),
                },
            );
            if let Some(mut conn) = state.connections.get_mut(&uuid) {
                conn.lobby_id = None;
            }
        }
    } else {
        let members_snapshot = lobby.members.clone();

        if lobby.members.is_empty() {
            drop(lobby);
            state.lobbies.remove(&lobby_id);
        } else {
            drop(lobby);
            state.send_to_all_except(
                &members_snapshot,
                player_uuid,
                ServerMessage::PlayerLeft { player_uuid },
            );
        }
    }

    // Notify the leaving player about all peers they were connected to,
    // so their matchbox instance can clean up the WebRTC connections.
    for &peer_uuid in &peers_of_leaving {
        state.send_to(
            player_uuid,
            ServerMessage::PlayerLeft {
                player_uuid: peer_uuid,
            },
        );
    }
}

pub fn list_lobbies(state: &ServerState) -> ServerMessage {
    let lobbies = state
        .lobbies
        .iter()
        .map(|entry| {
            let lobby = entry.value();
            LobbyInfo {
                lobby_id: lobby.lobby_id,
                host_name: lobby.host_name.clone(),
                player_count: lobby.members.len() as u32,
                max_players: lobby.max_players,
            }
        })
        .collect();

    ServerMessage::LobbyList { lobbies }
}
