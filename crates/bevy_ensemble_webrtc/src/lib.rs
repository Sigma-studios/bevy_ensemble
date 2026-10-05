pub mod protocol;

#[cfg(feature = "server")]
pub mod server;

/// The `axum` the signalling server is written against.
///
/// Use it — `bevy_ensemble_webrtc::axum::extract::ws::WebSocket` — rather than depending on
/// `axum` yourself. [`server::handle_socket`] takes *this* crate's `WebSocket`, and two axum
/// versions in one graph are two unrelated types, so a host binary that pins its own copy gets a
/// mismatch at the one call it exists to make. Going through this re-export means the version is
/// this crate's business, which is where it belongs.
#[cfg(feature = "server")]
pub use axum;

#[cfg(feature = "client")]
mod connection;
#[cfg(feature = "client")]
mod handshake;
#[cfg(feature = "client")]
mod join_first;
#[cfg(feature = "client")]
mod session;
#[cfg(feature = "client")]
mod systems;

#[cfg(feature = "client")]
use bevy::prelude::*;
#[cfg(feature = "client")]
pub use bevy_ensemble::PeerRtt;
#[cfg(feature = "client")]
use bevy_ensemble::{EnsembleAppExt, EnsembleTransportAppExt, MessageAuthority};
#[cfg(feature = "client")]
pub use bevy_ensemble_sockets::{IceServer, IceServers};
#[cfg(feature = "client")]
pub use join_first::JoinFirstLobby;

/// Bevy plugin for WebRTC P2P networking via a signaling server.
///
/// Connects to a signaling server for lobby management, then uses
/// bevy_ensemble_sockets for cross-platform WebRTC data channels
/// (works on both native and WASM).
#[cfg(feature = "client")]
pub struct BevyEnsembleWebrtcPlugin {
    pub server_url: String,
    /// The name this peer is *first* advertised under.
    ///
    /// Only the starting value: it is inserted as [`SignallingDisplayName`], and changing that
    /// resource re-sends it. A menu that lets a player type their name wants the resource, not
    /// this field.
    pub display_name: String,
    pub max_players: u32,
    /// The ICE servers peer connections gather candidates from.
    ///
    /// Defaults to the public STUN servers this crate has always hardcoded. Two peers on the same
    /// machine need none of them and wait on them anyway, so a local run or a test can pass
    /// [`IceServers::none()`] and skip straight to host candidates.
    pub ice_servers: IceServers,
    /// How long a lobby may stay pending before the attempt is given up on. `None` waits for ever.
    ///
    /// A backstop, not the main mechanism: a connection that is actually attempted and fails
    /// reports `Failed` and is handled the moment it does. This covers the attempts that report
    /// nothing — an offer that never arrives, a signalling server that accepts a join and goes
    /// quiet — where the alternative is a peer that waits for the rest of the session.
    ///
    /// Fifteen seconds because it has to sit above a slow-but-working join and below a person's
    /// patience. Both peers gathering, exchanging and pairing takes a second or two on a healthy
    /// network and a few more on a bad one; nothing legitimate takes fifteen.
    pub join_timeout: Option<std::time::Duration>,
    /// Which game this is, so the lobby list holds this game's lobbies and no other's.
    ///
    /// Any string that is the same in every build of one game and different from every other
    /// game on the same signalling server; the crate name is the obvious one. Empty declares
    /// nothing, and sees only the lobbies of other clients that declared nothing — which is what
    /// every build from before the declaration existed does. See
    /// [`ClientMessage::DeclareGame`](protocol::ClientMessage::DeclareGame).
    pub game: String,
}

#[cfg(feature = "client")]
impl Default for BevyEnsembleWebrtcPlugin {
    fn default() -> Self {
        Self {
            server_url: "ws://localhost:9090/ws".into(),
            display_name: "Player".into(),
            max_players: 8,
            ice_servers: IceServers::default(),
            join_timeout: Some(std::time::Duration::from_secs(15)),
            game: String::new(),
        }
    }
}

