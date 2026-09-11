# Migration

One section per phase of the netcode overhaul, in the order they landed. Each names what broke,
what to change in a consumer, and why. Both peers of a session must be built from the same
commit; the join handshake enforces it from phase E2 onward.

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
