use std::marker::PhantomData;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    Host, Lobby, LobbyClient, LobbyParticipant, LobbyParticipantOf, LocalMultiplayerPlayerId,
    PendingLobby, PlayerUUID, SendMode,
    messages::{EnsembleAppExt, EnsembleMessage, LobbyClientMessage, LobbyMessage, ReceivedEnsembleMessage},
};

/// Per-player data synchronized across the lobby.
///
/// Attach this to [`LobbyParticipant`] entities. Changes are automatically
/// broadcast from the host to all clients.
///
/// Use [`SetPlayerData`] to update the local player's data from either
/// the host or a client.
///
/// # Example
///
/// ```rust,ignore
/// #[derive(Message, Clone, Debug, serde::Serialize, serde::Deserialize)]
/// struct PlayerProfile {
///     color: [f32; 3],
/// }
///
/// app.add_plugins(PlayerDataPlugin::<PlayerProfile>::default());
///
/// fn show_profiles(
///     participants: Query<(&LobbyParticipant, Option<&PlayerData<PlayerProfile>>)>,
/// ) {
///     for (participant, profile) in participants.iter() {
///         if let Some(profile) = profile {
///             println!("{}: {:?}", participant.player_uuid, profile.0);
///         }
///     }
/// }
/// ```
#[derive(Component, Clone, Debug)]
pub struct PlayerData<T: EnsembleMessage>(pub T);

/// Entity event to set the local player's data.
///
/// Trigger this on a lobby entity to update your own [`PlayerData<T>`].
/// - On the **host**: directly inserts on the host's participant entity,
///   and change detection broadcasts to all clients.
/// - On a **client**: sends a request to the host, which applies and rebroadcasts.
///
/// # Example
///
/// ```rust,ignore
/// fn set_color(mut commands: Commands, lobby: Single<Entity, With<Lobby>>) {
///     commands.entity(*lobby).trigger(|entity| SetPlayerData::new(
///         entity,
///         PlayerProfile { color: [1.0, 0.0, 0.0] },
///     ));
/// }
/// ```
#[derive(EntityEvent, Debug)]
pub struct SetPlayerData<T: EnsembleMessage> {
    pub entity: Entity,
    pub data: T,
}

impl<T: EnsembleMessage> SetPlayerData<T> {
    pub fn new(entity: Entity, data: T) -> Self {
        Self { entity, data }
    }
}

/// Internal message for synchronizing player data across the network.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SyncPlayerData<T> {
    pub player_uuid: PlayerUUID,
    pub data: T,
}

/// Buffer for player data messages that arrived before their participant entity existed.
#[derive(Resource)]
struct PendingPlayerData<T: EnsembleMessage> {
    pending: Vec<SyncPlayerData<T>>,
}

impl<T: EnsembleMessage> Default for PendingPlayerData<T> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
        }
    }
}

/// Plugin that enables per-player data synchronization for a specific type.
///
/// Each data type you want to synchronize needs its own plugin instance.
/// The type must implement [`EnsembleMessage`] (i.e. `Message + Serialize +
/// Deserialize + Clone + Send + Sync + 'static`).
///
/// # Example
///
/// ```rust,ignore
/// app.add_plugins(PlayerDataPlugin::<PlayerProfile>::default());
/// ```
pub struct PlayerDataPlugin<T: EnsembleMessage>(PhantomData<T>);

impl<T: EnsembleMessage> Default for PlayerDataPlugin<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<T: EnsembleMessage> Plugin for PlayerDataPlugin<T> {
    fn build(&self, app: &mut App) {
        app.init_resource::<PendingPlayerData<T>>()
            .register_ensemble_message_type::<SyncPlayerData<T>>()
            .add_observer(handle_set_player_data::<T>)
            .add_systems(
                Update,
                (
                    broadcast_changed_player_data::<T>,
                    sync_existing_player_data_to_new_clients::<T>
                        .after(broadcast_changed_player_data::<T>),
                    apply_received_player_data::<T>,
                ),
            );
    }
}