/// Whether a signalling URL points at this machine.
///
/// Used to decide that no STUN and no relay are wanted: two peers reached through a loopback
/// signalling server are on one machine, pair on host candidates, and would otherwise wait on
/// servers whose answers cannot help them.
#[cfg(feature = "client")]
fn is_loopback_signalling(url: &str) -> bool {
    let authority = url
        .split("//")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("");
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host);
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

#[cfg(all(test, feature = "client"))]
mod ice_server_tests {
    use super::is_loopback_signalling;

    /// Every shape a local signalling server is written as, and some that are not one.
    ///
    /// `192.168.1.20` is the case worth keeping honest: a LAN address is *not* loopback. Two peers
    /// on one network still gather host candidates that pair directly, but a third on a phone does
    /// not, and treating a LAN address as local would silently deny them STUN and the relay.
    #[test]
    fn loopback_signalling_is_recognised() {
        assert!(is_loopback_signalling("ws://127.0.0.1:9090/ws"));
        assert!(is_loopback_signalling("ws://localhost:9090/ws"));
        assert!(is_loopback_signalling("ws://[::1]:9090/ws"));
        assert!(is_loopback_signalling("127.0.0.1:9090"));
        assert!(!is_loopback_signalling("wss://signal.sigma-dev.eu/ws"));
        assert!(!is_loopback_signalling("ws://192.168.1.20:9090/ws"));
    }
}

/// The ICE servers a client should gather candidates from, given how it is configured.
///
/// Prefer the [`ice_servers_from_env!`] macro, which fills the relay arguments in from the
/// environment. This is the same logic with the values passed explicitly, for a consumer that
/// gets them from somewhere else.
///
/// # What it decides
///
/// | Signalling | Relay configured | Result |
/// |---|---|---|
/// | loopback | either | [`IceServers::none()`] — peers on one machine pair on host candidates |
/// | remote | no | the public STUN pair |
/// | remote | yes | the public STUN pair, plus the relay |
///
/// The loopback case is not an optimisation detail: gathering waits on servers that cannot help,
/// which makes every local run slower and every local test noisier.
///
/// A relay matters because STUN alone is not always enough. It tells a peer its public address,
/// which is sufficient whenever the two networks will carry a direct connection — and when they
/// will not, because of client isolation, filtered mDNS, or a NAT neither side can traverse,
/// there is no third option and the join simply fails. That failure is per-device, which is why
/// it shows up as "some of us could not join and the rest could".
///
/// All three relay parts are required together. Two of them is a relay that silently is not
/// there, so it warns rather than half-configuring itself.
#[cfg(feature = "client")]
pub fn ice_servers_for(
    signalling_url: &str,
    turn_url: Option<&str>,
    turn_username: Option<&str>,
    turn_credential: Option<&str>,
) -> IceServers {
    if is_loopback_signalling(signalling_url) {
        return IceServers::none();
    }

    /// The runtime environment wins over whatever was baked in at compile time, so a native build
    /// can be pointed at another relay without being rebuilt. A wasm build has no environment to
    /// read, which is why the baked value has to exist at all.
    fn resolve(name: &str, baked: Option<&str>) -> Option<String> {
        std::env::var(name)
            .ok()
            .or_else(|| baked.map(String::from))
            .filter(|value| !value.is_empty())
    }

    let mut servers = IceServers::default().0;
    match (
        resolve("TURN_URL", turn_url),
        resolve("TURN_USER", turn_username),
        resolve("TURN_PASSWORD", turn_credential),
    ) {
        (Some(urls), Some(username), Some(credential)) => {
            info!("relay configured: {urls}");
            servers.push(IceServer {
                urls: vec![urls],
                username,
                credential,
            });
        }
        (None, None, None) => {}
        _ => warn!(
            "no relay: TURN_URL, TURN_USER and TURN_PASSWORD must all be set, and some are not"
        ),
    }
    IceServers(servers)
}

