//! Creating, joining, leaving and closing lobbies.
//!
//! # Everything about a lobby's membership is said while the lobby is held
//!
//! A lobby's members hear about it from more than one connection's task at once: a joiner's own
//! reply comes from its handler, while a host leaving is announced from the host's. With the lobby
//! released before sending, the two could interleave, and a joiner could be told who the new host
//! is before being told it had joined at all. So every message about membership is queued while
//! the lobby's map entry is held. Queueing never waits (each connection's channel is unbounded),
//! and the lock order is always `lobbies` before `connections`, never the reverse.

use dashmap::Entry;
use rand::Rng;

use crate::protocol::{CAPABILITY_HOST_MIGRATION, LobbyInfo, ServerMessage};

use super::state::{LobbyState, ServerState};

/// The most players a lobby may be created for, whatever the client asked.
///
/// `max_players` arrives from the client and was stored as is, so a client could declare a lobby
/// of four billion. Nothing broke on the server — the count is only compared against — but the
/// listing advertised it, and nothing bounds a mesh of WebRTC peers except this. Sixty-four is
/// well above anything a full-mesh session can carry.
pub const MAX_PLAYERS: u32 = 64;

/// The size a lobby is actually created with, given what the client asked for.
///
/// `0` means "as many as the server allows", which is also what anything above the cap gets.
pub fn clamp_max_players(requested: u32) -> u32 {
    match requested {
        0 => MAX_PLAYERS,
        n => n.min(MAX_PLAYERS),
    }
}

fn generate_lobby_code(state: &ServerState) -> String {
    let mut rng = rand::rng();
    loop {
        let code: String = (0..4)
            .map(|_| rng.random_range(b'A'..=b'Z') as char)
            .collect();
        if !state.lobby_codes.contains_key(&code) {
            return code;
        }
    }
}

fn refusal(reason: &str) -> ServerMessage {
    ServerMessage::LobbyError {
        reason: reason.into(),
    }
}

fn migratable(state: &ServerState, lobby_id: u64) -> ServerMessage {
    ServerMessage::LobbyMigratable {
        lobby_id,
        idle_timeout_secs: state
            .limits
            .idle_timeout
            .as_secs()
            .try_into()
            .unwrap_or(u32::MAX),
    }
}

/// Create a lobby hosted by `host_uuid`.
///
/// `Ok` once the host has been sent everything it is owed; `Err` is the refusal to send back.
pub fn create_lobby(
    state: &ServerState,
    host_uuid: u128,
    max_players: u32,
) -> Result<(), ServerMessage> {
    let (host_name, host_migration) = {
        let conn = state
            .connections
            .get(&host_uuid)
            .ok_or_else(|| refusal("Not authenticated"))?;
        if conn.lobby_id.is_some() {
            return Err(refusal("Already in a lobby"));
        }
        (
            conn.display_name.clone(),
            conn.declares(CAPABILITY_HOST_MIGRATION),
        )
    };

    let max_players = clamp_max_players(max_players);
    let code = generate_lobby_code(state);

    // Picked and inserted under the same entry so two creations that draw the same id -- which a
    // random u64 makes as good as impossible, but not impossible -- retry instead of one silently
    // replacing the other.
    loop {
        let lobby_id = state.next_lobby_id();
        let lobby = match state.lobbies.entry(lobby_id) {
            Entry::Vacant(slot) => slot.insert(LobbyState {
                lobby_id,
                code: code.clone(),
                host_uuid,
                host_name,
                members: vec![host_uuid],
                max_players,
                host_migration,
            }),
            Entry::Occupied(_) => continue,
        };
        state.lobby_codes.insert(code.clone(), lobby_id);
        if let Some(mut conn) = state.connections.get_mut(&host_uuid) {
            conn.lobby_id = Some(lobby_id);
        }
        state.send_to(
            host_uuid,
            ServerMessage::LobbyCreated {
                lobby_id,
                code: code.clone(),
            },
        );
        if lobby.host_migration {
            state.send_to(host_uuid, migratable(state, lobby_id));
        }
        return Ok(());
    }
}

/// Add `player_uuid` to the lobby `lobby_id`.
///
/// `Ok` once the joiner and the members have been sent everything they are owed; `Err` is the
/// refusal to send back.
pub fn join_lobby(
    state: &ServerState,
    player_uuid: u128,
    lobby_id: u64,
) -> Result<(), ServerMessage> {
    let joiner_migrates = {
        let conn = state
            .connections
            .get(&player_uuid)
            .ok_or_else(|| refusal("Not authenticated"))?;
        if conn.lobby_id.is_some() {
            return Err(refusal("Already in a lobby"));
        }
        conn.declares(CAPABILITY_HOST_MIGRATION)
    };

    let mut lobby = state
        .lobbies
        .get_mut(&lobby_id)
        .ok_or_else(|| refusal("Lobby not found"))?;

    if lobby.members.len() as u32 >= lobby.max_players {
        return Err(refusal("Lobby is full"));
    }

    let existing_members = lobby.members.clone();
    lobby.members.push(player_uuid);
    if let Some(mut conn) = state.connections.get_mut(&player_uuid) {
        conn.lobby_id = Some(lobby_id);
    }

    state.send_to_all_except(
        &lobby.members,
        player_uuid,
        ServerMessage::PlayerJoined { player_uuid },
    );
    state.send_to(
        player_uuid,
        ServerMessage::LobbyJoined {
            lobby_id,
            host_uuid: lobby.host_uuid,
            existing_members,
        },
    );
    // A joiner that did not declare the capability could not decode this, and has nothing to do
    // with it: should the host leave, it is disconnected, as it always was.
    if lobby.host_migration && joiner_migrates {
        state.send_to(player_uuid, migratable(state, lobby_id));
    }
    Ok(())
}

