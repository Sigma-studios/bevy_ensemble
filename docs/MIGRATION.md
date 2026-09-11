# Migration

One section per phase of the netcode overhaul, in the order they landed. Each names what broke,
what to change in a consumer, and why. Both peers of a session must be built from the same
commit; the join handshake enforces it from phase E2 onward.

## E1 — the trust boundary, liveness, and no placeholder identity

Nothing on the wire changed shape. What changed is who is believed.

### The transport sender is the only sender

**Before** a broadcast envelope's `sender` field was believed as written, so a client could put the
host's uuid in it; `SyncPlayerData { player_uuid }` was applied to whatever player it named; a
client applied `SyncLobbyParticipant` and `RemoveLobbyParticipant` from any peer.
**After** the host overwrites the envelope's sender with the transport's; player data is accepted
on the host only for the sender's own uuid; and every message type carries a
[`MessageAuthority`]: `Any` (the default) or `HostOnly`, which a client accepts only from the peer
named by the new `HostUuid` resource. Refusals are counted in `RefusedPackets` and logged three
times, then at debug level.
**What to change** Nothing for `Any` types. Register anything a client must take only from its
host with `register_ensemble_message_type_with::<T>(MessageAuthority::HostOnly)`. Protocol-level
types that must never travel inside a broadcast envelope (handshakes, pings) use
`register_control_message_type`. Game-side sender checks that compared `message.sender` to the
host are now redundant and can go.
**Watch** `HostUuid` must be present on a client before host-only traffic arrives. Every shipped
backend sets it as part of joining; a custom backend has to. A client that has not been told who
its host is trusts nobody with an authoritative message.

### Pings carry a sequence number, not a timestamp

`EnsemblePing { seq }` / `EnsemblePong { seq, dwell_micros }`. A pong that answers no ping this peer
sent is ignored; a claimed dwell is clamped into the round trip; a round trip over 30 s is not a
sample. `PeerRtt`, `PeerWireRtt`, `PeerRttJitter` keep their meaning. Nothing to change unless you
constructed these types yourself.

### Dead peers are noticed

`PeerTimeout(Option<Duration>)`, default 5 s, inserted by `EnsemblePlugin`. A client that has not
answered in that long is despawned on the host as if it had disconnected; a host that has not
answered ends the client's session with `LobbyLeft { reason: PeerTimeout }`. `PeerLastPong` now
exists from the moment a peer is known, not from its first pong. Override the resource to change
the limit or set `PeerTimeout(None)` to disable.

### `LobbyLeft` says why a session ended

A new message, `LobbyLeft { reason: LobbyLeftReason }`, with reasons `Left`, `Kicked`, `HostGone`,
`PeerTimeout`, `SignallingLost`, `ProtocolMismatch(String)`. Read it instead of
`RemovedComponents<Lobby>`, which never fires for a join that was refused and cannot tell a kick
from a crash.

### There is no placeholder identity

**Before** `StartHosting` inserted `LocalMultiplayerPlayerId(LOCAL_PLAYER_UUID)` (zero) until the
backend replaced it, and a host once adopted its role under it.
**After** the resource is absent until the backend knows the identity. `LOCAL_PLAYER_UUID` is
gone. `add_host_lobby_participant` no longer depends on `Added<Lobby>`, so a participant is
created whenever the identity arrives. A broadcast sent while the identity is unknown is dropped
with a warning rather than sent as player zero.
**What to change** Anything that read `LocalMultiplayerPlayerId` unconditionally: take it as
`Option<Res<_>>`. Anything that compared against `LOCAL_PLAYER_UUID` to mean "not yet known":
compare against absence. Games with a solo mode that reused the constant pick their own.

### Backends

- `bevy_ensemble_sockets` native: one writer task per peer, so same-frame reliable sends arrive
  in order (they did not; a consumer measured 14–75% reordering). `disconnect_peer` closes the
  peer connection (its tasks and sockets used to leak). WASM negotiation errors are warnings, not
  panics.
- `bevy_ensemble_webrtc`: a client keeps the host's uuid from `LobbyJoined`, sends only to it,
  accepts offers and the ready handshake only from it, and drops packets from anyone else. Another
  peer's disconnect no longer tears the client's lobby down. A signalling keep-alive is sent every
  20 s. The signalling server hands out random lobby ids, requires authentication before
  `ListLobbies`, rate-limits, closes idle connections, clamps `max_players`, and relays signals
  only between a lobby's host and a member.