/// The ICE servers for this build, reading the relay from the environment.
///
/// ```rust,ignore
/// app.add_plugins(BevyEnsembleWebrtcPlugin {
///     server_url: signalling_url.clone(),
///     ice_servers: bevy_ensemble_webrtc::ice_servers_from_env!(&signalling_url),
///     ..default()
/// });
/// ```
///
/// `TURN_URL`, `TURN_USER` and `TURN_PASSWORD` are read from the environment at runtime, falling
/// back to whatever they were at **this call site's** compile time. A macro rather than a
/// function because that is the only way `option_env!` can see the consumer's build: expanded
/// inside this crate it would capture whatever was set when *this* crate was compiled, which for
/// a cached dependency is a different build entirely, and silently stale.
#[cfg(feature = "client")]
#[macro_export]
macro_rules! ice_servers_from_env {
    ($signalling_url:expr) => {
        $crate::ice_servers_for(
            $signalling_url,
            ::core::option_env!("TURN_URL"),
            ::core::option_env!("TURN_USER"),
            ::core::option_env!("TURN_PASSWORD"),
        )
    };
}

/// Log directives that silence ICE gathering noise, for a consumer's `LogPlugin` filter.
///
/// # Why this is a string and not a fix
///
/// A WebRTC connection emits roughly a dozen `WARN`s that mean nothing. Measured over one
/// two-peer session — 15 warning lines in total, of which **14 come from `webrtc_ice` and none
/// from this crate**:
///
/// | lines | message | cause |
/// |---|---|---|
/// | 8 | `could not listen udp fe80::…: Can't assign requested address` | link-local IPv6 addresses it enumerates and cannot bind |
/// | 4 | `pingAllCandidates called with no candidate pairs` | gathering, before any pair exists |
/// | 2 | `failed to resolve stun host: stun.l.google.com` | no IPv6 route to the public STUN servers |
///
/// They are `log::warn!` calls inside `webrtc-ice`, so nothing in `bevy_ensemble` can downgrade
/// them at the source — which is what makes "zero warnings from a clean run" unusable as a pass
/// bar, and that is the cheapest netcode check there is. What this crate *can* do is stop every
/// consumer rediscovering the target names by grepping a log.
///
/// The last two rows go away on their own if you pass [`IceServers::none()`](IceServers::none),
/// which is right for loopback and a LAN. The other twelve do not.
///
/// ```rust,ignore
/// DefaultPlugins.set(LogPlugin {
///     filter: format!("{},{}", default_filter, bevy_ensemble_webrtc::QUIET_ICE_LOG_FILTER),
///     ..default()
/// })
/// ```
#[cfg(feature = "client")]
pub const QUIET_ICE_LOG_FILTER: &str = "webrtc_ice::agent::agent_gather=error,\
     webrtc_ice::agent::agent_internal=error,\
     webrtc_ice::mdns=error";

/// A connection settled on the relay, and this is why: every candidate both sides offered, every
/// pair ICE checked and how it went, with a verdict on top. Plain text, one per relayed
/// connection, also logged as a warning. A game can keep it somewhere a player can find it and
/// send it in; the host's copy covers every client's connection.
#[cfg(feature = "client")]
#[derive(Message, Clone, Debug)]
pub struct RelayReport {
    /// The peer the relayed connection is to.
    pub peer: u128,
    pub report: String,
}

/// The name this peer is advertised under in the lobby list, live.
///
/// Insert or change it and the new name is sent to the signalling server, which updates the
/// listing of any lobby this peer is hosting. Before this existed the name was fixed when the
/// socket was built — process start, for most consumers — so what a lobby was listed as had
/// nothing to do with what the player had called themselves.
///
/// Distinct from `PlayerData`, which is how a name reaches the *other players* in a session and
/// travels over the data channel. This one is only the signalling server's listing.
#[cfg(feature = "client")]
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct SignallingDisplayName(pub String);

/// Marks a lobby entity with its server-assigned lobby ID.
#[cfg(feature = "client")]
#[derive(Component)]
pub struct LobbyWebrtcId(pub u64);

/// Marks a lobby client entity with the remote peer's player UUID.
#[cfg(feature = "client")]
#[derive(Component)]
pub struct LobbyClientWebrtcUuid(pub u128);

/// On a client's lobby entity, the uuid of the peer hosting it, as the signalling server said.
///
/// The same fact as the [`HostUuid`](bevy_ensemble::HostUuid) resource, kept on the entity so the
/// systems that answer "is this packet, offer or disconnect from my host?" can read it next to the
/// lobby it belongs to. Inserted from `LobbyJoined`, before any data channel to the host exists.
#[cfg(feature = "client")]
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LobbyHostUuid(pub u128);

