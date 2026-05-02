use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::PlayerUUID;

/// The local player's network identity.
///
/// Inserted as a resource when the local player creates or joins a lobby.
/// Initially set to [`LOCAL_PLAYER_UUID`](crate::LOCAL_PLAYER_UUID) by
/// [`EnsemblePlugin`](crate::EnsemblePlugin), then overwritten by the platform
/// backend with the real player identity.
///
/// # Example
///
/// ```rust,ignore
/// fn my_system(local_id: Res<LocalMultiplayerPlayerId>) {
///     println!("I am player {}", local_id.0);
/// }
/// ```
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalMultiplayerPlayerId(pub PlayerUUID);

/// Marks an entity as an active, fully connected lobby.
///
/// Added by the platform backend once the lobby has been successfully created (for hosts)
/// or fully joined and handshaked (for clients). Before this component is present, the
/// entity will have [`PendingLobby`] instead.
///
/// # Querying
///
/// ```rust,ignore
/// // All active lobbies
/// fn system(lobbies: Query<Entity, With<Lobby>>) { .. }
///
/// // The lobby I'm hosting
/// fn host_system(lobby: Query<Entity, (With<Lobby>, With<Host>)>) { .. }
///
/// // The lobby I've joined as a client
/// fn client_system(lobby: Query<Entity, (With<Lobby>, Without<Host>)>) { .. }
/// ```
#[derive(Component)]
pub struct Lobby;

/// Marks a lobby entity that is waiting for the platform backend to finish setup.
///
/// Present from the moment a lobby creation or join is initiated until the backend
/// promotes it to [`Lobby`]. For hosts this happens when the platform confirms creation;
/// for clients it happens after the handshake with the host completes.
///
/// Automatically removed when [`Lobby`] is added.
#[derive(Component)]
pub struct PendingLobby;

/// Marks a lobby entity that has been requested for creation by the local player.
///
/// Spawned alongside [`PendingLobby`] and [`Host`] when a [`StartHosting`](crate::StartHosting)
/// message is processed. The platform backend observes entities with this component
/// to initiate the actual lobby creation on the network.
///
/// Removed by the platform backend once the lobby is created.
#[derive(Component)]
pub struct RequestLobby;

/// Marks a lobby entity as being hosted by the local player.
///
/// The host is the authoritative source of truth for participant lists and is
/// responsible for relaying messages between clients. When a
/// [`LobbyMessage`](crate::LobbyMessage) is triggered on a host lobby, it is
/// automatically forwarded to all connected [`LobbyClient`] entities.
#[derive(Component)]
pub struct Host;

/// Marks an entity as a remote client connected to a hosted lobby.
///
/// Each `LobbyClient` entity represents a single remote player's connection.
/// These entities are children of the lobby (via [`LobbyParticipantOf`]) and carry
/// platform-specific connection data (e.g. `LobbyClientSteamId` in the Steam backend).
///
/// `LobbyClient` entities also carry a [`LobbyClientPlayerUuid`] component that maps
/// the connection to a [`PlayerUUID`].
#[derive(Component)]
pub struct LobbyClient;

/// Maps a [`LobbyClient`] entity to its [`PlayerUUID`].
///
/// Attached by the platform backend when a remote client connects. This is the bridge
/// between platform-specific connection identifiers and the platform-agnostic player
/// identity used by the rest of the system.
///
/// # For backend implementors
///
/// You must attach this component to every `LobbyClient` (or pending client) entity
/// so that the core systems can create [`LobbyParticipant`] records.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LobbyClientPlayerUuid(pub PlayerUUID);

/// Data about a participant in a lobby.
///
/// Each player in a lobby (including the host) gets a `LobbyParticipant` entity
/// that is related to the lobby via [`LobbyParticipantOf`]. This component holds
/// the player's identity and role.
///
/// Participants are automatically synced across the network: the host broadcasts
/// changes via [`SyncLobbyParticipant`](crate::SyncLobbyParticipant), and clients
/// apply them locally.
///
/// # Querying
///
/// ```rust,ignore
/// // All participants in a specific lobby
/// fn show_roster(
///     lobby: Single<&LobbyParticipants, With<Lobby>>,
///     participants: Query<&LobbyParticipant>,
/// ) {
///     for &entity in lobby.iter() {
///         if let Ok(participant) = participants.get(entity) {
///             println!("Player {} (host: {})", participant.player_uuid, participant.is_host);
///         }
///     }
/// }
/// ```
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyParticipant {
    pub player_uuid: PlayerUUID,
    pub is_host: bool,
}

/// Relationship linking a participant (or client) entity to its owning lobby.
///
/// Used on both [`LobbyParticipant`] entities and [`LobbyClient`] entities to
/// indicate which lobby they belong to. The inverse is [`LobbyParticipants`].
#[derive(Component)]
#[relationship(relationship_target = LobbyParticipants)]
pub struct LobbyParticipantOf(pub Entity);

/// Collects all entities related to a lobby via [`LobbyParticipantOf`].
///
/// Automatically maintained by Bevy's relationship system. Query this on a lobby
/// entity to get all participant and client entities.
#[derive(Component, Default)]
#[relationship_target(relationship = LobbyParticipantOf, linked_spawn)]
pub struct LobbyParticipants(Vec<Entity>);

impl LobbyParticipants {
    /// Returns a slice of all participant entities in this lobby.
    pub fn iter(&self) -> impl Iterator<Item = &Entity> {
        self.0.iter()
    }
}

/// Summary information about a publicly listed lobby.
///
/// Used by backends that support lobby discovery (e.g. WebRTC signaling server).
/// The [`PublicLobbies`] resource contains a list of these.
#[derive(Clone, Debug)]
pub struct PublicLobbyInfo {
    /// Backend-specific lobby identifier (e.g. server-assigned u64 for WebRTC).
    pub lobby_id: u64,
    /// Display name of the lobby host.
    pub host_name: String,
    /// Current number of players in the lobby.
    pub player_count: u32,
    /// Maximum number of players allowed.
    pub max_players: u32,
}

/// Resource containing the list of publicly available lobbies.
///
/// Populated by backends that support lobby discovery. For Steam, friend lobbies
/// are tracked separately via `SteamFriendLobbies`. For WebRTC, this is populated
/// from the signaling server's lobby list.
#[derive(Resource, Clone, Debug, Default)]
pub struct PublicLobbies(pub Vec<PublicLobbyInfo>);

/// Relationship marking an entity as owned by a specific player.
///
/// Use this to associate game entities (characters, inventories, etc.) with the
/// player who controls them. The inverse is [`PlayerOwnedEntities`].
#[derive(Component)]
#[relationship(relationship_target = PlayerOwnedEntities)]
pub struct PlayerOwned(pub Entity);

/// Collects all entities owned by a player via [`PlayerOwned`].
///
/// Automatically maintained by Bevy's relationship system.
#[derive(Component, Default)]
#[relationship_target(relationship = PlayerOwned, linked_spawn)]
pub struct PlayerOwnedEntities(Vec<Entity>);
