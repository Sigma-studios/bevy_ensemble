//! A TURN relay, for the players who cannot reach each other directly.
//!
//! # Why this lives beside the signalling server
//!
//! Two peers connect directly whenever their networks allow it. When they do not — client
//! isolation on the Wi-Fi, mDNS filtered so a browser's `.local` candidates resolve nowhere, a NAT
//! neither side can traverse — a relay is the only remaining route, and a deployment without one
//! simply cannot serve those players. They see a join that times out, which reads as the game
//! being broken.
//!
//! It belongs in this crate for the same reason [`handle_socket`](super::handle_socket) does: it
//! is infrastructure a deployment runs, not game code. The client half of the same story already
//! lives here — `BevyEnsembleWebrtcPlugin::ice_servers` is what a peer gathers candidates from —
//! and a crate that hands out the question should be able to answer it. Putting the relay in one
//! consumer's example instead would leave every other consumer to rediscover it.
//!
//! # What it does not do
//!
//! Reads no environment and picks no defaults for the public address. Configuration is the
//! binary's job; this takes a [`RelayConfig`] and starts a server. That keeps the awkward
//! deployment questions — which IP is the public one, which ports the firewall opened — where
//! somebody can answer them.
//!
//! # Use
//!
//! ```no_run
//! # use bevy_ensemble_webrtc::server::{RelayConfig, RelayCredentials, start_relay};
//! # async fn f() -> Result<(), Box<dyn std::error::Error>> {
//! // Held for as long as the process should relay: dropping it stops the relay.
//! let _relay = start_relay(RelayConfig::new(
//!     "203.0.113.10".parse()?,
//!     RelayCredentials::Static { username: "run2d".into(), password: "…".into() },
//! ))
//! .await?;
//! # Ok(()) }
//! ```

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use turn::auth::{AuthHandler, LongTermAuthHandler, generate_auth_key};
use turn::relay::relay_range::RelayAddressGeneratorRanges;
use turn::server::Server;
use turn::server::config::{ConnConfig, ServerConfig};
use webrtc_util::vnet::net::Net;

/// A running relay. Dropping it stops relaying.
pub type Relay = Server;

/// The port a relay listens on when nothing says otherwise. 3478 is TURN's registered port.
pub const DEFAULT_LISTEN_PORT: u16 = 3478;

/// The range allocations are drawn from when nothing says otherwise.
///
/// Bounded on purpose. The alternative is an ephemeral port per allocation, which would mean
/// opening 32768-60999 on the firewall to cover it. A hundred ports is one narrow rule, far more
/// than a lobby-sized game produces — and, usefully, a hard ceiling on concurrent allocations,
/// which is the only quota this relay has.
pub const DEFAULT_RELAY_PORTS: (u16, u16) = (49160, 49260);

/// How the relay decides a credential is good.
///
/// TURN has no anonymous mode worth using: a relay that accepts anyone is an open relay, and open
/// relays are found by scanners within hours of coming up. Both variants below are real
/// authentication; they differ in who holds what.
pub enum RelayCredentials {
    /// One fixed pair, shared by every player.
    ///
    /// The simplest thing that works, and the right choice when the client is a wasm bundle whose
    /// configuration is baked in at compile time: there is nowhere to put a rotating credential
    /// that the client could read. The cost is that the pair is public — anybody who opens the
    /// bundle has it — and rotating it means republishing the client.
    ///
    /// The bounded relay port range is what keeps that survivable: it caps concurrent allocations
    /// regardless of who is asking.
    Static { username: String, password: String },

    /// Derived rather than stored: the username is an expiry and the password is an HMAC of it
    /// under this secret.
    ///
    /// The secret never leaves the server. Credentials are minted per session, expire on their
    /// own, and rotating them is a restart rather than a release — but something has to *deliver*
    /// them to each client, which means a channel the client already has. Use this when the
    /// signalling connection carries them.
    Secret(String),
}

