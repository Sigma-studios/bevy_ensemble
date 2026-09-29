use std::marker::PhantomData;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    AwaitingHost, Host, Lobby, LobbyClient, LobbyParticipant, LobbyParticipantOf,
    LocalMultiplayerPlayerId, PendingLobby, PlayerUUID, SendMode,
    messages::MessageAuthority,
    messages::{
        EnsembleAppExt, EnsembleMessage, LobbyClientMessage, LobbyMessage, ReceivedEnsembleMessage,
    },
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
#[doc(hidden)]
pub struct SyncPlayerData<T> {
    pub player_uuid: PlayerUUID,
    pub data: T,
}

/// Player data that arrived before its participant entity existed, waiting for it: `(sender,
/// data, when it was first buffered)`.
///
/// On the lobby entity it is waiting in, so it goes with that lobby. As a resource it outlived
/// every session, and an entry for a player who never appeared — one who left in the same frame
/// they were announced — was put back every frame for as long as the process ran, and read into
/// whichever session came next. Entries are also dropped after [`PENDING_PLAYER_DATA_SECS`]: a
/// participant appears within a frame or two of its data, so anything older is for somebody who
/// is not coming.
#[derive(Component)]
struct PendingPlayerData<T: EnsembleMessage> {
    pending: Vec<(Option<PlayerUUID>, SyncPlayerData<T>, f64)>,
}

impl<T: EnsembleMessage> Default for PendingPlayerData<T> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
        }
    }
}

/// How long player data waits for its participant before it is given up on.
const PENDING_PLAYER_DATA_SECS: f64 = 10.0;

/// On a lobby entity: the last `T` this peer asked to be its own there.
///
/// Kept so that it can be said again after a change of host. A client switching to a named new
/// host sends nothing until it has reached it (see [`AwaitingHost`](crate::AwaitingHost)), so an
/// edit made in that window was dropped; and one made just before the old host went may never
/// have been broadcast. Either way the new host holds the old value, and nothing would ever send
/// the new one.
#[derive(Component)]
struct RequestedPlayerData<T: EnsembleMessage>(T);