- `bevy_ensemble_steam`: P2P session requests are accepted only from lobby members; packets are
  decoded only from members (host) or the lobby owner (client).

### What to delete in consumers

| Consumer | Delete | Use instead |
|---|---|---|
| squiggles | client-side "is this from the host" checks (§15) | `MessageAuthority::HostOnly` |
| squiggles, bevy_factory, run-2d | `RemovedComponents<Lobby>`-based teardown and stuck-join timers | `LobbyLeft` |
| bevy_factory `game/multiplayer.rs` | `LocalPlayerIdentity` + `sync_local_player_identity` placeholder handling | `Option<Res<LocalMultiplayerPlayerId>>` |
| una_zombies | `SOLO_PLAYER_UUID = LOCAL_PLAYER_UUID` | a constant of its own |
| squiggles | `docs/upstream-patches/0001-sockets-one-sender-task-per-peer.patch` | landed |

## E0 — loopback harness additions

The `bevy_ensemble_loopback` backend grew the things every consumer had written around it.
Nothing in the message path changed; two API edges did.

### `Link` gained fields — use the builders

**Before**
```rust
Session::new(2).with_link(Link { delay: ms(60), jitter: ms(120), loss: 0.15 })
```
**After**
```rust
Session::new(2).with_link(Link::perfect().with_delay(ms(60)).with_jitter(ms(120)).with_loss(0.15))
// or: Link { delay: ms(60), jitter: ms(120), loss: 0.15, ..Link::perfect() }
```
**Why** `Link` now also carries `duplicate`, `reorder` and `max_message_size`, all of which apply
to unreliable packets only. A struct literal that names three fields no longer compiles.
`Link::bad_wifi()` now duplicates 1% of unreliable packets, mirroring `NetPreset::BadWifi`.

### `set_link` clears per-direction overrides

`set_link(link)` sets the default for every direction *and* forgets any
`set_link_between`/`set_link_pair` override. Call it first, then override.

### New, no change needed

- Per-direction links: `set_link_between(from, to, link)`, `set_link_pair(a, b, a_to_b, b_to_a)`,
  `link_between(from, to)`. `PeerRtt` published to each client now reflects its own two links.
- Lifecycle the real backends have: `add_pending_client` + `promote`, `set_local_id(peer, Option)`,
  `leave` / `rejoin`, `rehost(new_host)`, `half_open` / `reconnect`. `is_connected` now answers
  `false` for pending, half-open, disconnected and departed peers; `is_pending`, `is_half_open`,
  `has_left` and `try_lobby` are the finer questions.
- Stepping: `update_peer`, `step_only(&[peers])` (everyone else is frozen), `step_with(closure)`.
- Observation and faults: `trace_packets(true)`, `trace()` / `take_trace()` of `SentPacket { frame,
  from, to, mode, bytes, fate }`, `bytes_sent` / `packets_sent` counters (always on), `drop_next`,
  `corrupt_next`, `deliver_raw`.
- `seed(u64)` and `rng()` (a `SeededRng`) for reproducible test content. Consumers that carried
  their own `Lcg` can delete it.
- `EnsembleMessageRegistry::index_of::<T>()` is public. A test that pinned indices by encoding a
  value of each type and reading the two-byte prefix back can ask the registry instead.

### What to delete in consumers

| Consumer | Delete | Use instead |
|---|---|---|
| squiggles `src/testing/mod.rs` | `leave`, `rejoin`, `rehost`, `add_client_with_key` scaffolding; `trace_packets`/`SentPacket`; `Lcg` | the same names on `LoopbackNetwork`; `SeededRng` |
| squiggles `tests/wire_format.rs` | `index_of` that decodes the wire | `EnsembleMessageRegistry::index_of` |
| bevy_factory `src/testing/harness.rs` | `step_frame_with_ticks` plumbing over `advance`/`update_all` | still needs the game's tick driver; `step_with` gives the per-peer hook |
| run-2d `src/net_tests.rs` | the freeze-a-peer loop (`advance(1)` + `host.update()` + `collect_outbound()`) | `step_only(&[host])` |
