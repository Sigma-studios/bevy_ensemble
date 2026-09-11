# bevy_factory

A lockstep game over WebRTC with the in-process harness (`src/testing/harness.rs`) that
`bevy_ensemble_loopback` grew to match, and the multi-process signalling test that the
`test_support::SignallingServer` came from.

| Step | Delete | Replaced by | Covered by |
|---|---|---|---|
| 3 | `game/multiplayer.rs::LocalPlayerIdentity` and `sync_local_player_identity` | `Option<Res<LocalMultiplayerPlayerId>>`, absent until the backend knows (E1) | `no_identity_is_published_until_the_backend_has_one` |
| 4 | `return_to_menu_when_client_lobby_is_lost` and the `RemovedComponents<Lobby>` teardown | `LobbyLeft { reason }` (E1) | `a_live_peer_is_never_dropped_under_satellite_jitter` |
| 7 | `src/testing/harness.rs` link presets, per-direction links, `step_frame_with_ticks` plumbing over `advance`/`update_all` | `Link::{cable, bad_wifi, satellite}`, `set_link_pair`, `step_with` (E0) | `links_can_differ_per_direction`, `one_step_is_exactly_one_frame` |
| 7 | `tests/webrtc_multiprocess.rs::SignallingServer` | `bevy_ensemble_webrtc::server::test_support::SignallingServer` (E1) | `signalling.rs` |
| — | the ping-based buffer seed's use of `PeerRtt` | `PeerReliableRtt` (E2) | `wire_rtt_subtracts_the_dwell` |

**Watch:** `bevy_ticked_lockstep_networking` (T12) now reads `PeerReliableRtt` for its first
buffer estimate and `PeerState` for kicks; a game that reached into `PeerRtt` directly reads
the reliable one.
