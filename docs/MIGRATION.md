# Migration

One section per phase of the netcode overhaul, in the order they landed. Each names what broke,
what to change in a consumer, and why. Both peers of a session must be built from the same
commit; the join handshake enforces it from phase E2 onward.

## E4 — the first real join

Nothing on the wire changed. The first session between two processes over a real data
channel — `bevy_ticked`'s `netpeer` example, run by its multi-process test — never got past
the join, twice, for reasons the in-process harness could not produce. Both fixed here, with
loopback regression tests.

### A backend's ready handshake is decoded before the protocol is compared

**Before** every message from a peer whose protocol was not yet verified was held, the
protocol handshake excepted. The WebRTC backend promotes a pending lobby on its own ready
handshake, and the protocol handshake is announced on the promotion: held behind the
comparison it was meant to lead to, the ready handshake never arrived and no real join
completed. **After** `register_backend_handshake_message_type::<T>(name, authority)`
registers a backend's ready handshake as one decoded before verification; the WebRTC and
Steam backends use it. Every other control message still waits.
**What to change** A third-party backend registers its ready handshake with the new method
instead of `register_control_message_type`.

### A protocol match that arrives before its seat is kept

**Before** a client's protocol handshake that reached the host a frame before the host had
promoted it found no `LobbyClient` to mark and was dropped; it is sent once, so the join then
timed out. The same on a client whose lobby was still pending. **After** the match is
remembered (`ProtocolMatched`) and applied when the seat appears. No change for consumers.

## E3 — ICE restart and connection state

Nothing on the wire changed. A WebRTC session now survives a network path change — a phone
moving from Wi-Fi to cellular, a NAT rebinding a mapping — by restarting ICE and renegotiating
through the signalling server with the data channels kept. Before, ICE `disconnected` was logged
and hoped about, and `failed` ended the session.

### `PeerState` gained `Connecting` and `Reconnecting`

**Before** `PeerState` was `Connected | Disconnected | Failed`.
**After** `Connecting | Connected | Reconnecting | Disconnected | Failed`. `Connecting` is
reported once, first, for every peer. `Reconnecting` is reported from `Connected` when ICE loses
the path and is restarting; `Connected` follows when a new pair is nominated, `Failed` if none is
found within `ICE_RESTART_TIMEOUT` (15 s) or after `MAX_ICE_RESTARTS` (3) — both `pub const` in
`bevy_ensemble_sockets`. Nothing is reported after `Failed`.
**What to change** An exhaustive `match` on `PeerState` needs the two new arms. The right
reaction to `Reconnecting` is to do nothing: the channels are open, sends are queued by SCTP and
delivered when the path is back, and the socket will say `Connected` or `Failed`. Tearing down
on it turns a two-second blip into a lost session.
**Watch** `connected_peers()` now includes `Reconnecting` peers, since their channels are open.
Code that used it to mean "ICE has a pair right now" should track `update_peers` itself.

### `EnsembleSocket::restart_ice(peer)`

New. Restarts ICE on demand — for a "reconnect" button, or a test. Only the side that made the
original offer (the host, in the WebRTC backend) can restart, because a restart is an offer and
the other side's offers are refused; on the answerer it logs and does nothing, and the offerer's
ICE agent notices the same loss on its own. Counts against `MAX_ICE_RESTARTS`.

### What a game sees

On the host, a client whose path changes stays a `LobbyClient` throughout; on the client, the
lobby stands. `poll_socket_peers` logs `Reconnecting` at info and does nothing else. The only
new outcome is `Failed` *after* a session was up, when the restart finds no path in time: the
same teardown as `Disconnected` (`LobbyLeft { HostGone }` on the client, the `LobbyClient`
despawned on the host) plus a `LobbyJoinFailed` whose reason says the connection was lost rather
than never made.

`EnsembleSocket::discarded_candidates()` is new too: a count of remote ICE candidates the
transport refused, zero on a healthy socket, for soak tests to assert on. The native backend now
holds its own candidates until the offer or answer they belong to has been sent, so that a
restart's regathered candidates cannot arrive ahead of the offer that carries its credentials.

### `LivenessGrace`: the liveness check waits for the restart

