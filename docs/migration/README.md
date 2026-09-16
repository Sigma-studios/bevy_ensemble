# Migrating a consumer

`../MIGRATION.md` explains every change by phase (E0 to E5e). These are the per-consumer
checklists: what each game or library deletes, in the order to apply it.

1. **Pin** the commit. Both peers of a session must be built from the same one; from E2 the
   join handshake refuses a mismatch and names it.
2. **Registrations**: `register_ensemble_message_type::<T>("name")` for every message type
   (`_with(name, MessageAuthority::HostOnly)` for what only the host may send); delete every
   index pin and epoch.
3. **Identity**: `Option<Res<LocalMultiplayerPlayerId>>`; nothing is published until the backend
   knows who you are. Delete placeholder handling.
4. **Session end**: read `LobbyLeft { reason }`; delete `RemovedComponents<Lobby>` teardown and
   stuck-join timers.
5. **Trust**: delete client-side "is this from the host" checks; the relay stamps the transport
   sender and `HostOnly` types are dropped from anyone else.
6. **Connection state**: match `PeerState::{Connecting, Connected, Reconnecting, Disconnected,
   Failed}`; do nothing on `Reconnecting`.
7. **Tests**: `bevy_ensemble_loopback`'s `Link` builders, `trace_packets`, `step_only`,
   `add_pending_client`/`promote`, `leave`/`rejoin`/`rehost`, `half_open`; delete the copies.
8. **Host changes** (E5): a lobby survives its host on WebRTC (once the server is E5b) and on
   Steam. Read `HostChanged` and go back to your lobby screen, resetting whatever round state the
   old host owned. Show `AwaitingHost` while a client waits: `HostGone` can now take up to 90 s
   on WebRTC and 60 s on Steam. Stop treating an empty `(Lobby, Without<Host>)` query as the end
   of the session, because a promoted client matches it; read `LobbyLeft`. Query `With<Host>`
   each frame rather than caching the role when a screen is built. Give the host an "end for
   everyone" action that writes `CloseLobby`, since `LeaveLobby` by a host now hands the lobby
   over.