/// Temporary marker for lobby client entities awaiting handshake completion.
#[cfg(feature = "client")]
#[derive(Component)]
pub struct PendingWebrtcLobbyClient;

/// Write this message to request a lobby list refresh from the signaling server.
#[cfg(feature = "client")]
#[derive(Message, Clone, Copy, Debug)]
pub struct RefreshLobbyList;

/// Write this message to join a lobby by its server-assigned ID.
#[cfg(feature = "client")]
#[derive(Message, Clone, Copy, Debug)]
pub struct JoinWebrtcLobby(pub u64);

/// Write this message to join a lobby by its 4-letter code.
///
/// The code is normalised with [`normalize_lobby_code`](protocol::normalize_lobby_code) when this
/// is handled — whitespace dropped, upper-cased — so a menu, a link and a test can all write what
/// they were given.
#[cfg(feature = "client")]
#[derive(Message, Clone, Debug)]
pub struct JoinWebrtcLobbyByCode(pub String);

/// The lobby's short join code, as the signalling server assigned it.
///
/// On the lobby entity for the host and, from the moment the server confirms the join, for every
/// joiner too — however they came in: by code, by id from the listing, or through a link. It
/// follows the lobby through a host migration.
#[cfg(feature = "client")]
#[derive(Component)]
pub struct LobbyWebrtcCode(pub String);

/// The signalling server refused something this peer sent, and why.
///
/// Written for every refusal, which is more than the failures a player needs to hear about: a
/// join or a host the server turned down also ends with [`LobbyJoinFailed`] — the reason in
/// words, the pending lobby gone — while a request refused only for its rate is sent again on its
/// own and ends nothing. This is for a game that wants to tell those apart, or count them.
///
/// [`LobbyJoinFailed`]: bevy_ensemble::LobbyJoinFailed
#[cfg(feature = "client")]
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignallingRefused {
    pub error: protocol::SignallingError,
}

/// On a pending lobby, the request that is to become it, until the server's answer arrives.
///
/// What ties an answer to the lobby waiting for it, and what a pending lobby that is despawned
/// before the answer takes back with it: see [`systems::cancel_abandoned_request`].
#[cfg(feature = "client")]
#[derive(Component, Clone, Copy, Debug)]
pub(crate) struct LobbyRequest(pub protocol::RequestId);

/// Join the lobby whose code is in the page's URL, once, at startup.
///
/// `https://…/index.html?room=ABCD` drops a player straight into a friend's lobby: "join my game"
/// is a link, not a code read aloud. The value goes through [`JoinWebrtcLobbyByCode`], so it is
/// normalised like any other code, and a missing or empty parameter does nothing.
///
/// On the web only. Off it there is no page, and this does nothing at all; a native build that
/// wants the same from its command line or environment writes [`JoinWebrtcLobbyByCode`] itself.
///
/// ```rust,ignore
/// app.add_plugins(JoinFromUrlPlugin::default()); // `?room=CODE`
/// ```
#[cfg(feature = "client")]
pub struct JoinFromUrlPlugin {
    /// The query parameter the code is read from. `room` by default.
    pub param: &'static str,
}

#[cfg(feature = "client")]
impl Default for JoinFromUrlPlugin {
    fn default() -> Self {
        Self { param: "room" }
    }
}

#[cfg(feature = "client")]
impl Plugin for JoinFromUrlPlugin {
    fn build(&self, app: &mut App) {
        let param = self.param;
        app.add_systems(
            Startup,
            move |mut join: MessageWriter<JoinWebrtcLobbyByCode>| {
                let Some(code) = page_query_param(param) else {
                    return;
                };
                let code = protocol::normalize_lobby_code(&code);
                if code.is_empty() {
                    return;
                }
                info!("joining lobby {code} from the page URL");
                join.write(JoinWebrtcLobbyByCode(code));
            },
        );
    }
}

