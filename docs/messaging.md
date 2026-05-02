# Messaging

`bevy_ensemble` provides a type-safe message system for sending custom data over the network. Messages are serialized with CBOR, indexed by type for compact wire format, and delivered through Bevy's message system.

## Defining a Message

Any type that implements `Message`, `Serialize`, `Deserialize`, and `Clone` automatically satisfies the `EnsembleMessage` trait:

```rust,ignore
#[derive(Message, Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ChatMessage {
    sender_name: String,
    text: String,
}
```

## Registering Messages

Every message type must be registered during app setup. Registration order must be **identical on all peers** since types are identified by sequential index on the wire:

```rust,ignore
app.register_ensemble_message_type::<ChatMessage>()
    .register_ensemble_message_type::<PlayerAction>()
    .register_ensemble_message_type::<GameState>();
```

## Sending Messages

Trigger a `LobbyMessage<T>` on a lobby entity to send a message:

```rust,ignore
fn send_chat(mut commands: Commands, lobby: Single<Entity, With<Lobby>>) {
    let message = ChatMessage {
        sender_name: "Alice".into(),
        text: "Hello everyone!".into(),
    };
    commands.entity(*lobby).trigger(move |entity| LobbyMessage {
        entity,
        message,
    });
}
```

### Routing Rules

- **From a host lobby**: The message is forwarded to **all** connected `LobbyClient` entities.
- **From a client lobby**: The message is sent **to the host only**.

There is no direct client-to-client messaging. All messages flow through the host.

## Receiving Messages

Read incoming messages with a `MessageReader`:

```rust,ignore
fn receive_chat(mut messages: MessageReader<ReceivedEnsembleMessage<ChatMessage>>) {
    for msg in messages.read() {
        println!("[{:?}] {}: {}", msg.sender, msg.message.sender_name, msg.message.text);
    }
}
```

The `sender` field contains the `PlayerUUID` of the remote player, or `None` if unknown.

## The Host Relay Pattern

Since clients can only send to the host, a common pattern is:

1. Client sends an **intent** message to the host.
2. Host validates and processes the intent.
3. Host broadcasts a **result** message to all clients (including itself, if desired).

```rust,ignore
// Intent: client tells the host what they want to do
#[derive(Message, Clone, Debug, serde::Serialize, serde::Deserialize)]
struct MoveIntent {
    direction: Vec2,
}

// Result: host tells everyone what actually happened
#[derive(Message, Clone, Debug, serde::Serialize, serde::Deserialize)]
struct PlayerMoved {
    player_uuid: PlayerUUID,
    new_position: Vec3,
}

// On the host: validate and relay
fn handle_move_intents(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<MoveIntent>>,
    lobby: Single<Entity, (With<Lobby>, With<Host>)>,
) {
    for msg in messages.read() {
        let Some(sender) = msg.sender else { continue };

        // Validate, compute new position, etc.
        let new_position = Vec3::new(msg.message.direction.x, 0.0, msg.message.direction.y);

        // Broadcast the result to all clients
        let result = PlayerMoved { player_uuid: sender, new_position };
        commands.entity(*lobby).trigger(move |entity| LobbyMessage {
            entity,
            message: result,
        });
    }
}
```

## Message Encoding Pipeline

Understanding the internal pipeline can help with debugging:

```text
LobbyMessage<T>              (triggered by your code on a lobby entity)
       │
       ▼
LobbyClientMessage<T>        (routed to individual client entities)
       │
       ▼
SerializedLobbyPacket         (CBOR-encoded bytes, ready for transport)
       │
       ▼
Platform Backend              (sends bytes over the network)
```

On the receiving side:

```text
Platform Backend              (receives bytes from network)
       │
       ▼
decode_ensemble_packet()      (deserializes and dispatches)
       │
       ▼
ReceivedEnsembleMessage<T>    (read by your systems via MessageReader)
```

## Wire Format

Each packet is:
- **2 bytes**: Little-endian `u16` type index (assigned by registration order)
- **N bytes**: CBOR-encoded message payload

This keeps packets compact while supporting arbitrary custom types.
