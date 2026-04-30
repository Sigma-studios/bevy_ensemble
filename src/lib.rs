use bevy::prelude::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub type PlayerUUID = u128;
pub const LOCAL_PLAYER_UUID: PlayerUUID = 0;
use std::{
    any::{TypeId, type_name},
    collections::HashMap,
};

pub struct EnsemblePlugin;

impl Plugin for EnsemblePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EnsembleMessageRegistry>()
            .add_message::<StartHosting>()
            .register_ensemble_message_type::<SyncLobbyParticipant>()
            .register_ensemble_message_type::<RemoveLobbyParticipant>()
            .add_systems(
                Update,
                (
                    spawn_host_lobby,
                    add_host_lobby_participant,
                    sync_host_lobby_participant_identity,
                    add_remote_lobby_participants,
                    sync_existing_participants_to_new_lobby_clients
                        .after(add_remote_lobby_participants),
                    broadcast_changed_lobby_participants
                        .after(add_remote_lobby_participants),
                    apply_received_lobby_participants,
                    apply_removed_lobby_participants,
                ),
            );
    }
}

#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalMultiplayerPlayerId(pub PlayerUUID);

#[derive(Component)]
pub struct Lobby;

#[derive(Component)]
pub struct PendingLobby;

#[derive(Component)]
pub struct RequestLobby;

#[derive(Component)]
pub struct Host;

#[derive(Component)]
pub struct LobbyClient;

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyParticipant {
    pub player_uuid: PlayerUUID,
    pub is_host: bool,
}

#[derive(Component)]
#[relationship(relationship_target = LobbyParticipants)]
pub struct LobbyParticipantOf(pub Entity);

#[derive(Component, Default)]
#[relationship_target(relationship = LobbyParticipantOf, linked_spawn)]
pub struct LobbyParticipants(Vec<Entity>);

#[derive(Component)]
#[relationship(relationship_target = PlayerOwnedEntities)]
pub struct PlayerOwned(pub Entity);

#[derive(Component, Default)]
#[relationship_target(relationship = PlayerOwned, linked_spawn)]
pub struct PlayerOwnedEntities(Vec<Entity>);

#[derive(Message)]
pub struct StartHosting;

#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncLobbyParticipant {
    pub player_uuid: PlayerUUID,
    pub is_host: bool,
}

#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveLobbyParticipant {
    pub player_uuid: PlayerUUID,
}

pub trait EnsembleMessage:
    Message + Serialize + DeserializeOwned + Clone + Send + Sync + 'static
{
}

impl<T> EnsembleMessage for T where
    T: Message + Serialize + DeserializeOwned + Clone + Send + Sync + 'static
{
}

pub trait EnsembleAppExt {
    fn register_ensemble_message_type<T: EnsembleMessage>(&mut self) -> &mut Self;
}

impl EnsembleAppExt for App {
    fn register_ensemble_message_type<T: EnsembleMessage>(&mut self) -> &mut Self {
        self.init_resource::<EnsembleMessageRegistry>()
            .add_message::<ReceivedEnsembleMessage<T>>()
            .add_observer(encode_lobby_message::<T>)
            .add_observer(encode_lobby_client_message::<T>);

        let mut registry = self
            .world_mut()
            .resource_mut::<EnsembleMessageRegistry>();
        registry.register::<T>();

        self
    }
}

#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ReceivedEnsembleMessage<T: EnsembleMessage> {
    pub sender: Option<PlayerUUID>,
    pub message: T,
}

#[derive(EntityEvent, Debug)]
pub struct LobbyMessage<T: EnsembleMessage> {
    pub entity: Entity,
    pub message: T,
}

#[derive(EntityEvent, Debug)]
pub struct LobbyClientMessage<T: EnsembleMessage> {
    pub entity: Entity,
    pub message: T,
}

#[derive(EntityEvent, Debug)]
pub struct SerializedLobbyPacket {
    pub entity: Entity,
    pub packet: Vec<u8>,
}

const MESSAGE_TYPE_INDEX_BYTES: usize = std::mem::size_of::<u16>();

#[derive(Resource, Default)]
pub struct EnsembleMessageRegistry {
    entries: Vec<RegisteredEnsembleMessage>,
    type_indices: HashMap<TypeId, u16>,
}

struct RegisteredEnsembleMessage {
    type_name: &'static str,
    dispatch: fn(&mut World, Option<PlayerUUID>, &[u8]),
}

impl EnsembleMessageRegistry {
    fn register<T: EnsembleMessage>(&mut self) {
        let type_id = TypeId::of::<T>();
        let type_name = type_name::<T>();

        if self.type_indices.contains_key(&type_id) {
            panic!("Ensemble message type `{type_name}` was registered more than once");
        }

        let next_index = u16::try_from(self.entries.len()).unwrap_or_else(|_| {
            panic!(
                "Too many ensemble message types registered: maximum is {}",
                u16::MAX
            )
        });

        self.entries.push(RegisteredEnsembleMessage {
            type_name,
            dispatch: dispatch_message::<T>,
        });
        self.type_indices.insert(type_id, next_index);
    }

