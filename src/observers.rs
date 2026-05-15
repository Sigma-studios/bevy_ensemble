use bevy::prelude::*;

use crate::{
    Host, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyParticipant, LobbyParticipantOf,
    RemoveLobbyParticipant,
    components::LobbyParticipants,
    messages::{EnsembleMessage, LobbyClientMessage, LobbyMessage, SerializedLobbyPacket},
    registry::{EnsembleMessageRegistry, encode_ensemble_message},
};

/// Routes a [`LobbyMessage`] to the appropriate [`LobbyClientMessage`] targets.
///
/// - On a **host** lobby: iterates all [`LobbyClient`] participants and triggers
///   a [`LobbyClientMessage`] on each.
/// - On a **client** lobby: triggers a [`LobbyClientMessage`] on the lobby entity itself.
pub(crate) fn encode_lobby_message<T: EnsembleMessage>(
    message: On<LobbyMessage<T>>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    participants: Query<&LobbyParticipants>,
    lobby_clients: Query<(), With<LobbyClient>>,
    mut commands: Commands,
) {
    if host_lobbies.get(message.entity).is_ok() {
        let Ok(participants) = participants.get(message.entity) else {
            return;
        };

        for participant_entity in participants.iter().copied() {
            if lobby_clients.get(participant_entity).is_err() {
                continue;
            }

            let outgoing = message.message.clone();
            commands
                .entity(participant_entity)
                .trigger(move |entity| LobbyClientMessage::<T> {
                    entity,
                    message: outgoing,
                });
        }
        return;
    }

    let outgoing = message.message.clone();
    commands
        .entity(message.entity)
        .trigger(move |entity| LobbyClientMessage::<T> {
            entity,
            message: outgoing,
        });
}

/// Handles cleanup when a [`LobbyClient`] entity is removed.
///
/// On a host lobby, this broadcasts a [`RemoveLobbyParticipant`] message to
/// remaining clients and despawns the corresponding [`LobbyParticipant`] entity.
///
/// This enables kicking a player by simply despawning their `LobbyClient` entity —
/// the observer handles notifying other clients and cleaning up the participant roster.
pub(crate) fn on_lobby_client_removed(
    trigger: On<Remove, LobbyClient>,
    query: Query<(&LobbyClientPlayerUuid, &LobbyParticipantOf)>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
    mut commands: Commands,
) {
    let Ok((player_uuid, participant_of)) = query.get(trigger.event_target()) else {
        return;
    };

    let lobby = participant_of.0;
    if host_lobbies.get(lobby).is_err() {
        return;
    }

    let player_uuid = player_uuid.0;

    if let Ok(mut lobby_commands) = commands.get_entity(lobby) {
        lobby_commands.trigger(move |entity| LobbyMessage::<RemoveLobbyParticipant> {
            entity,
            message: RemoveLobbyParticipant { player_uuid },
        });
    }

    if let Some((participant_entity, _, _)) = participants
        .iter()
        .find(|(_, p, pof)| pof.0 == lobby && p.player_uuid == player_uuid)
    {
        commands.entity(participant_entity).try_despawn();
    }
}

/// Serializes a [`LobbyClientMessage`] into a [`SerializedLobbyPacket`].
///
/// Uses the [`EnsembleMessageRegistry`] to encode the message with its type index
/// and CBOR payload, then triggers a [`SerializedLobbyPacket`] on the same entity
/// for the platform backend to transmit.
pub(crate) fn encode_lobby_client_message<T: EnsembleMessage>(
    message: On<LobbyClientMessage<T>>,
    registry: Res<EnsembleMessageRegistry>,
    mut commands: Commands,
) {
    let packet = encode_ensemble_message(&registry, &message.message);
    commands
        .entity(message.entity)
        .trigger(move |entity| SerializedLobbyPacket { entity, packet });
}
