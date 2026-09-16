# squiggles

The consumer that patched the most around the transport: an upstream patch series, a reorder
probe, sender checks, connection guessing, a stuck-join watchdog and a wire-format test that
decoded indices off the wire.

| Step | Delete | Replaced by | Covered by |
|---|---|---|---|
| 1 | `upstream-patches/0001-sockets-one-sender-task-per-peer.patch` | landed in E1 (`sockets/src/native.rs`: one writer task per peer) | `a_burst_of_nine_reliable_sends_arrives_in_order` |
| 2 | `tests/wire_format.rs` (216 lines): pinning every index, `index_of` that decodes the wire | `EnsembleMessageRegistry::{index_of, wire_names, wire_hash}` (E0, E2) | `wire_indices_do_not_depend_on_registration_order` |
| 5 | the client-side "is this from the host" checks (§15 of `docs/upstream-needs.md`) | `MessageAuthority::HostOnly`, the relay's stamped sender (E1) | `a_relayed_envelope_carries_the_transport_sender_not_the_claimed_one`, `a_roster_sync_from_a_non_host_is_ignored` |
| 6 | `session/lobby.rs::track_connection` guessing at the connection from packet arrival | `PeerState` from the socket (E3) | `a_peer_state_transition_is_visible_to_the_game` |
| 4, 6 | `session/mod.rs::watch_stuck_joins` | `LobbyLeft { reason }` after `peer_timeout`, ICE restart before it (E1, E3) | `a_peer_that_stops_answering_pings_is_despawned_after_the_timeout`, `an_ice_restart_keeps_the_data_channel_open` |
| 7 | `src/testing/mod.rs` (612 lines): `leave`, `rejoin`, `rehost`, `add_client_with_key`, `trace_packets`, `SentPacket`, `Lcg`, `reorder_probe.rs` | the same names on `LoopbackNetwork`; `Link { reorder, duplicate }` (E0) | `an_unreliable_packet_can_be_reordered`, `the_same_seed_replays_the_same_trace` |

**Watch:** `docs/netcode.md` describes the pre-E1 relay, where the sender was whatever the
envelope claimed. Rewrite its trust section from `docs/TRUST_MODEL.md`.

## Host changes (E5)

squiggles picks this up on its next `cargo update` (it tracks `master`, locked at `4880d0b`). Over
WebRTC nothing migrates until the signalling server runs E5b. After that, a host that leaves or
crashes no longer sends everyone to the menu: the lobby stays, and `track_lobby_loss`
(`session/mod.rs:194`) never fires. The game's own session layer (`HostState`, `ClientState`,
seats, `SessionControl`) still believes the old host exists, so step 8 is code in the game, not a
deletion.

| Where | Today | On a host change | Covered by |
|---|---|---|---|
| `session/mod.rs` (new system) | nothing reads `HostChanged` | on `HostChanged`: run `handle_leave`'s resets **without** writing `LeaveLobby` (despawn surfaces, reset `HostState`/`ClientState`/`Presences`), set `SessionInfo.is_host = promoted`, go to `AppState::Lobby` | `every_peer_is_told_who_the_host_became` |
| `session/mod.rs:79` `SessionInfo.is_host` | set once when connecting (`:162`, `menu.rs`, `autostart.rs`) | rewrite it on `HostChanged`, or replace its readers with a live `With<Host>` query | `the_named_successor_becomes_the_host_and_everyone_else_follows` |
| `session/mod.rs:213` `spawn_shared_surface_on_host` | fires on `Added<Lobby>` | also fire on `Added<Host>`: a promotion inserts `Host` on the lobby that already exists | `the_named_successor_becomes_the_host_and_everyone_else_follows` |
| `net/host.rs:262` `host_seat_self`, `HostState.roster` | seats built as clients say hello | the promoted peer starts with an empty `HostState`; it seats itself at 0 and every follower's hello seats it again | `participant_entities_and_player_data_survive_a_host_change` |
| `net/client.rs` hello | sent once per join | a follower sends it again once `AwaitingHost` is removed (that is when the new host verified it); anything sent before is dropped | `messages_sent_while_switching_hosts_are_dropped_not_delivered_late`, `a_message_sent_as_the_new_host_is_verified_reaches_it` |
| `drawful/host.rs:45` `DrawfulHost` | prompts, lies, votes and the round timer exist only on the host | abandon the round: its secrets left with the old host (`drawful/mod.rs:7`). Leaving `AppState::Drawful` already runs `stop`/`exit` | — |
| `session/lobby.rs:73`, `modes/infinite.rs:83`, `drawful/client.rs:213`, `:1038` | Start, mode, "Clear this area" and "End game"/"Leave" fixed from `is_host` on enter | rebuild the screen on `HostChanged`; returning to `AppState::Lobby` re-enters it | — |
| `modes/infinite.rs:386` `sync_status` | "waiting for the host… (Ns)" from `PeerLastPong` | read `AwaitingHost { waited, successor }`: "the host left, waiting for a new one" or "reaching the new host" (`PeerLastPong` is removed during the change). `HostGone` now arrives only after about 90 s of waiting | `a_client_that_loses_its_host_keeps_its_lobby_and_waits` |
| `drawful/client.rs:1085`, `modes/infinite.rs:143`, `session/lobby.rs:150` | a host's Leave writes `LeaveSession` → `LeaveLobby` | `LeaveLobby` by a host now hands the lobby to the earliest-joined player. Add "End game for everyone" writing `CloseLobby` where the host should end it for all | `a_closed_lobby_ends_for_everyone_and_does_not_migrate`, `a_closed_lobby_ends_for_everyone_and_nobody_takes_it_over` |
| `testing/mod.rs:177` `rehost()`, `tests/session.rs:139` | models "host leaves, every lobby despawned" | keep it for the old path; add a test with `LoopbackNetwork::set_host_migration` and `migrate` that checks every survivor lands in `AppState::Lobby` with the same roster | `a_second_migration_works_like_the_first` |

**Keep:** `track_lobby_loss`. It still ends the session when no new host is named, or the new host
cannot be reached (`LobbyLeft { HostGone }`), and after `CloseLobby`.
