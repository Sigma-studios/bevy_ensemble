//! The signalling server, and the relay that goes with it.
//!
//! This is the deployable half of `bevy_ensemble_webrtc`: the thing a game's peers find each other
//! through, and — where it is configured — the thing their traffic falls back to when they cannot
//! reach each other directly. One process, two listeners.
//!
//! ```text
//! cargo build --release --target x86_64-unknown-linux-musl \
//!     -p bevy_ensemble_webrtc --no-default-features --features server \
//!     --bin bevy_ensemble_webrtc_server
//! ```
//!
//! `--no-default-features` matters: the crate defaults to `client`, which pulls `bevy` and with it
//! a Vulkan loader this will never call -- 74 MB against 2.6 MB. The musl target matters too, for
//! anything that will be copied to a server rather than built on it. See `docs/deploying.md`.
//!
//! # Configuration
//!
//! | Variable | Meaning |
//! |---|---|
//! | `SIGNALLING_ADDR` | Where signalling listens. Defaults to `0.0.0.0:9090`. |
//! | `TURN_USERS` | `user:password` pairs, comma separated — one per application sharing this relay. |
//! | `TURN_USER` / `TURN_PASSWORD` | Shorthand for a single pair. Merged with `TURN_USERS` if both are set. |
//! | `TURN_PUBLIC_IP` | The address handed to players. Must be the public one; see below. |
//! | `TURN_REALM` | Hashed into the credential key. Defaults to `bevy_ensemble`. |
//! | `TURN_PORT` | The relay's listener. Defaults to 3478. |
//! | `TURN_RELAY_PORTS` | The range allocations are drawn from. Defaults to `49160-49260`. |
//!
//! A deployment with no credentials at all is signalling only, which is what this has always been.
//! Peers who can pair directly are unaffected; peers who cannot simply have no route, and see a
//! join that never completes.
//!
//! One relay commonly serves several games. TURN derives its key from
//! `MD5(username:realm:password)`, so the username is not decoration — the server has to know
//! which password goes with the name a client presents. Giving each application its own entry is
//! what keeps them independent: rotating or revoking one leaves the others connected, where a
//! single shared pair would take every application down at once.
//!
//! ```text
//! TURN_USERS=run2d:2f9c…,bevy_kart:8a10…,bevy_clash:4b77…
//! ```
//!
//! A password containing a comma cannot be expressed this way. Hex secrets, which is what
//! `openssl rand -hex 32` produces, never contain one.
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

use std::collections::HashMap;
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

/// Every credential this relay accepts, from `TURN_USERS` and the single-pair shorthand.
fn users() -> Result<HashMap<String, String>, String> {
    let mut users = HashMap::new();

    if let Ok(raw) = std::env::var("TURN_USERS") {
        for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (user, password) = entry
                .split_once(':')
                .ok_or(format!("TURN_USERS wants `user:password`, got `{entry}`"))?;
            if user.is_empty() || password.is_empty() {
                return Err(format!("TURN_USERS entry `{entry}` has an empty half"));
            }
            users.insert(user.to_owned(), password.to_owned());
        }
    }

    match (std::env::var("TURN_USER"), std::env::var("TURN_PASSWORD")) {
        (Ok(user), Ok(password)) if !user.is_empty() && !password.is_empty() => {
            users.insert(user, password);
        }
        // Half of the shorthand is a mistake worth naming: ignoring it silently would leave one
        // application unable to allocate, and only that application's players would ever see it.
        (Ok(_), Err(_)) => return Err("TURN_USER is set but TURN_PASSWORD is not".into()),
        (Err(_), Ok(_)) => return Err("TURN_PASSWORD is set but TURN_USER is not".into()),
        _ => {}
    }

    Ok(users)
}

/// The relay's configuration from the environment, or `None` when this deployment has no relay.
fn relay_config() -> Result<Option<RelayConfig>, String> {
    let users = users()?;
    if users.is_empty() {
        return Ok(None);
    }
    let public_ip: IpAddr = std::env::var("TURN_PUBLIC_IP")
        .map_err(|_| "relay credentials are set, so TURN_PUBLIC_IP must be too".to_string())?
        .parse()
        .map_err(|_| "TURN_PUBLIC_IP is not an IP address".to_string())?;

    let mut config = RelayConfig::new(public_ip, RelayCredentials::Users(users));
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
            info!(
                "no relay: no credentials configured, so peers that cannot pair directly cannot play"
            );
            return None;
        }
        Err(error) => {
            warn!("relay disabled: {error}");
            return None;
        }
    };

    let (ip, port) = (config.public_ip, config.listen_port);
    let (min, max) = config.relay_ports;
    // Named at startup because an unknown username and a wrong password are the same 401 on the
    // wire, and a player experiences either as a join that never completes.
    if let RelayCredentials::Users(users) = &config.credentials {
        let mut names: Vec<&str> = users.keys().map(String::as_str).collect();
        names.sort_unstable();
        info!("relay accepts: {}", names.join(", "));
    }
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

    let app = Router::new()
        .route("/ws", get(ws_upgrade))
        .with_state(state);

    let addr = std::env::var("SIGNALLING_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_owned());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|error| panic!("could not bind {addr}: {error}"));

    info!("signalling server listening on ws://{addr}/ws");

    // Held for the lifetime of the process: dropping it stops the relay.
    let _relay = relay().await;

    axum::serve(listener, app).await.expect("Server failed");
}
