# Writing a Platform Backend

`bevy_ensemble` defines the lobby model and message system, but relies on **platform backend** crates for actual network transport. This guide describes the contract that a backend must fulfill.

## Overview

A backend plugin is responsible for:

0. **Claiming the transport** (call `app.claim_transport("my_crate_name")` first — see below)
1. **Creating lobbies** on the platform (observe `RequestLobby` + `Host` entities)
2. **Joining lobbies** via platform-specific mechanisms
3. **Managing the lobby lifecycle** (`PendingLobby` to `Lobby` promotion)
4. **Sending packets** (observe `SerializedLobbyPacket` entity events)
5. **Receiving packets** (call `decode_ensemble_packet` when bytes arrive)
6. **Tracking connections** (spawn and manage `LobbyClient` entities)
7. **Setting player identity** (insert `LocalMultiplayerPlayerId` resource)

## Claiming the Transport

Every backend observes the same outbound event, so an app holding two of them encodes each message
once and sends it twice — once down each wire. Nothing downstream detects the duplicate; the
session just behaves strangely.

So a backend claims the transport as the first thing it does in `build`:

```rust,ignore
use bevy_ensemble::EnsembleTransportAppExt;

impl Plugin for MyBackendPlugin {
    fn build(&self, app: &mut App) {
        app.claim_transport("my_backend_crate");
        // ... everything else
    }
}
```

A second claim panics and names both backends. The name is what that message calls you, so use the
crate name. Game code can read the winner back out of the `TransportBackend` resource.

Two backends in one graph is nearly always a pair of cargo features that ought to be mutually
exclusive rather than additive.

## Required Components

When a remote player connects to a hosted lobby, the backend must spawn an entity with:

| Component | Purpose |
|-----------|---------|
| `LobbyClientPlayerUuid(uuid)` | Maps this connection to a `PlayerUUID` |
| `LobbyParticipantOf(lobby)` | Links this client to the lobby entity |

Then, once the connection is fully established, add `LobbyClient` to promote the entity and trigger participant creation.

The core systems will automatically:
- Create a `LobbyParticipant` for the new client
- Sync existing participants to the new client
- Broadcast the new participant to all other clients

## Lobby Lifecycle Management

### Host Creation Flow

1. Observe entities with `Added<RequestLobby>` and `Host`.
2. Initiate platform lobby creation asynchronously.
3. On success:
   - Remove `PendingLobby` and `RequestLobby` components.
   - Add `Lobby` component.
   - Add any platform-specific components (e.g. `LobbySteamId`).
   - Insert `LocalMultiplayerPlayerId` with the local player's real UUID.
4. On failure:
   - Despawn the entity.
   - Remove `LocalMultiplayerPlayerId` resource.

### Client Join Flow

1. Handle platform-specific join requests (messages, overlay callbacks, etc.).
2. Spawn an entity with `PendingLobby`.
3. Perform any handshake with the host.
4. On success:
   - Remove `PendingLobby`.
   - Add `Lobby`.
   - Insert `LocalMultiplayerPlayerId` with the local player's real UUID.
5. On failure:
   - Despawn the entity.

## Sending Packets

Add an observer for `SerializedLobbyPacket`:

```rust,ignore
fn send_packet(
    packet: On<SerializedLobbyPacket>,
    // ... your platform client resource, lobby queries, etc.
) {
    // Determine the target(s) based on the entity:
    // - If triggered on a host lobby entity: send to all remote members
    // - If triggered on a client lobby entity: send to the host
    // - If triggered on a LobbyClient entity: send to that specific client
    
    send_bytes_to_remote(target, &packet.packet);
}
```

## Receiving Packets

When bytes arrive from the network, call `decode_ensemble_packet`:

```rust,ignore
use bevy_ensemble::{decode_ensemble_packet, PlayerUUID};

fn read_incoming_messages(world: &mut World) {
    // Poll your platform for incoming packets
    let packets: Vec<(PlayerUUID, Vec<u8>)> = poll_network();
    
    for (sender_uuid, data) in packets {
        decode_ensemble_packet(world, Some(sender_uuid), &data);
    }
}
```

This will deserialize the packet and write the appropriate `ReceivedEnsembleMessage<T>` to the world's message buffer.

## Disconnection Handling

When a remote player disconnects from a hosted lobby:

1. Close the platform-level connection.
2. Broadcast a `RemoveLobbyParticipant` message so clients remove the participant:
   ```rust,ignore
   commands.entity(lobby_entity).trigger(move |entity| LobbyMessage::<RemoveLobbyParticipant> {
       entity,
       message: RemoveLobbyParticipant { player_uuid },
   });
   ```
3. Despawn the participant entity on the host.
4. Despawn the `LobbyClient` entity.

When the host disconnects (detected on the client side):

1. Close the platform-level connection.
2. Despawn the client's lobby entity.

## Connection State and ICE Restart

`bevy_ensemble_sockets` reports each peer through `EnsembleSocket::update_peers` as a
`PeerState`, in this order over a connection's life:

| State | Meaning | What the WebRTC backend does |
|-------|---------|------------------------------|
| `Connecting` | A connection exists; offer/answer and gathering are under way. Reported once, first. | Logs it. |
| `Connected` | The reliable data channel is open. | Logs it; the handshake takes it from here. |
| `Reconnecting` | ICE lost the path (Wi-Fi to cellular, a NAT rebinding) and is restarting. The data channels are still open; sends are queued by SCTP and delivered once a new pair is nominated. | Logs it, keeps everything: the host keeps the `LobbyClient`, the client keeps its lobby. |
| `Disconnected` | A connection that was open has ended. | Host: despawns the `LobbyClient`. Client: if it was the host, despawns the lobby (`LobbyLeft { HostGone }`). |
| `Failed` | Over for good: no pair ever worked, or a restart found none within `ICE_RESTART_TIMEOUT` (15 s) or `MAX_ICE_RESTARTS` (3). Nothing is reported after it. | Same teardown as `Disconnected`, plus `LobbyJoinFailed` with a reason. |

Only the side that made the original offer restarts ICE — in this protocol, the host — because a
restart *is* a new offer and a client's offers are refused. The host's ICE agent notices the loss
and sends an offer with `ice_restart` through the signalling server; the client answers it as it
would the first one, and reports `Reconnecting` while its own ICE is disconnected. Nothing new
crosses the signalling server for this: a second `PeerSignal::Offer` on an existing connection
is a renegotiation. `EnsembleSocket::restart_ice(peer)` asks for one explicitly — a "reconnect"
button — and `connected_peers()` includes reconnecting peers, so that pings and inputs keep
flowing through the window that decides whether the session survives.

A backend built on another transport has no obligation to restart anything, but should report
the same states with the same meanings, and in particular should not report `Disconnected` for a
loss it is about to recover from.

## Message Registration

If your backend uses internal messages for handshaking or protocol purposes, register them as ensemble message types:

```rust,ignore
impl Plugin for MyBackendPlugin {
    fn build(&self, app: &mut App) {
        app.register_ensemble_message_type::<MyHandshakeMessage>();
    }
}
```

This allows them to flow through the same serialization pipeline.

## Reference Implementation

See `bevy_ensemble_steam` (`crates/bevy_ensemble_steam/`) for a complete implementation that handles:
- Steam lobby creation and joining
- P2P networking with Steam's networking API
- Overlay-initiated joins
- Handshake protocol for connection readiness
- Connection reconciliation and stale client cleanup