/// The value of the query parameter `name` in the page's URL, as written there. `None` when it is
/// absent, and always off the web.
///
/// The fragment is ignored, `?a=1&room=ABCD#top` gives `ABCD` for `room`, and a parameter
/// present with no `=` is `Some("")`. Nothing is percent-decoded: this is for short codes and
/// flags, not for arbitrary text.
#[cfg(feature = "client")]
pub fn page_query_param(name: &str) -> Option<String> {
    query_param_of(&page_url()?, name)
}

#[cfg(all(feature = "client", target_arch = "wasm32"))]
fn page_url() -> Option<String> {
    web_sys::window()?.location().href().ok()
}

#[cfg(all(feature = "client", not(target_arch = "wasm32")))]
fn page_url() -> Option<String> {
    None
}

#[cfg(feature = "client")]
fn query_param_of(url: &str, name: &str) -> Option<String> {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let (_, query) = without_fragment.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then(|| value.to_owned())
    })
}

#[cfg(all(test, feature = "client"))]
mod query_param_tests {
    use super::query_param_of;

    #[test]
    fn a_query_parameter_is_read_from_a_page_url() {
        let url = "https://example.com/game/index.html?name=x&room=abcd#top";
        assert_eq!(query_param_of(url, "room").as_deref(), Some("abcd"));
        assert_eq!(query_param_of(url, "name").as_deref(), Some("x"));
        assert_eq!(
            query_param_of(url, "top"),
            None,
            "the fragment is not the query"
        );
        assert_eq!(
            query_param_of("https://x/?room", "room").as_deref(),
            Some("")
        );
        assert_eq!(query_param_of("https://x/#?room=ABCD", "room"), None);
        assert_eq!(query_param_of("https://x/", "room"), None);
    }
}

/// App extensions for the signalling side of this crate.
#[cfg(feature = "client")]
pub trait SignallingAppExt {
    /// Keep [`SignallingDisplayName`] equal to a name the game already keeps somewhere else.
    ///
    /// A game has one name for its player — typed in a menu, saved in a profile — and the
    /// signalling server's listing is a second place it has to reach. `name` reads it out of the
    /// resource `R` whenever `R` changes, and the listing follows; a resource that does not
    /// exist yet is waited for.
    ///
    /// ```rust,ignore
    /// app.sync_listing_name_from(|profile: &LocalPlayerData<Profile>| profile.0.name.clone());
    /// ```
    ///
    /// Requires [`BevyEnsembleWebrtcPlugin`].
    fn sync_listing_name_from<R: Resource>(
        &mut self,
        name: impl Fn(&R) -> String + Send + Sync + 'static,
    ) -> &mut Self;
}

#[cfg(feature = "client")]
impl SignallingAppExt for App {
    fn sync_listing_name_from<R: Resource>(
        &mut self,
        name: impl Fn(&R) -> String + Send + Sync + 'static,
    ) -> &mut Self {
        self.add_systems(
            Update,
            (move |source: Option<Res<R>>, listing: Option<ResMut<SignallingDisplayName>>| {
                let (Some(source), Some(mut listing)) = (source, listing) else {
                    return;
                };
                // Compared before it is written, so an unchanged name is not a change the
                // publisher would read as something to re-send.
                if !source.is_changed() && !listing.is_added() {
                    return;
                }
                let name = name(&source);
                if listing.0 != name {
                    listing.0 = name;
                }
            })
            .before(systems::publish_display_name),
        )
    }
}

/// Newtype wrapper around [`bevy_ensemble_sockets::EnsembleSocket`] so it can be used as a Bevy Resource.
#[cfg(feature = "client")]
#[derive(Resource, Deref, DerefMut)]
pub(crate) struct EnsembleSocketRes(bevy_ensemble_sockets::EnsembleSocket);

/// Holds the Tokio runtime (native only) and plugin config so the socket can be recreated
/// when leaving and rejoining lobbies.
#[cfg(feature = "client")]
#[derive(Resource)]
pub(crate) struct WebrtcRuntime {
    #[cfg(not(target_arch = "wasm32"))]
    runtime: tokio::runtime::Runtime,
    server_url: String,
    pub(crate) max_players: u32,
    ice_servers: IceServers,
    pub(crate) join_timeout: Option<std::time::Duration>,
    game: String,
}

