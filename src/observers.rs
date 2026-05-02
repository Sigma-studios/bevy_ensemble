use bevy::prelude::*;

use crate::{
    Host, Lobby, LobbyClient,
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
