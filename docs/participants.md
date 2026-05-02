# Participants

Every player in a lobby (including the host) is represented by a **participant entity**. These entities track who is in the lobby and are automatically synchronized across the network.

## The Participant Entity

Each participant is an entity with:

| Component | Purpose |
|-----------|---------|
| `LobbyParticipant` | Holds the player's `player_uuid` and `is_host` flag |
| `LobbyParticipantOf(entity)` | Relationship pointing to the owning lobby entity |

The lobby entity itself has a `LobbyParticipants` component that collects all participant entities via Bevy's relationship system.

## Querying the Roster

```rust,ignore
use bevy_ensemble::*;

// List all participants in a lobby
fn show_roster(
    lobby: Single<&LobbyParticipants, With<Lobby>>,
    participants: Query<&LobbyParticipant>,
) {
    for &entity in lobby.iter() {
        if let Ok(participant) = participants.get(entity) {
            let role = if participant.is_host { "Host" } else { "Client" };
            println!("[{}] Player {}", role, participant.player_uuid);
        }
    }
}

// Find the host participant
fn find_host(
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    lobby: Single<Entity, With<Lobby>>,
) {
    if let Some((host, _)) = participants.iter().find(|(p, of)| {
        of.0 == *lobby && p.is_host
    }) {
        println!("Host is player {}", host.player_uuid);
    }
}

// Check if a specific player is in the lobby
fn is_player_connected(
    participants: Query<&LobbyParticipant>,
    target_uuid: PlayerUUID,
) -> bool {
    participants.iter().any(|p| p.player_uuid == target_uuid)
}
```

## How Participants are Synced

Participant synchronization is **host-authoritative** and fully automatic:

1. When the host's lobby becomes active, the host is added as a participant with `is_host: true`.
2. When a remote client connects (gets `LobbyClient` component), a participant is created for them.
3. Changes to participants on the host are broadcast to all clients via `SyncLobbyParticipant` messages.
4. When a player disconnects, the host broadcasts a `RemoveLobbyParticipant` message and clients despawn the corresponding participant entity.

You don't need to manage this yourself. Just query the participant entities to see who's in the lobby.

## Player Identity

Each player has a `PlayerUUID` (a `u128`) that uniquely identifies them for the session. The local player's UUID is stored in the `LocalMultiplayerPlayerId` resource:

```rust,ignore
fn my_uuid(local_id: Res<LocalMultiplayerPlayerId>) {
    println!("My player UUID: {}", local_id.0);
}
```

Platform backends map their native identifiers to `PlayerUUID`. For example, the Steam backend uses `u128::from(steam_id.raw())`.

## Player Ownership

The `PlayerOwned` and `PlayerOwnedEntities` relationship lets you associate game entities with the player who controls them:

```rust,ignore
// Spawn a character owned by a player
fn spawn_character(mut commands: Commands, player_entity: Entity) {
    commands.spawn((
        // Your character components...
        PlayerOwned(player_entity),
    ));
}

// Query all entities owned by a specific player
fn player_entities(
    player: Single<&PlayerOwnedEntities>,
) {
    for &entity in player.0.iter() {
        // Process owned entities...
    }
}
```

This is a general-purpose ownership relationship. You can use it to track which characters, inventories, or other game objects belong to which player.