/// Observer for [`SetPlayerData<T>`].
///
/// On the host, directly inserts the data on the host's participant entity.
/// On a client, sends the data to the host via [`LobbyMessage`].
fn handle_set_player_data<T: EnsembleMessage>(
    event: On<SetPlayerData<T>>,
    mut commands: Commands,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    mut pending: ResMut<PendingPlayerData<T>>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let Some(local_player) = local_player else {
        return;
    };

    let lobby_entity = event.entity;
    let data = event.data.clone();

    if host_lobbies.get(lobby_entity).is_ok() {
        // Host: directly insert on own participant
        if let Some((participant_entity, _, _)) = participants
            .iter()
            .find(|(_, p, pof)| pof.0 == lobby_entity && p.player_uuid == local_player.0)
        {
            commands.entity(participant_entity).insert(PlayerData(data));
        } else {
            // Participant not created yet — buffer for retry
            pending.pending.push(SyncPlayerData {
                player_uuid: local_player.0,
                data,
            });
        }
    } else {
        // Client: send request to host
        let player_uuid = local_player.0;
        commands
            .entity(lobby_entity)
            .trigger(move |entity| LobbyMessage {
                entity,
                message: SyncPlayerData { player_uuid, data },
                send_mode: SendMode::Reliable,
            });
    }
}

/// Host: broadcasts changed [`PlayerData<T>`] to all clients.
fn broadcast_changed_player_data<T: EnsembleMessage>(
    mut commands: Commands,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    changed_data: Query<
        (&LobbyParticipant, &LobbyParticipantOf, &PlayerData<T>),
        Or<(Added<PlayerData<T>>, Changed<PlayerData<T>>)>,
    >,
) {
    for (participant, participant_of, player_data) in changed_data.iter() {
        if host_lobbies.get(participant_of.0).is_err() {
            continue;
        }

        let message = SyncPlayerData {
            player_uuid: participant.player_uuid,
            data: player_data.0.clone(),
        };
        commands
            .entity(participant_of.0)
            .trigger(move |entity| LobbyMessage {
                entity,
                message,
                send_mode: SendMode::Reliable,
            });
    }
}

/// Host: sends existing [`PlayerData<T>`] to newly connected clients.
fn sync_existing_player_data_to_new_clients<T: EnsembleMessage>(
    mut commands: Commands,
    participants_with_data: Query<(&LobbyParticipant, &LobbyParticipantOf, &PlayerData<T>)>,
    added_lobby_clients: Query<
        (Entity, &LobbyParticipantOf),
        (With<LobbyClient>, Added<LobbyClient>),
    >,
) {
    for (client_entity, client_participant_of) in added_lobby_clients.iter() {
        for (participant, participant_of, player_data) in participants_with_data.iter() {
            if participant_of.0 != client_participant_of.0 {
                continue;
            }

            let message = SyncPlayerData {
                player_uuid: participant.player_uuid,
                data: player_data.0.clone(),
            };
            commands
                .entity(client_entity)
                .trigger(move |entity| LobbyClientMessage {
                    entity,
                    message,
                    send_mode: SendMode::Reliable,
                });
        }
    }
}

/// Receives [`SyncPlayerData`] messages and applies them.
///
/// - On the **host**: treats incoming messages as client requests — applies
///   the data to the participant entity, which triggers broadcast via change detection.
/// - On a **client**: applies data from host broadcasts to the local participant entity.
///
/// Messages that arrive before their target participant entity exists are buffered
/// and retried on subsequent frames.
fn apply_received_player_data<T: EnsembleMessage>(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<SyncPlayerData<T>>>,
    mut pending: ResMut<PendingPlayerData<T>>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    pending_client_lobby: Option<Single<Entity, (With<PendingLobby>, Without<Host>)>>,
    participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let buffered = std::mem::take(&mut pending.pending);
    let all_messages = buffered
        .into_iter()
        .chain(messages.read().map(|m| m.message.clone()));

    for sync_msg in all_messages {
        let player_uuid = sync_msg.player_uuid;

        // Host: a client is requesting to set their data
        if let Some(host_lobby) = host_lobbies.iter().next() {
            if let Some((participant_entity, _, _)) = participants
                .iter()
                .find(|(_, p, pof)| pof.0 == host_lobby && p.player_uuid == player_uuid)
            {
                commands
                    .entity(participant_entity)
                    .insert(PlayerData(sync_msg.data));
            } else {
                pending.pending.push(sync_msg);
            }
            continue;
        }

        // Client: apply synced data
        let Some(lobby) = client_lobby
            .as_ref()
            .map(|s| **s)
            .or_else(|| pending_client_lobby.as_ref().map(|s| **s))
        else {
            pending.pending.push(sync_msg);
            continue;
        };

        if let Some((participant_entity, _, _)) = participants
            .iter()
            .find(|(_, p, pof)| pof.0 == lobby && p.player_uuid == player_uuid)
        {
            commands
                .entity(participant_entity)
                .insert(PlayerData(sync_msg.data));
        } else {
            pending.pending.push(sync_msg);
        }
    }
}