#[cfg(feature = "client")]
impl WebrtcRuntime {
    /// Build a fresh EnsembleSocket + lobby connection and start the WS handler task.
    ///
    /// Called at init and again each time the player leaves a lobby or the signalling connection
    /// is rebuilt, which is why `display_name` is a parameter rather than a field: every one of
    /// those is a new connection that has to authenticate, and it has to authenticate as whoever
    /// the player is *now*. Held as a field, the name the plugin was built with was re-sent every
    /// time — and [`systems::publish_display_name`] would not correct it, because it fires on the
    /// resource changing and the resource had not changed. A player who typed their name and then
    /// left one lobby was listed under the builder's placeholder for every lobby after it.
    pub(crate) fn build_socket(
        &self,
        display_name: &str,
    ) -> (EnsembleSocketRes, connection::LobbyConnection) {
        use std::sync::{Arc, Mutex};

        use connection::WsHandlerBuilder;
        use protocol::ClientMessage;
        use tokio::sync::mpsc;

        let (lobby_event_tx, lobby_event_rx) = mpsc::unbounded_channel();
        let (lobby_command_tx, lobby_command_rx) = mpsc::unbounded_channel();
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();

        // Ask for the listing as soon as the connection exists, so a server browser has something
        // in it without the player pressing anything. Queued rather than sent: the handler task
        // writes `Authenticate` and `DeclareCapabilities` straight to the socket before it ever
        // reads this channel, so this lands third however quickly it is queued.
        //
        // Every fresh connection does it, and that is deliberate — the two rebuilds are leaving a
        // lobby and recovering a dropped signalling socket, and in both the listing a game is
        // holding is exactly as stale as the connection it came from.
        //
        // Which game this is goes first, so the server knows it before the listing is asked for
        // and before any lobby is created. Every connection, because the server keeps it per
        // connection.
        if !self.game.is_empty() {
            let _ = lobby_command_tx.send(ClientMessage::DeclareGame {
                game: self.game.clone(),
            });
        }
        let _ = lobby_command_tx.send(ClientMessage::ListLobbies);

        let ws_builder = WsHandlerBuilder {
            display_name: display_name.to_owned(),
            lobby_event_tx,
            lobby_command_rx: Arc::new(Mutex::new(Some(lobby_command_rx))),
            signal_tx,
            #[cfg(not(target_arch = "wasm32"))]
            runtime_handle: self.runtime.handle().clone(),
        };

        ws_builder.start(self.server_url.clone());

        // Create the EnsembleSocket
        #[cfg(not(target_arch = "wasm32"))]
        let socket = bevy_ensemble_sockets::EnsembleSocket::new(self.runtime.handle().clone())
            .with_ice_servers(self.ice_servers.clone());
        #[cfg(target_arch = "wasm32")]
        let socket =
            bevy_ensemble_sockets::EnsembleSocket::new().with_ice_servers(self.ice_servers.clone());

        let lobby_connection = connection::LobbyConnection {
            command_tx: lobby_command_tx,
            event_rx: std::sync::Mutex::new(lobby_event_rx),
            signal_rx: std::sync::Mutex::new(signal_rx),
            local_player_uuid: None,
            signalling_lost: false,
            announced_name: display_name.to_owned(),
            server_outdated: false,
            unanswered: Default::default(),
            next_keep_alive_at: std::time::Duration::ZERO,
        };

        (EnsembleSocketRes(socket), lobby_connection)
    }
}