    fn index_of<T: EnsembleMessage>(&self) -> Option<u16> {
        self.type_indices.get(&TypeId::of::<T>()).copied()
    }

    fn entry(&self, index: u16) -> Option<&RegisteredEnsembleMessage> {
        self.entries.get(index as usize)
    }
}

pub fn encode_ensemble_message<T: EnsembleMessage>(
    registry: &EnsembleMessageRegistry,
    message: &T,
) -> Vec<u8> {
    let index = registry.index_of::<T>().unwrap_or_else(|| {
        panic!(
            "Ensemble message type `{}` was sent without being registered",
            type_name::<T>()
        )
    });
    let payload = serde_cbor::to_vec(message).unwrap_or_else(|error| {
        panic!(
            "Failed to serialize ensemble message type `{}`: {error}",
            type_name::<T>()
        )
    });

    let mut packet = Vec::with_capacity(MESSAGE_TYPE_INDEX_BYTES + payload.len());
    packet.extend_from_slice(&index.to_le_bytes());
    packet.extend_from_slice(&payload);
    packet
}

pub fn decode_ensemble_packet(world: &mut World, sender: Option<PlayerUUID>, packet: &[u8]) {
    assert!(
        packet.len() >= MESSAGE_TYPE_INDEX_BYTES,
        "Received ensemble packet that was too short to contain a type index"
    );

    let index = u16::from_le_bytes([packet[0], packet[1]]);
    let dispatch = {
        let registry = world.resource::<EnsembleMessageRegistry>();
        let entry = registry.entry(index).unwrap_or_else(|| {
            panic!("Received ensemble packet for unregistered type index {index}")
        });
        let _type_name = entry.type_name;
        entry.dispatch
    };

    dispatch(world, sender, &packet[MESSAGE_TYPE_INDEX_BYTES..]);
}

fn dispatch_message<T: EnsembleMessage>(
    world: &mut World,
    sender: Option<PlayerUUID>,
    payload: &[u8],
) {
    let message = serde_cbor::from_slice::<T>(payload).unwrap_or_else(|error| {
        panic!(
            "Failed to deserialize ensemble message type `{}`: {error}",
            type_name::<T>()
        )
    });

    world
        .write_message(ReceivedEnsembleMessage::<T> { sender, message })
        .unwrap_or_else(|| {
            panic!(
                "Message buffer for ensemble message type `{}` is missing",
                type_name::<T>()
            )
        });
}

fn encode_lobby_message<T: EnsembleMessage>(
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

        for &participant_entity in &participants.0 {
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

fn encode_lobby_client_message<T: EnsembleMessage>(
    message: On<LobbyClientMessage<T>>,
    registry: Res<EnsembleMessageRegistry>,
    mut commands: Commands,
) {
    let packet = encode_ensemble_message(&registry, &message.message);
    commands
        .entity(message.entity)
        .trigger(move |entity| SerializedLobbyPacket { entity, packet });
}

fn spawn_host_lobby(
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

fn add_host_lobby_participant(
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

fn add_remote_lobby_participants(
    mut commands: Commands,
    added_lobby_clients: Query<
        (
            &crate_steam_bridge::LobbyClientPlayerUuid,
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

fn sync_existing_participants_to_new_lobby_clients(
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

fn sync_host_lobby_participant_identity(
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

fn broadcast_changed_lobby_participants(
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

fn apply_received_lobby_participants(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<SyncLobbyParticipant>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    existing_participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let Some(client_lobby) = client_lobby else {
        return;
    };

    for message in messages.read() {
        if let Some((participant_entity, _, _)) =
            existing_participants
                .iter()
                .find(|(_, participant, participant_of)| {
                    participant_of.0 == *client_lobby
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
            LobbyParticipantOf(*client_lobby),
        ));
    }
}

fn apply_removed_lobby_participants(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<RemoveLobbyParticipant>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    existing_participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let Some(client_lobby) = client_lobby else {
        return;
    };

    for message in messages.read() {
        if let Some((participant_entity, _, _)) =
            existing_participants
                .iter()
                .find(|(_, participant, participant_of)| {
                    participant_of.0 == *client_lobby
                        && participant.player_uuid == message.message.player_uuid
                })
        {
            commands.entity(participant_entity).try_despawn();
        }
    }
}

pub mod crate_steam_bridge {
    use bevy::prelude::*;
    use crate::PlayerUUID;

    #[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
    pub struct LobbyClientPlayerUuid(pub PlayerUUID);
}
