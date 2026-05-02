use bevy::prelude::*;

use crate::{
    Host, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyParticipant, LobbyParticipantOf,
    LocalMultiplayerPlayerId, PendingLobby, RequestLobby,
    messages::{
        LobbyClientMessage, LobbyMessage, ReceivedEnsembleMessage, RemoveLobbyParticipant,
        StartHosting, SyncLobbyParticipant,
    },
    LOCAL_PLAYER_UUID,
};

/// Spawns a host lobby entity when [`StartHosting`] is received.
///
/// Creates an entity with [`PendingLobby`], [`RequestLobby`], and [`Host`] components.
/// Also inserts the [`LocalMultiplayerPlayerId`] resource with [`LOCAL_PLAYER_UUID`]
/// as a placeholder until the platform backend assigns the real identity.
///
/// Ignored if a host lobby already exists.
pub(crate) fn spawn_host_lobby(
    mut commands: Commands,
    mut start_hosting: MessageReader<StartHosting>,
    existing_hosts: Query<(), (With<Host>, Or<(With<Lobby>, With<PendingLobby>)>)>,
) {
    let mut should_spawn = false;
    for _ in start_hosting.read() {
        should_spawn = true;
    }

    if !should_spawn || !existing_hosts.is_empty() {
        return;
    }

    commands.spawn((PendingLobby, RequestLobby, Host));
    commands.insert_resource(LocalMultiplayerPlayerId(LOCAL_PLAYER_UUID));
}

/// Adds the host player as a [`LobbyParticipant`] when the lobby becomes active.
///
/// Runs when [`Lobby`] is added to a host entity. Creates a participant entity
/// with `is_host: true` linked via [`LobbyParticipantOf`].
pub(crate) fn add_host_lobby_participant(
    mut commands: Commands,
    local_player_id: Option<Res<LocalMultiplayerPlayerId>>,
    ready_host_lobbies: Query<Entity, (With<Lobby>, With<Host>, Added<Lobby>)>,
    existing_participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
) {
    let Some(local_player_id) = local_player_id else {
        return;
    };

    for lobby in ready_host_lobbies.iter() {
        if existing_participants
            .iter()
            .any(|(participant, participant_of)| {
                participant_of.0 == lobby && participant.player_uuid == local_player_id.0
            })
        {
            continue;
        }

        commands.spawn((
            LobbyParticipant {
                player_uuid: local_player_id.0,
                is_host: true,
            },
            LobbyParticipantOf(lobby),
        ));
    }
}

/// Creates [`LobbyParticipant`] entities for newly connected remote clients.
///
/// When a [`LobbyClient`] component is added to an entity that also has
/// [`LobbyClientPlayerUuid`], this system spawns a corresponding participant.
pub(crate) fn add_remote_lobby_participants(
    mut commands: Commands,
    added_lobby_clients: Query<
        (
            &LobbyClientPlayerUuid,
            &LobbyParticipantOf,
        ),
        (With<LobbyClient>, Added<LobbyClient>),
    >,
    existing_participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
) {
    for (player_uuid, participant_of) in added_lobby_clients.iter() {
        if existing_participants
            .iter()
            .any(|(participant, existing_participant_of)| {
                existing_participant_of.0 == participant_of.0
                    && participant.player_uuid == player_uuid.0
            })
        {
            continue;
        }

        commands.spawn((
            LobbyParticipant {
                player_uuid: player_uuid.0,
                is_host: false,
            },
            LobbyParticipantOf(participant_of.0),
        ));
    }
}

/// Sends existing participant data to newly connected lobby clients.
///
/// When a new [`LobbyClient`] joins, this system sends a
/// [`SyncLobbyParticipant`] message for every existing participant so the
/// new client can build its local roster.
pub(crate) fn sync_existing_participants_to_new_lobby_clients(
    mut commands: Commands,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    added_lobby_clients: Query<
        (Entity, &LobbyParticipantOf),
        (With<LobbyClient>, Added<LobbyClient>),
    >,
) {
    for (client_entity, participant_of) in added_lobby_clients.iter() {
        for (participant, existing_participant_of) in participants.iter() {
            if existing_participant_of.0 != participant_of.0 {
                continue;
            }

            let message = SyncLobbyParticipant {
                player_uuid: participant.player_uuid,
                is_host: participant.is_host,
            };
            commands
                .entity(client_entity)
                .trigger(move |entity| LobbyClientMessage::<SyncLobbyParticipant> {
                    entity,
                    message,
                });
        }
    }
}

