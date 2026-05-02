# Lobbies

A **lobby** is the central concept in `bevy_ensemble`. It's a Bevy entity that represents a multiplayer session. Players can create lobbies (becoming the **host**) or join existing ones (becoming **clients**).

## Lobby Entity Lifecycle

```text
StartHosting message
        │
        ▼
 ┌──────────────┐     Platform backend     ┌────────┐
 │ PendingLobby │ ─── creates on network ──▶│ Lobby  │
 │ RequestLobby │                           │ Host   │
 │ Host         │                           └────────┘
 └──────────────┘

Join request (backend-specific)
        │
        ▼
 ┌──────────────┐     Handshake complete    ┌────────┐
 │ PendingLobby │ ─────────────────────────▶│ Lobby  │
 └──────────────┘                           └────────┘
```

### Component States

| Component | Meaning |
|-----------|---------|
| `PendingLobby` | Lobby creation/join is in progress. Not yet usable. |
| `RequestLobby` | This entity was requested for creation (host only). Consumed by the backend. |
| `Host` | The local player owns this lobby and is the authority. |
| `Lobby` | The lobby is fully active and connected. Safe to send messages. |

## Creating a Lobby (Hosting)

Write a `StartHosting` message to create a new hosted lobby:

```rust,ignore
fn start_hosting(mut writer: MessageWriter<StartHosting>) {
    writer.write(StartHosting);
}
```

This spawns an entity with `(PendingLobby, RequestLobby, Host)`. The platform backend observes `RequestLobby` and initiates network lobby creation. Once the platform confirms, it removes `PendingLobby` and `RequestLobby`, then adds `Lobby`.

Only one hosted lobby is allowed at a time. Additional `StartHosting` messages are ignored while one exists.

## Joining a Lobby

Joining is **backend-specific** because different platforms discover and join lobbies differently.

### Steam Example

```rust,ignore
use bevy_ensemble_steam::JoinSteamLobby;

// Join by lobby ID (e.g. from friend list)
fn join_lobby(mut writer: MessageWriter<JoinSteamLobby>, lobby_id: LobbyId) {
    writer.write(JoinSteamLobby(lobby_id));
}
```

Players can also join via the Steam overlay, which is handled automatically by the Steam backend.

The join flow creates a `PendingLobby` entity, performs a handshake with the host, and then promotes it to `Lobby` once the connection is established.

## Querying Lobbies

```rust,ignore
// Am I in any active lobby?
fn check_lobby(lobby: Option<Single<Entity, With<Lobby>>>) {
    if let Some(lobby) = lobby {
        println!("In lobby {:?}", *lobby);
    }
}

// Am I the host?
fn check_hosting(lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>) {
    if lobby.is_some() {
        println!("I'm hosting!");
    }
}

// Am I a client?
fn check_client(lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>) {
    if lobby.is_some() {
        println!("I'm a client!");
    }
}

// Is a lobby still loading?
fn check_pending(pending: Query<Entity, With<PendingLobby>>) {
    for entity in pending.iter() {
        println!("Lobby {:?} is still connecting...", entity);
    }
}
```

## Leaving a Lobby

To leave or close a lobby, despawn the lobby entity. The platform backend handles the network-level cleanup (closing connections, leaving the platform lobby, etc.):

```rust,ignore
fn leave_lobby(mut commands: Commands, lobby: Single<Entity, With<Lobby>>) {
    commands.entity(*lobby).try_despawn();
}
```

When the host leaves, all clients will detect the disconnection through the platform backend and have their lobby entities despawned automatically.

## Host vs Client

The **host** is the authoritative peer:

- The host's participant list is the source of truth, synced automatically to all clients.
- When a `LobbyMessage` is triggered on a host lobby, it is forwarded to **all** connected clients.
- When a `LobbyMessage` is triggered on a client lobby, it is sent **only to the host**.

This makes the host the natural place for game-authoritative logic. A common pattern is for clients to send **intents** to the host, and the host broadcasts **results** to everyone. See the [messaging guide](messaging.md) for details.
