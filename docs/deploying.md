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
| `TURN_USERS` | `user:password` pairs, comma separated. **Absent means no relay.** |
| `TURN_USER` / `TURN_PASSWORD` | Shorthand for a single pair. Merged with `TURN_USERS` if both are set. |
| `TURN_PUBLIC_IP` | The address handed to players. Must be the public one. |
| `TURN_REALM` | Hashed into the credential key. Defaults to `bevy_ensemble`. |
| `TURN_PORT` | The relay's listener. Defaults to 3478. |
| `TURN_RELAY_PORTS` | The allocation range. Defaults to `49160-49260`. |

## One relay, several games

TURN derives its key from `MD5(username:realm:password)`, so the username is not decoration — the
server has to know which password goes with the name a client presents. Give each application its
own entry:

```sh
TURN_USERS=run2d:2f9c…,bevy_kart:8a10…,bevy_clash:4b77…
```

That is what keeps them independent. Rotating or revoking one leaves the others connected, where a
single shared pair would take every application down at once. Each password is public anyway — a
wasm client bakes its configuration in at compile time and anybody can read the bundle — so what
per-application credentials buy is blast radius, not secrecy.

The username each client presents has to match its entry. A mismatch and an unknown application
fail identically, as a 401, which a player experiences as a join that never completes and cannot
tell apart from having no relay at all. That is why startup logs the names it will accept:

```
INFO relay accepts: bevy_kart, run2d
```

A password containing a comma cannot be expressed in `TURN_USERS`. Hex secrets — what
`openssl rand -hex 32` produces — never contain one.

`TURN_PUBLIC_IP` is explicit rather than detected because it is *advertised* in an allocation
rather than bound. On a host behind NAT the interface address is private, and a relay advertising
`10.x.x.x` hands every player somewhere unreachable while looking perfectly healthy in its own log.

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
`run-2d`'s `examples/relay_probe.rs` allocates for real and pushes game-shaped traffic through the
relay, reporting latency, jitter and loss.
