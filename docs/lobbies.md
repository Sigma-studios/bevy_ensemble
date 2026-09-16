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

What happens when the *host* leaves depends on the backend. If the lobby can migrate — the backend put
`HostMigratable` on it — another member becomes the host of the same lobby; see below. Otherwise every
client detects the disconnection through the platform backend and has its lobby entity despawned, with
`LobbyLeft { HostGone }`.

A host that wants the game over for everyone, rather than handed over, writes `CloseLobby`:

```rust,ignore
fn end_game(mut writer: MessageWriter<CloseLobby>) {
    writer.write(CloseLobby);
}
```

Members are told over the data channel, and through the platform too: the WebRTC signalling server
ends the lobby, and on Steam the lobby is marked closed in its data. A member that hears the host
go before either message arrives still leaves, rather than taking the lobby over.

## When the Host Leaves

In a migratable lobby, losing the host is a wait, not an ending:

```text
 client lobby ── host lost ──▶ Lobby + AwaitingHost { successor: None }
                                      │
                         arbiter names the new host
                          ┌───────────┴────────────┐
                    it is this peer            it is another peer
                          │                        │
                   Lobby + Host             Lobby + AwaitingHost { successor: Some(..) }
                                                   │
                                        protocols compared with it
                                                   │
                                                 Lobby
```

- The lobby **entity** stays: participants, `PlayerData`, anything the game put on them.
- `HostChanged { lobby, previous, new, promoted }` is written on every peer when the arbiter names
  the new host. `Added<Lobby>` does not fire again; `Added<Host>` marks a promotion.
- `AwaitingHost` is on the lobby while this peer waits. A game can show "the host left — waiting
  for a new one" from `Added<AwaitingHost>`.
- Messages in flight to or from the old host are lost, and a client drops what it sends while it is
  switching to the new host.
- If nobody is named within `HostMigratable::successor_within`, or the new host cannot be reached
  within `reach_within`, the session ends with `LobbyLeft { HostGone }`. A game can shorten or
  lengthen both with the `HostMigrationTimeouts` resource.

```rust,ignore
// Am I waiting for a host?
fn waiting(lobby: Option<Single<&AwaitingHost, With<Lobby>>>) {
    if let Some(awaiting) = lobby {
        println!("waiting {:.0}s for a new host", awaiting.waited.as_secs_f64());
    }
}
```

Which peer becomes host is the platform's decision: the earliest-joined member on the WebRTC
signalling server, Steam's own choice of lobby owner on Steam.

## Host vs Client

The **host** is the authoritative peer — and in a migratable lobby, the role can move to another peer
while the lobby entity stays, so query `With<Host>` each frame rather than deciding once:

- The host's participant list is the source of truth, synced automatically to all clients.
- When a `LobbyMessage` is triggered on a host lobby, it is forwarded to **all** connected clients.
- When a `LobbyMessage` is triggered on a client lobby, it is sent **only to the host**.

This makes the host the natural place for game-authoritative logic. A common pattern is for clients to send **intents** to the host, and the host broadcasts **results** to everyone. See the [messaging guide](messaging.md) for details.