#[cfg(feature = "client")]
impl Plugin for BevyEnsembleWebrtcPlugin {
    fn build(&self, app: &mut App) {
        let webrtc_runtime = WebrtcRuntime {
            #[cfg(not(target_arch = "wasm32"))]
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Failed to create Tokio runtime"),
            server_url: self.server_url.clone(),
            max_players: self.max_players,
            ice_servers: self.ice_servers.clone(),
            join_timeout: self.join_timeout,
            game: self.game.clone(),
        };

        let (socket, lobby_connection) = webrtc_runtime.build_socket(&self.display_name);

        app.claim_transport("bevy_ensemble_webrtc")
            .insert_resource(webrtc_runtime)
            .insert_resource(lobby_connection)
            .insert_resource(socket)
            .insert_resource(SignallingDisplayName(self.display_name.clone()))
            .add_message::<connection::LobbyEvent>()
            .add_message::<JoinWebrtcLobby>()
            .add_message::<JoinWebrtcLobbyByCode>()
            .add_message::<RefreshLobbyList>()
            .add_message::<RelayReport>()
            .add_message::<SignallingRefused>()
            // A control message: never relayed through the broadcast path, and on a client
            // taken only from the host. `from_host: true` from anybody else is refused before it
            // is decoded, on top of the sender check in `promote_client_lobby_on_host_handshake`.
            .register_backend_handshake_message_type::<handshake::WebrtcReadyHandshake>(
                "bevy_ensemble_webrtc/ReadyHandshake",
                MessageAuthority::HostOnly,
            )
            .init_resource::<systems::UntrustedPacketDrops>()
            .init_resource::<systems::DeferredSignals>()
            .add_systems(
                Update,
                (
                    systems::flush_lobby_events,
                    systems::apply_lobby_events,
                    systems::publish_display_name,
                )
                    .chain(),
            )
            .add_systems(
                Update,
                (
                    // After the lobby events, so a request goes out only once this frame's answers
                    // have been read: an answer can then never arrive before the pending lobby
                    // is marked with the request it answers, and be taken for one nobody wants.
                    systems::create_lobby.after(systems::apply_lobby_events),
                    systems::join_requested_lobbies.after(systems::apply_lobby_events),
                    systems::join_requested_lobbies_by_code.after(systems::apply_lobby_events),
                    systems::refresh_lobby_list,
                    // After the listing is applied and before the join and the refresh it asks
                    // for are sent, so both go out the frame they are decided on, and the pending
                    // lobby a join spawns exists before this runs again.
                    join_first::join_first_lobby
                        .run_if(resource_exists::<JoinFirstLobby>)
                        .after(systems::apply_lobby_events)
                        .before(systems::join_requested_lobbies)
                        .before(systems::refresh_lobby_list),
                    systems::poll_socket_peers,
                    systems::poll_peer_routes,
                    // After the lobby events so that, on the frame a client's `LobbyJoined` and
                    // its host's offer both arrive, the host is known before the offer is judged.
                    // The signalling server sends them in that order and the WebSocket task
                    // forwards them in that order; this keeps it so across the two channels.
                    systems::pump_socket_signals.after(systems::apply_lobby_events),
                    systems::resend_rate_limited_requests.after(systems::apply_lobby_events),
                    systems::refuse_requests_to_an_outdated_server
                        .after(systems::apply_lobby_events)
                        .after(systems::create_lobby)
                        .after(systems::join_requested_lobbies)
                        .after(systems::join_requested_lobbies_by_code),
                    systems::send_keep_alives,
                    handshake::send_client_handshakes,
                    handshake::send_host_handshakes,
                    handshake::promote_client_lobby_on_host_handshake,
                    handshake::promote_host_client_on_client_handshake,
                    systems::time_out_pending_lobbies,
                    // Leaving rebuilds the signalling connection; the reconnect below only
                    // steps in when no leave is going to. Ordered so it can tell.
                    systems::detect_lobby_leave.after(systems::apply_lobby_events),
                    systems::reconnect_signalling.after(systems::detect_lobby_leave),
                ),
            )
            // Drain the socket in PreUpdate so every Update reader (core and game) sees
            // this frame's packets the same frame they arrived.
            .add_systems(
                PreUpdate,
                systems::read_peer_messages.in_set(bevy_ensemble::EnsembleSet::ReceivePackets),
            )
            // `bevy_ensemble`'s backend-neutral session requests, so a game does not have to
            // name this crate to host, list, join or leave. See `session`.
            .add_systems(
                Update,
                (
                    session::refresh_lobbies,
                    session::join_lobby,
                    session::leave_lobby,
                    session::close_lobby,
                ),
            )
            .add_observer(systems::send_serialized_lobby_packet)
            .add_observer(systems::cancel_abandoned_request)
            .add_observer(systems::disconnect_removed_lobby_client);
    }
}
