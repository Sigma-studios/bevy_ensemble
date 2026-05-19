# bevy_ensemble_webrtc

## Server

Run locally:

```bash
cargo server
```

Build static binary for deployment:

```bash
cargo server-build
```

Binary will be at `target/x86_64-unknown-linux-musl/release/bevy_ensemble_webrtc_server`.

## CLI Tools

List active lobbies on the signaling server:

```bash
cargo lobbies
```

Against a remote server:

```bash
SERVER_URL="wss://signal.sigma-dev.eu/ws" cargo lobbies
```

## Example

Desktop:

```bash
cargo run -p bevy_ensemble_webrtc --example minimal_lobby
```

Desktop (remote server):

```bash
SIGNALLING_SERVER_URL="wss://signal.sigma-dev.eu/ws" cargo run -p bevy_ensemble_webrtc --example minimal_lobby
```

Browser:

```bash
SIGNALLING_SERVER_URL="wss://signal.sigma-dev.eu/ws" bevy run -p bevy_ensemble_webrtc --example minimal_lobby web
```
SIGNALLING_SERVER_URL="wss://signal.sigma-dev.eu/ws" cargo run -p bevy_ensemble_webrtc --example circle_sync