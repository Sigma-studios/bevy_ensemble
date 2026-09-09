# Deploying the server

`bevy_ensemble_webrtc_server` is the deployable half of this workspace: the signalling server peers
find each other through, and the TURN relay their traffic falls back to when they cannot reach each
other directly. One process, two listeners.

## Building something the server can actually run

```sh
cargo build --release --target x86_64-unknown-linux-musl \
    -p bevy_ensemble_webrtc --no-default-features --features server \
    --bin bevy_ensemble_webrtc_server
```

Two flags in there are not optional, and both were learned the hard way.

**`--no-default-features`.** The crate's default feature is `client`, which pulls `bevy`, which
pulls `wgpu` and `ash`. A server binary built without this contains a Vulkan loader it will never
call: 74 MB against 2.6 MB, and a build that fails on a machine with no graphics headers.

**`--target x86_64-unknown-linux-musl`**, with static linking. A glibc binary is linked against the
build machine's loader — on NixOS that is a `/nix/store/…` path that does not exist anywhere else,
so copying it to a server produces `No such file or directory` for a file that is plainly there.
Even on an ordinary distribution, a binary built against a newer glibc than the target has will
fail on symbol versions. A static musl build has no interpreter and no libc dependency, so it runs
on anything with the same architecture.

`ring` (via `turn`) compiles C, so the musl target needs a musl C compiler:

```sh
# NixOS: get the compiler path, then build outside the shell.
nix-shell -p musl --run 'which musl-gcc'

CC_x86_64_unknown_linux_musl=/nix/store/…/bin/musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=+crt-static -C strip=symbols" \
cargo build --release --target x86_64-unknown-linux-musl \
    -p bevy_ensemble_webrtc --no-default-features --features server \
    --bin bevy_ensemble_webrtc_server
```

Outside the `nix-shell`, deliberately. Running the whole build inside one puts musl's libraries on
the default search path, and the *host* build scripts — which must link against glibc — then fail
with undefined `gnu_get_libc_version`. And use the target-scoped
`CARGO_TARGET_…_RUSTFLAGS` rather than plain `RUSTFLAGS`, which would apply `+crt-static` to those
host build scripts too, with the same result.

The binary lands at `target/x86_64-unknown-linux-musl/release/bevy_ensemble_webrtc_server`.

## Configuring it

| Variable | Meaning |
|---|---|
| `SIGNALLING_ADDR` | Where signalling listens. Defaults to `0.0.0.0:9090`. |
| `TURN_PASSWORD` | One password, accepted with any username. **Absent means no relay.** |
| `TURN_USERS` | `user:password` pairs, comma separated. Only when applications need separate passwords. |
| `TURN_PUBLIC_IP` | The address handed to players. Must be the public one. |
| `TURN_REALM` | Hashed into the credential key. Defaults to `bevy_ensemble`. |
| `TURN_PORT` | The relay's listener. Defaults to 3478. |
| `TURN_RELAY_PORTS` | The allocation range. Defaults to `49160-49260`. |

## One relay, several games

Set `TURN_PASSWORD` and the relay needs to be told about none of them.

TURN derives its key from `MD5(username:realm:password)`, and the username arrives *in the
request* — so the server computes the key from whatever name the client presented. It never has to
have been configured with that name. A new game points at the relay, picks a username, ships, and
works. Nothing restarts and nothing here changes.

The username still reaches the server and still appears in logs. It is a label, not a credential.
What authenticates is the password.

```sh
TURN_PASSWORD=2f9c…          # every game, one secret
```

### When you want per-application passwords instead

`TURN_USERS` trades that convenience for blast radius. With separate passwords, rotating or
revoking one application leaves the others connected, where one shared password disconnects
everything at once:

```sh
TURN_USERS=first-game:2f9c…,second-game:8a10…,third-game:4b77…
```

The cost is a server change for every new application, and a username that must now match its
entry exactly — a mismatch and an unknown application fail identically, as a 401, which a player
experiences as a join that never completes. `TURN_USERS` wins if both are set; asking for strict
per-application passwords and a catch-all at once is a contradiction.

Startup says which mode is live:

```
INFO relay accepts: any username, on one shared password
INFO relay accepts: first-game, second-game
```

Neither mode is about secrecy. A wasm client bakes its configuration in at compile time, so any
password it presents is readable by anybody who opens the bundle. The bounded relay port range is
what caps the damage.

A password containing a comma cannot be expressed in `TURN_USERS`. Hex secrets — what
`openssl rand -hex 32` produces — never contain one.

## Ports

Both ranges, in the host firewall **and** any cloud firewall in front of it:

```sh
ufw allow 3478/udp
ufw allow 49160:49260/udp
```

The allocation range is bounded so that rule stays narrow, and it doubles as the only quota the
relay has: a ceiling on concurrent allocations regardless of who is asking.

No certificate is needed. `turn:` over UDP authenticates with STUN message integrity, and WebRTC
encrypts what it carries regardless. `turns:` on 443 — the transport that gets through networks
blocking UDP outright — is not implemented here; that is the case for coturn alongside, if it ever
comes up.

## Checking it works

Startup must log **two** lines, not one:

```
INFO signalling server listening on ws://0.0.0.0:9090/ws
INFO relay listening on turn:203.0.113.10:3478 (udp), allocating in 49160-49260
```

A missing second line is always explained: `no relay: TURN_PASSWORD is unset`, or
`relay disabled: <reason>`. A relay that will not start never takes signalling down with it.

Then measure it from somewhere else — not from the server, which would measure loopback.
`examples/relay_probe.rs` in this crate allocates for real and pushes game-shaped traffic through
the relay, reporting latency, jitter and loss:

```sh
cargo run --example relay_probe -p bevy_ensemble_webrtc --no-default-features --features server -- \
    --target ours --url turn:relay.example.com:3478 --user first-game --pass … \
    --preset loopback --budget 50 --cliff 190
```

The `loopback` preset runs an in-process relay alongside, so the floor and the real thing appear in
one comparison table and a surprising number can be pinned on the network rather than the tool.