New component in `bevy_ensemble`. `PeerTimeout` (default 5 s) would otherwise end the session a
restart was about to save: ICE takes a few seconds to notice a lost path and a restart a few
more. While a peer is `Reconnecting`, the WebRTC backend inserts
`LivenessGrace { extra: ICE_RESTART_TIMEOUT }` on that peer's entity (the `LobbyClient` on the
host, the lobby on a client) and removes it on `Connected`; `detect_dead_peers` adds `extra` to
the timeout while it is present. The grace adds, it does not replace: a peer that never comes
back is dropped after timeout plus grace, and a restart that fails reports `Failed` and tears
down on its own before that. A game or another backend with its own reason to expect silence
(a suspended tab it was told about) may insert it too.

## E2 — protocol v2: named types, a join handshake, framed datagrams, real latency

**This is a wire-format change.** Every peer of a session must be built from this commit or
later; the join handshake refuses anything else, and says which registration differs.

### Every message type is registered under a wire name

**Before**
```rust
app.register_ensemble_message_type::<ChatMessage>()
   .register_broadcast_message::<Wave>();
```
**After**
```rust
app.register_ensemble_message_type::<ChatMessage>("ChatMessage")
   .register_broadcast_message::<Wave>("Wave");
```
**Why** The index a type travels under is now its rank among the *sorted* wire names, not the
order plugins happened to register in. Two peers agree on every index exactly when they
registered the same names, and plugin order stops being a wire format. `wire_hash()` is the
number the handshake compares; `wire_names()` is the list. Renaming the Rust type is free;
renaming the wire name is a protocol change. Libraries use `"<crate>/<Type>"`.
**Delete** Golden tests that pin registration order; the `ProtocolEpoch` phantom component
bevy_kart registered to fold this registry's shape into another handshake; any comment saying
"plugin add order is part of the wire format".
**Watch** Registration after the first message has been encoded or decoded panics with the
latecomer's name. Register in `build`.

### The join handshake

The core sends a `ProtocolHandshake` (version, hash, sorted names) on every new `Lobby` and
`LobbyClient`. A match inserts `HandshakeVerified` on the client's lobby entity and on the host's
`LobbyClient`; anything that must not send to an unverified peer waits for it. A mismatch ends the
join on both sides: the client gets `LobbyLeft { reason: ProtocolMismatch(text) }` and
`LobbyJoinFailed`, the host despawns the `LobbyClient` and gets `LobbyJoinFailed`, and `text`
names the first differing registration. Backends' own ready handshakes are unchanged.

### One datagram per peer per channel per frame

Every message encoded during a frame is batched and flushed in `Last` as one
`SerializedLobbyPacket` per `(peer, channel)`, framed under a reserved type index. A single
message is not framed and costs what it did. `ReliableNoDelay` goes alone. Frames never exceed
`MAX_DATAGRAM_BYTES` (60 000); an unreliable frame past `UNRELIABLE_ADVISORY_BYTES` (1 200) is
logged once. `NetMetrics` gained `tx_messages`, `rx_messages` and `largest_unreliable_packet`;
`tx_packets`/`rx_packets` now count datagrams.
**Watch** A backend observing `SerializedLobbyPacket` sees it at the end of the frame, not at
the trigger. A packet addressed to an entity despawned earlier in the frame (the kick
notification) no longer resolves on backends that look the entity up; the loopback backend
remembers recent clients and still delivers it.

### `received_at` is an `Instant`

`ReceivedEnsembleMessage::received_at` is `bevy_ensemble::Instant` (a `web_time::Instant`, so it
works on wasm), stamped by the backend on its receiving task — not `Time::elapsed()` at decode.
`decode_ensemble_packet` takes it as a fourth argument. Peer dwell is now the time an app really
held a ping, and `PeerWireRtt` is no longer identical to `PeerRtt`. Backends that construct
`ReceivedEnsembleMessage` themselves pass `Instant::now()`.

### Pings on both channels

`EnsemblePing`/`EnsemblePong` carry `reliable: bool`; one of each goes out per interval. The new
`PeerReliableRtt` is the round trip on the reliable, ordered channel — retransmits and
head-of-line blocking included — which is what a lockstep buffer has to cover. `PeerRtt`,
`PeerWireRtt` and `PeerRttJitter` keep their meaning (unreliable channel).

### What to delete in consumers

| Consumer | Delete | Use instead |
|---|---|---|
| bevy_kart `src/main.rs`, `src/wire_format.rs` | `ProtocolEpoch`, "plugin add order is the wire format" | `wire_hash()`, the handshake |
| squiggles `tests/wire_format.rs` | pinning every index | `wire_names()` |
| bevy_ticked lockstep adaptive buffer | sizing from `PeerRtt` | `PeerReliableRtt` |

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