/// Keeps the host's participant identity in sync with [`LocalMultiplayerPlayerId`].
///
/// If the platform backend updates the local player's UUID after the host participant
/// was already created, this system patches the participant to match.
pub(crate) fn sync_host_lobby_participant_identity(
    mut commands: Commands,
    local_player_id: Option<Res<LocalMultiplayerPlayerId>>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let Some(local_player_id) = local_player_id else {
        return;
    };

    for host_lobby in host_lobbies.iter() {
        for (participant_entity, participant, participant_of) in participants.iter() {
            if participant_of.0 != host_lobby || !participant.is_host {
                continue;
            }

            if participant.player_uuid == local_player_id.0 {
                continue;
            }

            commands.entity(participant_entity).insert(LobbyParticipant {
                player_uuid: local_player_id.0,
                is_host: true,
            });
        }
    }
}

/// Broadcasts participant changes from the host to all clients.
///
/// When a [`LobbyParticipant`] is added or changed on a host lobby, this system
/// triggers a [`LobbyMessage<SyncLobbyParticipant>`] to propagate the change.
pub(crate) fn broadcast_changed_lobby_participants(
    mut commands: Commands,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    changed_participants: Query<
        (&LobbyParticipant, &LobbyParticipantOf),
        Or<(Added<LobbyParticipant>, Changed<LobbyParticipant>)>,
    >,
    lobby_clients: Query<(), With<LobbyClient>>,
) {
    if host_lobbies.is_empty() {
        return;
    }

    for (participant, participant_of) in changed_participants.iter() {
        if lobby_clients.get(participant_of.0).is_ok() {
            continue;
        }

        let message = SyncLobbyParticipant {
            player_uuid: participant.player_uuid,
            is_host: participant.is_host,
        };
        commands
            .entity(participant_of.0)
            .trigger(move |entity| LobbyMessage::<SyncLobbyParticipant> { entity, message });
    }
}

/// Applies received participant sync messages on client lobbies.
///
/// When a client receives a [`ReceivedEnsembleMessage<SyncLobbyParticipant>`],
/// this system either updates an existing participant or spawns a new one.
pub(crate) fn apply_received_lobby_participants(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<SyncLobbyParticipant>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    pending_client_lobby: Option<Single<Entity, (With<PendingLobby>, Without<Host>)>>,
    existing_participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let Some(client_lobby) = client_lobby
        .map(|s| *s)
        .or_else(|| pending_client_lobby.map(|s| *s))
    else {
        return;
    };

    for message in messages.read() {
        if let Some((participant_entity, _, _)) =
            existing_participants
                .iter()
                .find(|(_, participant, participant_of)| {
                    participant_of.0 == client_lobby
                        && participant.player_uuid == message.message.player_uuid
                })
        {
            commands
                .entity(participant_entity)
                .insert(LobbyParticipant {
                    player_uuid: message.message.player_uuid,
                    is_host: message.message.is_host,
                });
            continue;
        }

        commands.spawn((
            LobbyParticipant {
                player_uuid: message.message.player_uuid,
                is_host: message.message.is_host,
            },
            LobbyParticipantOf(client_lobby),
        ));
    }
}

/// Removes participant entities when a removal message is received.
///
/// When a client receives a [`ReceivedEnsembleMessage<RemoveLobbyParticipant>`],
/// this system despawns the matching participant entity.
pub(crate) fn apply_removed_lobby_participants(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<RemoveLobbyParticipant>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    pending_client_lobby: Option<Single<Entity, (With<PendingLobby>, Without<Host>)>>,
    existing_participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let Some(client_lobby) = client_lobby
        .map(|s| *s)
        .or_else(|| pending_client_lobby.map(|s| *s))
    else {
        return;
    };

    for message in messages.read() {
        if let Some((participant_entity, _, _)) =
            existing_participants
                .iter()
                .find(|(_, participant, participant_of)| {
                    participant_of.0 == client_lobby
                        && participant.player_uuid == message.message.player_uuid
                })
        {
            commands.entity(participant_entity).try_despawn();
        }
    }
}
