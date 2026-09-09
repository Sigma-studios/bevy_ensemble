//! The signalling server, and the relay that goes with it.
//!
//! This is the deployable half of `bevy_ensemble_webrtc`: the thing a game's peers find each other
//! through, and — where it is configured — the thing their traffic falls back to when they cannot
//! reach each other directly. One process, two listeners.
//!
//! ```text
//! cargo build --release -p bevy_ensemble_webrtc --features server \
//!     --bin bevy_ensemble_webrtc_server
//! ```
//!
//! # Configuration
//!
//! | Variable | Meaning |
//! |---|---|
//! | `SIGNALLING_ADDR` | Where signalling listens. Defaults to `0.0.0.0:9090`. |
//! | `TURN_PASSWORD` | The credential clients present. **Absent means no relay.** |
//! | `TURN_PUBLIC_IP` | The address handed to players. Must be the public one; see below. |
//! | `TURN_USER` | The username clients present. Defaults to `ensemble`. |
//! | `TURN_REALM` | Hashed into the credential key. Defaults to `bevy_ensemble`. |
//! | `TURN_PORT` | The relay's listener. Defaults to 3478. |
//! | `TURN_RELAY_PORTS` | The range allocations are drawn from. Defaults to `49160-49260`. |
//!
//! A deployment with no `TURN_PASSWORD` is signalling only, which is what this has always been.
//! Peers who can pair directly are unaffected; peers who cannot simply have no route, and see a
//! join that never completes.
//!
//! Both port ranges have to be open, in the host firewall **and** any cloud firewall in front of
//! it. The allocation range is bounded so that rule stays narrow — and it doubles as the only
//! quota the relay has, since it caps concurrent allocations regardless of who is asking.
//!
//! `TURN_PUBLIC_IP` is explicit rather than detected on purpose. It is *advertised* in an
//! allocation, not bound, so on a host behind NAT the interface address is the wrong one — and a
//! relay that advertises `10.x.x.x` hands every player somewhere unreachable while looking
//! perfectly healthy in its own logs. Detection would guess wrong silently; a variable cannot.
//!
//! Nothing here needs a certificate. `turn:` over UDP authenticates with STUN message integrity,
//! and WebRTC encrypts what it carries regardless.

use std::net::IpAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use tracing::{info, warn};

use bevy_ensemble_webrtc::server::{
    DEFAULT_RELAY_PORTS, Relay, RelayConfig, RelayCredentials, ServerState, handle_socket,
    start_relay,
};

/// Where signalling listens when `SIGNALLING_ADDR` does not say otherwise.
const DEFAULT_ADDR: &str = "0.0.0.0:9090";

/// The username clients present when `TURN_USER` does not say otherwise.
const DEFAULT_TURN_USER: &str = "ensemble";

/// The realm the relay authenticates under when `TURN_REALM` does not say otherwise.
///
/// Any stable string does — it is hashed into the credential key, and clients take it from the
/// challenge rather than being configured with it. Changing it invalidates nothing on the client
/// side; it only has to stay consistent within a deployment.
const DEFAULT_TURN_REALM: &str = "bevy_ensemble";

/// `MIN-MAX`, or the default range.
fn relay_ports() -> Result<(u16, u16), String> {
    let Ok(raw) = std::env::var("TURN_RELAY_PORTS") else {
        return Ok(DEFAULT_RELAY_PORTS);
    };
    let (min, max) = raw
        .split_once('-')
        .ok_or(format!("TURN_RELAY_PORTS wants MIN-MAX, got {raw}"))?;
    let port = |s: &str| {
        s.trim()
            .parse::<u16>()
            .map_err(|_| format!("TURN_RELAY_PORTS: {s} is not a port"))
    };
    Ok((port(min)?, port(max)?))
}

/// The relay's configuration from the environment, or `None` when this deployment has no relay.
fn relay_config() -> Result<Option<RelayConfig>, String> {
    let Ok(password) = std::env::var("TURN_PASSWORD") else {
        return Ok(None);
    };
    if password.is_empty() {
        return Err("TURN_PASSWORD is set but empty".into());
    }
    let public_ip: IpAddr = std::env::var("TURN_PUBLIC_IP")
        .map_err(|_| "TURN_PASSWORD is set, so TURN_PUBLIC_IP must be too".to_string())?
        .parse()
        .map_err(|_| "TURN_PUBLIC_IP is not an IP address".to_string())?;

    let username = std::env::var("TURN_USER").unwrap_or_else(|_| DEFAULT_TURN_USER.to_owned());
    let mut config = RelayConfig::new(
        public_ip,
        RelayCredentials::Static { username, password },
    );
    config.realm = std::env::var("TURN_REALM").unwrap_or_else(|_| DEFAULT_TURN_REALM.to_owned());
    config.relay_ports = relay_ports()?;
    if let Ok(raw) = std::env::var("TURN_PORT") {
        config.listen_port = raw
            .parse()
            .map_err(|_| format!("TURN_PORT is not a port: {raw}"))?;
    }
    Ok(Some(config))
}

/// Start the relay this deployment is configured for, saying what happened either way.
///
/// Never fatal. Signalling without a relay is a working deployment for everybody who can pair
/// directly, and refusing to start would take those sessions down over a feature that is purely
/// additive.
async fn relay() -> Option<Relay> {
    let config = match relay_config() {
        Ok(Some(config)) => config,
        Ok(None) => {
            info!("no relay: TURN_PASSWORD is unset, so peers that cannot pair directly cannot play");
            return None;
        }
        Err(error) => {
            warn!("relay disabled: {error}");
            return None;
        }
    };

    let (ip, port) = (config.public_ip, config.listen_port);
    let (min, max) = config.relay_ports;
    match start_relay(config).await {
        Ok(relay) => {
            info!("relay listening on turn:{ip}:{port} (udp), allocating in {min}-{max}");
            info!("  both must be open: `ufw allow {port}/udp` and `ufw allow {min}:{max}/udp`");
            Some(relay)
        }
        Err(error) => {
            warn!("relay disabled: {error}");
            None
        }
    }
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServerState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let state = Arc::new(ServerState::new());

    let app = Router::new().route("/ws", get(ws_upgrade)).with_state(state);

    let addr = std::env::var("SIGNALLING_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_owned());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|error| panic!("could not bind {addr}: {error}"));

    info!("signalling server listening on ws://{addr}/ws");

    // Held for the lifetime of the process: dropping it stops the relay.
    let _relay = relay().await;

    axum::serve(listener, app).await.expect("Server failed");
}
