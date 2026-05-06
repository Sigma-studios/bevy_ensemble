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

## Example

Desktop:

```bash
cargo run -p bevy_ensemble_webrtc --example minimal_lobby --features client
```

Desktop (remote server):

```bash
SIGNALLING_SERVER_URL="wss://signal.sigma-dev.eu/ws" cargo run -p bevy_ensemble_webrtc --example minimal_lobby --features client
```

Browser:

```bash
SIGNALLING_SERVER_URL="wss://signal.sigma-dev.eu/ws" bevy run -p bevy_ensemble_webrtc --example minimal_lobby --features client web
```