/// Everything a relay needs that this crate cannot reasonably guess.
pub struct RelayConfig {
    /// The address handed to players in an allocation.
    ///
    /// **Must be the public one.** It is advertised, not bound, and on a host behind NAT the
    /// interface address is private — a relay advertising `10.x.x.x` hands every player somewhere
    /// unreachable while looking perfectly healthy in its own logs.
    pub public_ip: IpAddr,
    /// Where the relay listens for allocation requests.
    pub listen_port: u16,
    /// Inclusive range allocations are drawn from. Both ends must be open on the firewall.
    pub relay_ports: (u16, u16),
    /// Hashed into the credential key. Any stable string does; changing it invalidates every
    /// credential minted under the old one.
    pub realm: String,
    pub credentials: RelayCredentials,
}

impl RelayConfig {
    /// The two things with no sensible default, with defaults for everything else.
    pub fn new(public_ip: IpAddr, credentials: RelayCredentials) -> Self {
        Self {
            public_ip,
            listen_port: DEFAULT_LISTEN_PORT,
            relay_ports: DEFAULT_RELAY_PORTS,
            realm: "bevy_ensemble".to_owned(),
            credentials,
        }
    }
}

/// Why a relay would not start.
#[derive(Debug)]
pub enum RelayError {
    /// The listener could not be bound — the port is taken, or in use by another process.
    Bind { port: u16, source: std::io::Error },
    /// The configuration cannot describe a working relay.
    Config(String),
    /// The TURN server itself refused the configuration.
    Turn(turn::Error),
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind { port, source } => {
                write!(f, "could not bind udp/{port} for the relay: {source}")
            }
            Self::Config(why) => write!(f, "{why}"),
            Self::Turn(error) => write!(f, "the relay would not start: {error}"),
        }
    }
}

impl std::error::Error for RelayError {}

/// Accepts one fixed credential.
struct StaticAuth {
    username: String,
    password: String,
}

impl AuthHandler for StaticAuth {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        _src_addr: std::net::SocketAddr,
    ) -> Result<Vec<u8>, turn::Error> {
        if username != self.username {
            return Err(turn::Error::ErrNoSuchUser);
        }
        Ok(generate_auth_key(username, realm, &self.password))
    }
}

/// Start a relay, which runs until the returned value is dropped.
pub async fn start_relay(config: RelayConfig) -> Result<Relay, RelayError> {
    let (min_port, max_port) = config.relay_ports;
    if min_port == 0 || max_port < min_port {
        return Err(RelayError::Config(format!(
            "relay port range {min_port}-{max_port} is not a range"
        )));
    }
    if !config.public_ip.is_ipv4() {
        return Err(RelayError::Config(
            "public_ip must be IPv4: an allocation advertises a single address".into(),
        ));
    }

    let conn = Arc::new(
        tokio::net::UdpSocket::bind(format!("0.0.0.0:{}", config.listen_port))
            .await
            .map_err(|source| RelayError::Bind {
                port: config.listen_port,
                source,
            })?,
    );

    let auth_handler: Arc<dyn AuthHandler + Send + Sync> = match config.credentials {
        RelayCredentials::Static { username, password } => {
            Arc::new(StaticAuth { username, password })
        }
        RelayCredentials::Secret(secret) => Arc::new(LongTermAuthHandler::new(secret)),
    };

    Server::new(ServerConfig {
        conn_configs: vec![ConnConfig {
            conn,
            relay_addr_generator: Box::new(RelayAddressGeneratorRanges {
                // Advertised and bound are different addresses on purpose — see `public_ip`.
                relay_address: config.public_ip,
                address: "0.0.0.0".to_owned(),
                min_port,
                max_port,
                max_retries: 10,
                net: Arc::new(Net::new(None)),
            }),
        }],
        realm: config.realm,
        auth_handler,
        channel_bind_timeout: Duration::from_secs(0),
        alloc_close_notify: None,
    })
    .await
    .map_err(RelayError::Turn)
}
