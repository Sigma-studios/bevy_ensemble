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