pub fn join_lobby_by_code(
    state: &ServerState,
    player_uuid: u128,
    code: &str,
) -> Result<(), ServerMessage> {
    let code = code.to_uppercase();
    let Some(lobby_id) = state.lobby_codes.get(&code).map(|r| *r) else {
        return Err(refusal("Lobby not found"));
    };
    join_lobby(state, player_uuid, lobby_id)
}

/// `player_uuid` leaves its lobby: on request, or because its connection ended.
///
/// A member leaving is announced to the rest. A host leaving a migratable lobby hands it to the
/// member who joined earliest among those that can be told so; otherwise, or when there is no
/// such member, the lobby ends for everyone in it.
pub fn leave_lobby(state: &ServerState, player_uuid: u128) {
    depart(state, player_uuid, Departure::Leave);
}

/// The host of `player_uuid`'s lobby ends it for everyone, migratable or not. Ignored from anyone
/// but the host.
pub fn close_lobby(state: &ServerState, player_uuid: u128) {
    depart(state, player_uuid, Departure::Close);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Departure {
    Leave,
    Close,
}

fn depart(state: &ServerState, player_uuid: u128, departure: Departure) {
    let Some(lobby_id) = state
        .connections
        .get(&player_uuid)
        .and_then(|conn| conn.lobby_id)
    else {
        return;
    };

    let Some(mut lobby) = state.lobbies.get_mut(&lobby_id) else {
        // A lobby that is already gone is one this connection has left; saying so keeps it from
        // being refused "Already in a lobby" for good.
        if let Some(mut conn) = state.connections.get_mut(&player_uuid) {
            conn.lobby_id = None;
        }
        return;
    };
    let is_host = lobby.host_uuid == player_uuid;
    if departure == Departure::Close && !is_host {
        tracing::warn!("Ignoring CloseLobby from {player_uuid}: it does not host lobby {lobby_id}");
        return;
    }
    if let Some(mut conn) = state.connections.get_mut(&player_uuid) {
        conn.lobby_id = None;
    }

    // Collect the peers the leaving player was connected to,
    // so we can send PeerLeft events to them AND to the leaving player.
    let peers_of_leaving: Vec<u128> = lobby
        .members
        .iter()
        .copied()
        .filter(|&uuid| uuid != player_uuid)
        .collect();
    lobby.members.retain(|&uuid| uuid != player_uuid);

    if !is_host {
        state.send_to_all_except(
            &lobby.members,
            player_uuid,
            ServerMessage::PlayerLeft { player_uuid },
        );
    } else {
        let successor = match departure {
            Departure::Leave if lobby.host_migration => lobby
                .members
                .iter()
                .copied()
                .find(|&uuid| state.declares(uuid, CAPABILITY_HOST_MIGRATION)),
            _ => None,
        };
        match successor {
            Some(new_host) => hand_over(state, &mut lobby, player_uuid, new_host),
            None => {
                let remaining = std::mem::take(&mut lobby.members);
                let code = lobby.code.clone();
                // Removing an entry while holding it would deadlock the map; nobody can join a
                // lobby with no host from here on anyway, since its code goes with it.
                drop(lobby);
                state.lobbies.remove(&lobby_id);
                state.lobby_codes.remove(&code);
                let reason = match departure {
                    Departure::Leave => "Host left the lobby",
                    Departure::Close => "Host closed the lobby",
                };
                disconnect(state, &remaining, reason);
            }
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

/// `new_host` takes over the lobby `previous_host` left. The lobby is held by the caller.
///
/// A member that never declared the capability cannot be told who the host became, and could not
/// follow if it were: it is removed with the `Disconnected` it has always had, and is never the
/// successor.
fn hand_over(state: &ServerState, lobby: &mut LobbyState, previous_host: u128, new_host: u128) {
    let (followers, stranded): (Vec<u128>, Vec<u128>) = lobby
        .members
        .iter()
        .copied()
        .partition(|&uuid| state.declares(uuid, CAPABILITY_HOST_MIGRATION));
    disconnect(state, &stranded, "Host left the lobby");

    lobby.members = followers;
    lobby.host_uuid = new_host;
    if let Some(conn) = state.connections.get(&new_host) {
        lobby.host_name = conn.display_name.clone();
    }
    tracing::info!(
        "Lobby {}: host {previous_host} left, {new_host} hosts it now",
        lobby.lobby_id
    );

    let change = ServerMessage::HostChanged {
        lobby_id: lobby.lobby_id,
        previous_host,
        new_host,
        code: lobby.code.clone(),
        members: lobby.members.clone(),
    };
    for &uuid in &lobby.members {
        state.send_to(uuid, change.clone());
    }
}

/// Remove `members` from whatever lobby they were in, telling each why.
fn disconnect(state: &ServerState, members: &[u128], reason: &str) {
    for &uuid in members {
        state.send_to(
            uuid,
            ServerMessage::Disconnected {
                reason: reason.into(),
            },
        );
        if let Some(mut conn) = state.connections.get_mut(&uuid) {
            conn.lobby_id = None;
        }
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
                code: lobby.code.clone(),
                host_name: lobby.host_name.clone(),
                player_count: lobby.members.len() as u32,
                max_players: lobby.max_players,
            }
        })
        .collect();

    ServerMessage::LobbyList { lobbies }
}