/// Add `entries` to the lobby's buffer, creating it if this is the first thing to wait there.
fn buffer_pending<T: EnsembleMessage>(
    commands: &mut Commands,
    lobby: Entity,
    buffer: Option<Mut<PendingPlayerData<T>>>,
    entries: Vec<(Option<PlayerUUID>, SyncPlayerData<T>, f64)>,
) {
    match buffer {
        Some(mut buffer) => buffer.pending.extend(entries),
        None if !entries.is_empty() => {
            commands
                .entity(lobby)
                .try_insert(PendingPlayerData { pending: entries });
        }
        None => {}
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

impl<T: EnsembleMessage + Default> PlayerDataPlugin<T> {
    /// Keep the local player's own copy across sessions, and publish it whenever a lobby appears.
    ///
    /// `key` is the storage key, shared with everything else on the origin, so prefix it with
    /// something that belongs to the game (`"mygame.profile"`).
    ///
    /// The `Default` bound is why this is a separate constructor rather than a field: a game that
    /// only wants the synchronised half should not have to invent a default for its data.
    pub fn persisted(self, key: &'static str) -> PersistedPlayerDataPlugin<T> {
        PersistedPlayerDataPlugin {
            key,
            marker: PhantomData,
        }
    }
}

/// The local player's own `T`, whether or not there is a lobby to put it in.
///
/// [`PlayerData<T>`] is the synchronised half: a component on a participant entity, owned by the
/// host, and it exists only *inside* a lobby. That is no use to a menu, which has to show you your
/// own name and colours **before** you host or join anything — at that moment there is no lobby,
/// no participant and no component. So the local copy lives here, in a resource, and the lobby is
/// something it is published *into* rather than the place it is kept.
///
/// Edit it directly. [`PersistedPlayerDataPlugin`] writes it out when it changes and publishes it
/// with [`SetPlayerData`] whenever a lobby turns up, so a game never writes that wiring again.
#[derive(Resource, Clone, Debug, Default, Serialize, Deserialize)]
pub struct LocalPlayerData<T>(pub T);

/// [`PlayerDataPlugin`] plus the local copy: kept across sessions, published on every lobby.
///
/// Built with [`PlayerDataPlugin::persisted`].
pub struct PersistedPlayerDataPlugin<T: EnsembleMessage> {
    key: &'static str,
    marker: PhantomData<T>,
}

impl<T: EnsembleMessage + Default> Plugin for PersistedPlayerDataPlugin<T> {
    fn build(&self, app: &mut App) {
        app.add_plugins(PlayerDataPlugin::<T>::default())
            .add_systems(Update, publish_local_player_data::<T>);

        #[cfg(feature = "persistence")]
        app.add_plugins(crate::persistence::PersistedResourcePlugin::<
            LocalPlayerData<T>,
        >::new(self.key));
        #[cfg(not(feature = "persistence"))]
        {
            let _ = self.key;
            app.init_resource::<LocalPlayerData<T>>();
        }
    }
}

/// Put the local copy into whatever lobby this peer is in, and into the next one too.
///
/// Two things make it fire: the data changing, and the *lobby* changing. The second is the one
/// worth spelling out — a value published into a lobby that has since gone is not published into
/// the next one, so without it a player who left and joined again arrived wearing a default. It
/// is deduplicated on which lobby was last written to rather than on the value, because
/// [`EnsembleMessage`] does not require `PartialEq` and a per-frame clone-and-compare would be a
/// worse trade than one entity comparison.
fn publish_local_player_data<T: EnsembleMessage>(
    mut commands: Commands,
    data: Res<LocalPlayerData<T>>,
    lobbies: Query<Entity, With<Lobby>>,
    mut published_into: Local<Option<Entity>>,
) {
    let Ok(lobby) = lobbies.single() else {
        // No lobby to be in. The next one is a new one, whatever it turns out to be.
        *published_into = None;
        return;
    };
    if *published_into == Some(lobby) && !data.is_changed() {
        return;
    }
    *published_into = Some(lobby);
    let value = data.0.clone();
    commands
        .entity(lobby)
        .trigger(move |entity| SetPlayerData::new(entity, value));
}

impl<T: EnsembleMessage> Plugin for PlayerDataPlugin<T> {
    fn build(&self, app: &mut App) {
        app.register_control_message_type::<SyncPlayerData<T>>(
            // One name per data type, or two `PlayerDataPlugin`s would collide.
            std::any::type_name::<SyncPlayerData<T>>(),
            MessageAuthority::HostOnly,
        )
        .add_observer(handle_set_player_data::<T>)
        .add_systems(
            Update,
            (
                broadcast_changed_player_data::<T>,
                sync_existing_player_data_to_new_clients::<T>
                    .after(broadcast_changed_player_data::<T>),
                // After the roster, so a participant announced in the same frame as its data
                // is there to receive it.
                apply_received_player_data::<T>.after(crate::systems::apply_lobby_state),
                clear_pending_on_host_change::<T>.before(apply_received_player_data::<T>),
                republish_player_data_to_a_new_host::<T>,
            ),
        );
    }
}

/// What was buffered under one host means nothing under the next: a client's requests to the
/// old host would be replayed as the new host's own, and refused as sent by somebody else.
fn clear_pending_on_host_change<T: EnsembleMessage>(
    mut changes: MessageReader<crate::HostChanged>,
    mut pending: Query<&mut PendingPlayerData<T>>,
) {
    for change in changes.read() {
        if let Ok(mut pending) = pending.get_mut(change.lobby) {
            pending.pending.clear();
        }
    }
}

/// Say this peer's own data again once there is a new host to say it to.
///
/// When the host changes to this peer, at once: it now owns its participant and sets the value
/// directly. When it changes to somebody else, once this peer has reached them — the moment
/// [`AwaitingHost`] comes off — because until then a request has nowhere to go.
fn republish_player_data_to_a_new_host<T: EnsembleMessage>(
    mut commands: Commands,
    mut changes: MessageReader<crate::HostChanged>,
    mut reached: RemovedComponents<AwaitingHost>,
    lobbies: Query<(&RequestedPlayerData<T>, Option<&AwaitingHost>), With<Lobby>>,
) {
    let mut lobbies_to_republish: Vec<Entity> = changes
        .read()
        .map(|change| change.lobby)
        .chain(reached.read())
        .collect();
    lobbies_to_republish.sort();
    lobbies_to_republish.dedup();
    for lobby in lobbies_to_republish {
        let Ok((requested, awaiting)) = lobbies.get(lobby) else {
            continue;
        };
        if awaiting.is_some_and(|awaiting| awaiting.successor.is_some()) {
            continue;
        }
        let data = requested.0.clone();
        commands
            .entity(lobby)
            .trigger(move |entity| SetPlayerData::new(entity, data));
    }
}

/// Observer for [`SetPlayerData<T>`].
///
/// On the host, directly inserts the data on the host's participant entity.
/// On a client, sends the data to the host via [`LobbyMessage`].
fn handle_set_player_data<T: EnsembleMessage>(
    event: On<SetPlayerData<T>>,
    mut commands: Commands,
    time: Res<Time>,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    mut pending: Query<&mut PendingPlayerData<T>>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let Some(local_player) = local_player else {
        return;
    };

    let lobby_entity = event.entity;
    let data = event.data.clone();
    // Remembered on the lobby, to be said again to a new host. See `RequestedPlayerData`.
    commands
        .entity(lobby_entity)
        .try_insert(RequestedPlayerData(data.clone()));

    if host_lobbies.get(lobby_entity).is_ok() {
        // Host: directly insert on own participant
        if let Some((participant_entity, _, _)) = participants
            .iter()
            .find(|(_, p, pof)| pof.0 == lobby_entity && p.player_uuid == local_player.0)
        {
            commands
                .entity(participant_entity)
                .try_insert(PlayerData(data));
        } else {
            // Participant not created yet — buffer for retry. The host's own request is
            // filed under its own identity, which is what the sender check will compare.
            let entry = (
                Some(local_player.0),
                SyncPlayerData {
                    player_uuid: local_player.0,
                    data,
                },
                time.elapsed_secs_f64(),
            );
            buffer_pending(
                &mut commands,
                lobby_entity,
                pending.get_mut(lobby_entity).ok(),
                vec![entry],
            );
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
/// Messages that arrive before their target participant entity exists are buffered on the lobby
/// and retried on subsequent frames (see [`PendingPlayerData`]). A message that arrives while
/// this peer is in no lobby is dropped: there is nothing it could be about.
fn apply_received_player_data<T: EnsembleMessage>(
    mut commands: Commands,
    time: Res<Time>,
    mut messages: MessageReader<ReceivedEnsembleMessage<SyncPlayerData<T>>>,
    mut pending: Query<&mut PendingPlayerData<T>>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    client_lobbies: Query<Entity, (Or<(With<Lobby>, With<PendingLobby>)>, Without<Host>)>,
    participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let arrived: Vec<_> = messages
        .read()
        .map(|m| (m.sender, m.message.clone()))
        .collect();
    let host_lobby = host_lobbies.iter().next();
    let Some(lobby) = host_lobby.or_else(|| client_lobbies.iter().next()) else {
        return;
    };

    let now = time.elapsed_secs_f64();
    let buffered = pending
        .get_mut(lobby)
        .map(|mut pending| std::mem::take(&mut pending.pending))
        .unwrap_or_default();
    let all_messages = buffered.into_iter().chain(
        arrived
            .into_iter()
            .map(|(sender, message)| (sender, message, now)),
    );
    let mut still_waiting = Vec::new();

    for (sender, sync_msg, buffered_at) in all_messages {
        let player_uuid = sync_msg.player_uuid;

        // Host: a client is requesting to set their data — its own, and nobody else's. The
        // target used to be whatever the message named, so one message rewrote any player.
        if host_lobby.is_some() && sender != Some(player_uuid) {
            warn!(
                "refused player data for {player_uuid:#x} sent by {sender:#x?}: a client may \
                 only set its own"
            );
            continue;
        }

        if let Some((participant_entity, _, _)) = participants
            .iter()
            .find(|(_, p, pof)| pof.0 == lobby && p.player_uuid == player_uuid)
        {
            // `try_`: a participant this frame's roster removed is still in the query.
            commands
                .entity(participant_entity)
                .try_insert(PlayerData(sync_msg.data));
        } else if now - buffered_at < PENDING_PLAYER_DATA_SECS {
            still_waiting.push((sender, sync_msg, buffered_at));
        } else {
            debug!(
                "dropping player data for {player_uuid:#x}: no such participant appeared in \
                 {PENDING_PLAYER_DATA_SECS}s"
            );
        }
    }

    buffer_pending(
        &mut commands,
        lobby,
        pending.get_mut(lobby).ok(),
        still_waiting,
    );
}
