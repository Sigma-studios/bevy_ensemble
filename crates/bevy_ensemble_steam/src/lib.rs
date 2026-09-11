use bevy::prelude::*;
use bevy_ensemble::{
    EnsembleAppExt, EnsembleTransportAppExt, Host, HostUuid, Instant, Lobby, LobbyClient,
    LobbyClientPlayerUuid, LobbyJoinFailed, LobbyLeft, LobbyLeftReason, LobbyParticipantOf,
    LocalMultiplayerPlayerId, MessageAuthority, PendingLobby, RequestLobby, SerializedLobbyPacket,
    decode_ensemble_packet, encode_ensemble_message,
};

/// The `bevy-steamworks` this crate is built against.
///
/// Use it — `bevy_ensemble_steam::bevy_steamworks::Client` — rather than depending on
/// `bevy-steamworks` yourself. [`BevyEnsembleSteamPlugin`] adds `SteamworksPlugin`, and that is
/// what inserts the `Client` resource; a consumer that reaches `Client` through a second copy of
/// the crate gets a second, unrelated type. Cargo builds both without complaint, and then every
/// system taking `Res<Client>` silently fails parameter validation at runtime, saying nothing
/// about why. Going through this re-export makes that impossible rather than documented.
pub use bevy_steamworks;
pub use bevy_steamworks::LobbyId;
use bevy_steamworks::{
    CallbackResult, ChatMemberStateChange, ChatRoomEnterResponse, Client, FriendFlags,
    LobbyDataUpdate, LobbyType, SteamId, SteamworksEvent, SteamworksPlugin,
    networking_types::{NetworkingIdentity, SendFlags},
};
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

mod session;

pub const DEFAULT_STEAM_APP_ID: u32 = 480;
/// TODO: this is EResult::k_EResultOK, swap it out for the proper type once exposed by steamworks-rs
const STEAM_RESULT_OK: u32 = 1;
pub const MAX_LOBBY_PLAYERS: u32 = 8;
pub struct BevyEnsembleSteamPlugin {
    pub app_id: u32,
}

#[derive(Component)]
pub struct LobbySteamId(pub LobbyId);

#[derive(Component)]
pub struct LobbyClientSteamId(pub SteamId);

/// The owner of the Steam lobby at the moment this client joined it — the peer this client
/// treats as its host for as long as the lobby entity lives.
///
/// Pinned rather than read back from `lobby_owner()` each time because Steam migrates lobby
/// ownership when the owner leaves: after that, "the lobby owner" is some other member, and a
/// client that kept trusting whoever Steam names would start taking host-only messages from a
/// peer it never joined. If the pinned host goes, the lobby is torn down instead.
#[derive(Component, Clone, Copy, Debug)]
pub struct LobbyHostSteamId(pub SteamId);

/// The Steam lobby this peer is in, readable from outside the ECS.
///
/// Steam's session-request callback runs with no access to the world, and it has to answer "is
/// this SteamID a member of my lobby?" before a P2P session opens. So the systems that create,
/// join and leave a lobby keep this cell current, and the callback reads it. Zero means no lobby.
#[derive(Resource, Clone, Default)]
struct CurrentSteamLobby(Arc<AtomicU64>);

impl CurrentSteamLobby {
    fn get(&self) -> Option<LobbyId> {
        match self.0.load(Ordering::Acquire) {
            0 => None,
            raw => Some(LobbyId::from_raw(raw)),
        }
    }

    fn set(&self, lobby: Option<LobbyId>) {
        self.0
            .store(lobby.map_or(0, |lobby| lobby.raw()), Ordering::Release);
    }
}

/// Which side of the lobby this peer is on, for the trust decisions below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerRole {
    Host,
    Client,
}

/// Whether to open a P2P session with `requester`: only a member of the lobby this peer is in.
///
/// Anyone who knows a SteamID can ask Steam to open a messages session with it; the lobby is
/// what says who is actually part of this game. With no lobby (`members` empty) nobody is.
fn accept_session_request(members: &[SteamId], requester: SteamId) -> bool {
    members.contains(&requester)
}

/// Whether a packet from `from` should be decoded at all.
///
/// A host takes packets from its lobby's members — it is about to spawn a `LobbyClient` for
/// whoever sends one, so a stranger must not get that far. A client takes packets from the one
/// peer it joined, `lobby_owner` (see [`LobbyHostSteamId`]); other members talk to it through
/// the host's relay, never directly, so anything direct from them is dropped unread. A client
/// that does not yet know its host trusts nobody.
fn accept_packet(
    role: PeerRole,
    lobby_owner: Option<SteamId>,
    members: &[SteamId],
    from: SteamId,
) -> bool {
    match role {
        PeerRole::Host => members.contains(&from),
        PeerRole::Client => lobby_owner == Some(from),
    }
}

/// Refusals are worth a `warn!` the first few times and noise after that.
const REFUSALS_LOGGED_LOUDLY: u64 = 3;

#[derive(Clone, Debug)]
pub struct SteamFriendLobbySummary {
    pub lobby_id: LobbyId,
    pub host_name: String,
    pub member_count: usize,
}

#[derive(Resource, Clone, Debug, Default)]
pub struct SteamFriendLobbies(pub Vec<SteamFriendLobbySummary>);

#[derive(Resource, Debug)]
struct PendingSteamFriendLobbies {
    lobbies: HashMap<u64, SteamFriendLobbySummary>,
}

fn request_lobby_data(lobby_id: LobbyId) -> bool {
    unsafe {
        // The safe `steamworks` wrapper doesn't expose RequestLobbyData yet (it's
        // on steamworks-rs `master`, unreleased), so drop to raw FFI. Once a
        // `steamworks` release adds `Matchmaking::request_lobby_data`, replace
        // this with the safe call and remove the `steamworks-sys` dep + the
        // workspace `[patch.crates-io]` for it.
        let mm = steamworks_sys::SteamAPI_SteamMatchmaking_v009();
        steamworks_sys::SteamAPI_ISteamMatchmaking_RequestLobbyData(mm, lobby_id.raw())
    }
}

#[derive(Message, Clone, Copy, Debug)]
pub struct JoinSteamLobby(pub LobbyId);

#[derive(Component)]
struct PendingSteamLobbyClient;

#[derive(Message, Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct SteamReadyHandshake {
    from_host: bool,
}

impl Default for BevyEnsembleSteamPlugin {
    fn default() -> Self {
        Self {
            app_id: DEFAULT_STEAM_APP_ID,
        }
    }
}

impl Plugin for BevyEnsembleSteamPlugin {
    fn build(&self, app: &mut App) {
        app.claim_transport("bevy_ensemble_steam");
        app.add_plugins(
            SteamworksPlugin::init_app(self.app_id)
                .expect("Steamworks initialization plugin should build with a valid app id"),
        )
        .add_message::<JoinSteamLobby>()
        // A control message: never relayed by the broadcast path, and on a client only taken
        // from the host. That only restricts what a *client* accepts, so the client -> host
        // half of the handshake is unaffected.
        .register_control_message_type::<SteamReadyHandshake>(
            "bevy_ensemble_steam/ReadyHandshake",
            MessageAuthority::HostOnly,
        )
        .init_resource::<CurrentSteamLobby>()
        .add_systems(Startup, (announce_local_identity, setup_join_policy))
        .add_observer(forget_lobby)
        .add_systems(
            Update,
            (
                populate_friend_lobbies,
                create_lobby,
                join_requested_lobbies,
                react_to_events,
            ),
        )
        .add_systems(
            Update,
            (
                send_client_handshakes,
                send_host_handshakes,
                promote_client_lobby_on_host_handshake,
                promote_host_client_on_client_handshake,
            ),
        )
        // Drain messages in PreUpdate so every Update reader (core and game) sees this
        // frame's packets the same frame they arrived.
        .add_systems(
            PreUpdate,
            read_messages.in_set(bevy_ensemble::EnsembleSet::ReceivePackets),
        )
        // `bevy_ensemble`'s backend-neutral session requests, so a game does not have to name
        // this crate — or know how a Steam session is torn down — to host, list, join or leave.
        // See `session`.
        .init_resource::<bevy_ensemble::PublicLobbies>()
        .add_systems(
            Update,
            (
                session::refresh_lobbies,
                session::publish_public_lobbies,
                session::join_lobby,
                session::leave_lobby,
            ),
        )
        .add_observer(send_serialized_lobby_packet);
    }
}

fn send_message(steam_client: &Client, target: SteamId, data: &[u8], send_flags: SendFlags) {
    if let Err(e) = steam_client.networking_messages().send_message_to_user(
        NetworkingIdentity::new_steam_id(target),
        send_flags,
        data,
        0,
    ) {
        error!("Failed to send message to {:?}: {:?}", target, e);
    }
}

/// The host a client lobby talks to: the one pinned at join time, or — for a lobby entity that
/// somehow lacks the pin — whoever Steam currently names.
fn host_of(steam_client: &Client, lobby_id: LobbyId, pinned: Option<&LobbyHostSteamId>) -> SteamId {
    pinned
        .map(|host| host.0)
        .unwrap_or_else(|| steam_client.matchmaking().lobby_owner(lobby_id))
}

fn send_to_lobby(
    steam_client: &Client,
    lobby_id: &LobbySteamId,
    host: Option<&Host>,
    pinned_host: Option<&LobbyHostSteamId>,
    packet: &[u8],
    send_flags: SendFlags,
) {
    match host {
        Some(_) => {
            let local_steam_id = steam_client.user().steam_id();
            for client in steam_client.matchmaking().lobby_members(lobby_id.0) {
                if client != local_steam_id {
                    send_message(steam_client, client, packet, send_flags);
                }
            }
        }
        None => {
            let host = host_of(steam_client, lobby_id.0, pinned_host);
            send_message(steam_client, host, packet, send_flags);
        }
    }
}

fn send_serialized_lobby_packet(
    packet: On<SerializedLobbyPacket>,
    steam_client: Res<Client>,
    lobby_query: Query<(&LobbySteamId, Option<&Host>, Option<&LobbyHostSteamId>), With<Lobby>>,
    pending_lobby_query: Query<
        (&LobbySteamId, Option<&Host>, Option<&LobbyHostSteamId>),
        (With<PendingLobby>, Without<Lobby>),
    >,
    lobby_client_query: Query<&LobbyClientSteamId>,
) {
    // The only backend where `ReliableNoDelay` differs from `Reliable`, because it is the only one
    // that coalesces: bare `RELIABLE` runs Steam's Nagle timer, which holds a small message ~5 ms
    // hoping to pack it with the next. See `SendMode` for when that is worth paying and when it is
    // pure loss.
    let send_flags = match packet.send_mode {
        bevy_ensemble::SendMode::Reliable => SendFlags::RELIABLE,
        bevy_ensemble::SendMode::ReliableNoDelay => SendFlags::RELIABLE_NO_NAGLE,
        bevy_ensemble::SendMode::Unreliable => SendFlags::UNRELIABLE_NO_NAGLE,
    };

    if let Ok((lobby_id, host, pinned_host)) = lobby_query.get(packet.entity) {
        send_to_lobby(
            &steam_client,
            lobby_id,
            host,
            pinned_host,
            &packet.packet,
            send_flags,
        );
        return;
    }

    if let Ok((lobby_id, host, pinned_host)) = pending_lobby_query.get(packet.entity) {
        send_to_lobby(
            &steam_client,
            lobby_id,
            host,
            pinned_host,
            &packet.packet,
            send_flags,
        );
        return;
    }

    if let Ok(client_steam_id) = lobby_client_query.get(packet.entity) {
        send_message(&steam_client, client_steam_id.0, &packet.packet, send_flags);
        return;
    }

    error!(
        "Serialized lobby packet was triggered for unsupported entity {:?}",
        packet.entity
    );
}

/// This peer's identity is its SteamID, known from the first frame — so say so before any lobby
/// exists, rather than leaving the core to guess a placeholder when hosting starts.
fn announce_local_identity(mut commands: Commands, steam_client: Res<Client>) {
    commands.insert_resource(LocalMultiplayerPlayerId(u128::from(
        steam_client.user().steam_id().raw(),
    )));
}

/// Accept a P2P session only from a member of the lobby this peer is in; see
/// [`accept_session_request`].
///
/// The callback is Steam's, not the schedule's: it gets a clone of the client and of the shared
/// lobby cell, and nothing else.
fn setup_join_policy(steam_client: Res<Client>, current_lobby: Res<CurrentSteamLobby>) {
    info!("Setting up join policy");
    let client = Client::clone(&steam_client);
    let current_lobby = CurrentSteamLobby::clone(&current_lobby);
    steam_client
        .networking_messages()
        .session_request_callback(move |request| {
            let requester = request.remote().steam_id();
            let lobby = current_lobby.get();
            let members = lobby
                .map(|lobby| client.matchmaking().lobby_members(lobby))
                .unwrap_or_default();
            match requester {
                Some(requester) if accept_session_request(&members, requester) => {
                    let accepted = request.accept();
                    info!(
                        "Accepted a session request from lobby member {requester:?} \
                         (accept returned {accepted})"
                    );
                }
                _ => {
                    info!(
                        "Refused a session request from {requester:?}: not a member of the \
                         current lobby {lobby:?}"
                    );
                    request.reject();
                }
            }
        });
    steam_client
        .networking_messages()
        .session_failed_callback(|args| {
            info!("Session failed: {:?}", args);
        });
}

fn create_lobby(steam_client: Res<Client>, lobbies: Query<(), (Added<RequestLobby>, With<Host>)>) {
    for _ in lobbies.iter() {
        steam_client
            .matchmaking()
            .create_lobby(LobbyType::FriendsOnly, MAX_LOBBY_PLAYERS, |_| {});
    }
}

fn populate_friend_lobbies(
    mut commands: Commands,
    steam_client: Res<Client>,
    existing_list: Option<Res<SteamFriendLobbies>>,
    pending_list: Option<Res<PendingSteamFriendLobbies>>,
) {
    if existing_list.is_some() || pending_list.is_some() {
        return;
    }

    let current_app_id = steam_client.utils().app_id();
    let mut seen_lobbies = HashSet::new();
    let mut lobbies = HashMap::new();

    for friend in steam_client.friends().get_friends(FriendFlags::IMMEDIATE) {
        let Some(friend_game) = friend.game_played() else {
            continue;
        };
        if friend_game.game.app_id() != current_app_id {
            continue;
        }

        let lobby_id = friend_game.lobby;
        if lobby_id.raw() == 0 || !seen_lobbies.insert(lobby_id.raw()) {
            continue;
        }

        request_lobby_data(lobby_id);

        lobbies.insert(
            lobby_id.raw(),
            SteamFriendLobbySummary {
                lobby_id,
                host_name: friend.name(),
                member_count: 0,
            },
        );
    }

    if lobbies.is_empty() {
        commands.insert_resource(SteamFriendLobbies::default());
    } else {
        commands.insert_resource(PendingSteamFriendLobbies { lobbies });
    }
}

fn hydrate_friend_lobbies(
    commands: &mut Commands,
    steam_client: &Client,
    pending: &mut PendingSteamFriendLobbies,
    data: &LobbyDataUpdate,
) {
    if !data.success {
        pending.lobbies.remove(&data.lobby.raw());
    } else if let Some(summary) = pending.lobbies.get_mut(&data.lobby.raw()) {
        summary.member_count = steam_client.matchmaking().lobby_member_count(data.lobby);
    }

    let all_hydrated = pending
        .lobbies
        .values()
        .all(|summary| summary.member_count > 0);

    if all_hydrated {
        let lobbies: Vec<_> = pending.lobbies.drain().map(|(_, v)| v).collect();
        commands.remove_resource::<PendingSteamFriendLobbies>();
        commands.insert_resource(SteamFriendLobbies(lobbies));
    }
}

fn join_requested_lobbies(
    mut commands: Commands,
    steam_client: Res<Client>,
    mut join_requests: MessageReader<JoinSteamLobby>,
    existing_client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
    pending_client_lobbies: Query<(), (With<PendingLobby>, Without<Host>)>,
) {
    let Some(join_request) = join_requests.read().last().copied() else {
        return;
    };
    if !existing_client_lobbies.is_empty() || !pending_client_lobbies.is_empty() {
        warn!("Ignoring join request while a client lobby is already active or pending");
        return;
    }

    commands.spawn(PendingLobby);
    steam_client
        .matchmaking()
        .join_lobby(join_request.0, |_| {});
}

/// A client-side lobby entity that has entered its Steam lobby: pending or promoted (`Has<Lobby>`).
type ClientLobbies<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static LobbySteamId,
        &'static LobbyHostSteamId,
        Has<Lobby>,
    ),
    (Or<(With<Lobby>, With<PendingLobby>)>, Without<Host>),
>;

fn react_to_events(
    mut commands: Commands,
    steam_client: Res<Client>,
    current_lobby: Res<CurrentSteamLobby>,
    mut events: MessageReader<SteamworksEvent>,
    mut lobby_left: MessageWriter<LobbyLeft>,
    mut join_failed: MessageWriter<LobbyJoinFailed>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    pending_host_lobbies: Query<Entity, (With<RequestLobby>, With<Host>)>,
    client_lobbies: ClientLobbies,
    pending_client_lobbies: Query<
        (Entity, Option<&LobbySteamId>),
        (With<PendingLobby>, Without<Host>),
    >,
    lobby_clients: Query<
        (
            Entity,
            &LobbyClientSteamId,
            &LobbyClientPlayerUuid,
            Option<&PendingSteamLobbyClient>,
        ),
        Or<(With<LobbyClient>, With<PendingSteamLobbyClient>)>,
    >,
    mut pending_friend_lobbies: Option<ResMut<PendingSteamFriendLobbies>>,
) {
    let local_steam_id = steam_client.user().steam_id();

    for event in events.read() {
        match event {
            SteamworksEvent::CallbackResult(event) => match event {
                CallbackResult::LobbyCreated(lobby) => {
                    info!("Lobby created: {:?}", lobby);
                    if lobby.result != STEAM_RESULT_OK {
                        error!("Lobby creation failed with result: {}", lobby.result);
                        for entity in pending_host_lobbies.iter() {
                            commands.entity(entity).try_despawn();
                        }
                        join_failed.write(LobbyJoinFailed {
                            reason: format!("Steam could not create the lobby ({})", lobby.result),
                        });
                        continue;
                    }
                    // The identity was announced at startup; re-saying it here costs nothing and
                    // covers a game (or the core's kick path) having removed it in between.
                    let own_uuid = u128::from(local_steam_id.raw());
                    commands.insert_resource(LocalMultiplayerPlayerId(own_uuid));
                    commands.insert_resource(HostUuid(own_uuid));
                    current_lobby.set(Some(lobby.lobby));
                    if let Some(entity) = pending_host_lobbies.iter().next() {
                        commands
                            .entity(entity)
                            .remove::<(PendingLobby, RequestLobby)>()
                            .insert((Lobby, LobbySteamId(lobby.lobby)));
                    }
                }
                CallbackResult::LobbyEnter(enter) => {
                    info!("Lobby entered: {:?}", enter.lobby);
                    // Only handle for client joins; host is handled by LobbyCreated.
                    let Some((entity, None)) = pending_client_lobbies
                        .iter()
                        .find(|(_, steam_id)| steam_id.is_none())
                    else {
                        continue;
                    };
                    match enter.chat_room_enter_response {
                        ChatRoomEnterResponse::Success => {
                            // Pin the host now, before the first handshake can arrive: the
                            // core takes host-only messages from `HostUuid` and nobody else,
                            // and `read_messages` decodes packets from this SteamID and nobody
                            // else.
                            let host = steam_client.matchmaking().lobby_owner(enter.lobby);
                            commands.insert_resource(LocalMultiplayerPlayerId(u128::from(
                                local_steam_id.raw(),
                            )));
                            commands.insert_resource(HostUuid(u128::from(host.raw())));
                            current_lobby.set(Some(enter.lobby));
                            commands
                                .entity(entity)
                                .insert((LobbySteamId(enter.lobby), LobbyHostSteamId(host)));
                        }
                        other => {
                            error!("Failed to enter lobby: {:?}", other);
                            commands.entity(entity).try_despawn();
                            join_failed.write(LobbyJoinFailed {
                                reason: format!("Steam refused the join: {other:?}"),
                            });
                        }
                    }
                }
                CallbackResult::GameLobbyJoinRequested(request) => {
                    if !client_lobbies.is_empty() || !pending_client_lobbies.is_empty() {
                        warn!(
                            "Ignoring Steam overlay join request while a client lobby is already active or pending"
                        );
                        continue;
                    }
                    info!("Game lobby join requested: {:?}", request.lobby_steam_id);
                    commands.spawn(PendingLobby);
                    steam_client
                        .matchmaking()
                        .join_lobby(request.lobby_steam_id, |_| {});
                }
                CallbackResult::LobbyDataUpdate(data) => {
                    debug!(
                        "Lobby member count: {:?}",
                        steam_client.matchmaking().lobby_member_count(data.lobby)
                    );
                    if let Some(pending) = pending_friend_lobbies.as_deref_mut() {
                        hydrate_friend_lobbies(&mut commands, &steam_client, pending, data);
                    }
                }
                CallbackResult::LobbyChatUpdate(update) => {
                    debug!("Lobby chat updated: {:?}", update);

                    match update.member_state_change {
                        ChatMemberStateChange::Left
                        | ChatMemberStateChange::Disconnected
                        | ChatMemberStateChange::Kicked
                        | ChatMemberStateChange::Banned => {
                            info!("Lobby member left: {:?}", update.user_changed);
                            if update.user_changed == local_steam_id {
                                // We are still holding a lobby entity for a lobby Steam says we
                                // are no longer in: a voluntary leave despawned it before this
                                // arrived, so this is Steam's doing.
                                let reason = match update.member_state_change {
                                    ChatMemberStateChange::Kicked
                                    | ChatMemberStateChange::Banned => LobbyLeftReason::Kicked,
                                    _ => LobbyLeftReason::HostGone,
                                };
                                if let Some(lobby) = client_lobbies
                                    .iter()
                                    .find(|(_, lobby_id, _, _)| lobby_id.0 == update.lobby)
                                {
                                    tear_down_client_lobby(
                                        &mut commands,
                                        &steam_client,
                                        &mut lobby_left,
                                        &mut join_failed,
                                        lobby,
                                        reason,
                                    );
                                }
                            } else if host_lobby.is_some() {
                                despawn_lobby_client_for_remote(
                                    &mut commands,
                                    &lobby_clients,
                                    update.user_changed,
                                );
                            } else if let Some(lobby) =
                                client_lobbies.iter().find(|(_, lobby_id, host, _)| {
                                    lobby_id.0 == update.lobby && host.0 == update.user_changed
                                })
                            {
                                // Steam will hand the lobby to another member; the session we
                                // joined is over regardless, so leave rather than follow.
                                info!("The host {:?} left the lobby", update.user_changed);
                                tear_down_client_lobby(
                                    &mut commands,
                                    &steam_client,
                                    &mut lobby_left,
                                    &mut join_failed,
                                    lobby,
                                    LobbyLeftReason::HostGone,
                                );
                            }
                        }
                        ChatMemberStateChange::Entered => {
                            if update.user_changed != local_steam_id {
                                if let Some(lobby) = host_lobby.as_ref() {
                                    ensure_pending_lobby_client_for_remote(
                                        &mut commands,
                                        &lobby_clients,
                                        **lobby,
                                        update.user_changed,
                                    );
                                }
                            }
                        }
                    }
                }
                CallbackResult::NetworkingMessagesSessionFailed(failed) => {
                    debug!("Networking session failed: {:?}", failed);

                    let Some(remote_identity) = failed.info.identity_remote() else {
                        continue;
                    };
                    let Some(remote) = remote_identity.steam_id() else {
                        continue;
                    };

                    if host_lobby.is_some() {
                        despawn_lobby_client_for_remote(&mut commands, &lobby_clients, remote);
                    } else if let Some(lobby) = client_lobbies
                        .iter()
                        // Only a promoted lobby: while the join is still pending, a session that
                        // fails to open is retried by the handshake, not given up on.
                        .find(|(_, _, host, promoted)| *promoted && host.0 == remote)
                    {
                        tear_down_client_lobby(
                            &mut commands,
                            &steam_client,
                            &mut lobby_left,
                            &mut join_failed,
                            lobby,
                            LobbyLeftReason::HostGone,
                        );
                    }
                }
                CallbackResult::PersonaStateChange(_) => {}
                CallbackResult::UserStatsReceived(_) => {}
                e => {
                    debug!("Unhandled event: {:?}", e);
                }
            },
        }
    }
}

/// Who this peer decodes packets from this frame; see [`accept_packet`].
struct PacketGate {
    role: PeerRole,
    /// The pinned host, on a client.
    owner: Option<SteamId>,
    /// The lobby's members, on a host. Read once per frame, not once per packet.
    members: Vec<SteamId>,
}

fn packet_gate(world: &mut World) -> Option<PacketGate> {
    let steam_client = world.resource::<Client>().clone();
    let mut lobbies = world.query_filtered::<
        (&LobbySteamId, Has<Host>, Option<&LobbyHostSteamId>),
        Or<(With<Lobby>, With<PendingLobby>)>,
    >();
    let (lobby_id, is_host, pinned_host) = lobbies.iter(world).next()?;
    Some(if is_host {
        PacketGate {
            role: PeerRole::Host,
            owner: None,
            members: steam_client.matchmaking().lobby_members(lobby_id.0),
        }
    } else {
        PacketGate {
            role: PeerRole::Client,
            owner: Some(host_of(&steam_client, lobby_id.0, pinned_host)),
            members: Vec::new(),
        }
    })
}

fn refuse_packet(refused: &mut u64, from: SteamId, why: &str) {
    *refused += 1;
    if *refused <= REFUSALS_LOGGED_LOUDLY {
        warn!(
            "dropped a packet from {from:?} unread: {why} ({refused} so far, later ones at \
             debug level)"
        );
    } else {
        debug!("dropped a packet from {from:?} unread: {why} ({refused} so far)");
    }
}

fn read_messages(world: &mut World, mut refused: Local<u64>) {
    const BATCH_SIZE: usize = 64;

    let gate = packet_gate(world);

    loop {
        let messages = {
            let steam_client = world.resource::<Client>();
            steam_client
                .networking_messages()
                .receive_messages_on_channel(0, BATCH_SIZE)
        };
        // Steam is polled, so this is the earliest point the bytes are known to have
        // arrived: one stamp for the whole drained batch.
        let received_at = Instant::now();

        let count = messages.len();

        for msg in &messages {
            let Some(steam_id) = msg.identity_peer().steam_id() else {
                continue;
            };
            let Some(gate) = &gate else {
                refuse_packet(&mut refused, steam_id, "this peer is not in a lobby");
                continue;
            };
            if !accept_packet(gate.role, gate.owner, &gate.members, steam_id) {
                let why = match gate.role {
                    PeerRole::Host => "not a member of the hosted lobby",
                    PeerRole::Client => "not the host this client joined",
                };
                refuse_packet(&mut refused, steam_id, why);
                continue;
            }
            if gate.role == PeerRole::Host {
                ensure_pending_lobby_client_for_remote_world(world, steam_id);
            }
            decode_ensemble_packet(
                world,
                Some(u128::from(steam_id.raw())),
                msg.data(),
                received_at,
            );
        }

        if count < BATCH_SIZE {
            break;
        }
    }
}

// Retries every 500ms because the peer's `PendingLobby` / `PendingSteamLobbyClient`
// entity may not exist yet when the first handshake arrives. Messages are sent
// reliably so loss isn't the concern — only the entity-readiness race.
fn send_client_handshakes(
    steam_client: Res<Client>,
    registry: Res<bevy_ensemble::EnsembleMessageRegistry>,
    pending_client_lobbies: Query<
        (&LobbySteamId, Option<&LobbyHostSteamId>),
        (With<PendingLobby>, Without<Lobby>, Without<Host>),
    >,
    time: Res<Time>,
    mut cooldown: Local<f32>,
) {
    *cooldown -= time.delta_secs();
    if *cooldown > 0.0 {
        return;
    }
    *cooldown = 0.5;

    let packet = encode_ensemble_message(&registry, &SteamReadyHandshake { from_host: false });
    for (lobby_id, pinned_host) in pending_client_lobbies.iter() {
        let host = host_of(&steam_client, lobby_id.0, pinned_host);
        send_message(&steam_client, host, &packet, SendFlags::RELIABLE);
    }
}

fn send_host_handshakes(
    steam_client: Res<Client>,
    registry: Res<bevy_ensemble::EnsembleMessageRegistry>,
    host_lobbies: Query<&LobbySteamId, (With<Lobby>, With<Host>)>,
    time: Res<Time>,
    mut cooldown: Local<f32>,
) {
    *cooldown -= time.delta_secs();
    if *cooldown > 0.0 {
        return;
    }
    *cooldown = 0.5;

    let Some(lobby_id) = host_lobbies.iter().next() else {
        return;
    };

    let packet = encode_ensemble_message(&registry, &SteamReadyHandshake { from_host: true });
    let local_steam_id = steam_client.user().steam_id();
    for remote in steam_client.matchmaking().lobby_members(lobby_id.0) {
        if remote == local_steam_id {
            continue;
        }
        send_message(&steam_client, remote, &packet, SendFlags::RELIABLE);
    }
}

fn promote_client_lobby_on_host_handshake(
    mut commands: Commands,
    steam_client: Res<Client>,
    mut messages: MessageReader<bevy_ensemble::ReceivedEnsembleMessage<SteamReadyHandshake>>,
    pending_client_lobbies: Query<
        (Entity, &LobbySteamId, Option<&LobbyHostSteamId>),
        (With<PendingLobby>, Without<Lobby>, Without<Host>),
    >,
) {
    for message in messages.read() {
        if !message.message.from_host {
            continue;
        }

        let Some(sender) = message.sender else {
            continue;
        };

        let Some((entity, _, _)) =
            pending_client_lobbies
                .iter()
                .find(|(_, lobby_steam_id, pinned_host)| {
                    u128::from(host_of(&steam_client, lobby_steam_id.0, *pinned_host).raw())
                        == sender
                })
        else {
            continue;
        };

        commands
            .entity(entity)
            .remove::<PendingLobby>()
            .insert(Lobby);
    }
}

fn promote_host_client_on_client_handshake(
    mut commands: Commands,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    mut messages: MessageReader<bevy_ensemble::ReceivedEnsembleMessage<SteamReadyHandshake>>,
    pending_clients: Query<
        (Entity, &LobbyClientPlayerUuid, &LobbyParticipantOf),
        (With<PendingSteamLobbyClient>, With<LobbyClientSteamId>),
    >,
) {
    let Some(host_lobby) = host_lobby else {
        return;
    };

    for message in messages.read() {
        if message.message.from_host {
            continue;
        }

        let Some(sender) = message.sender else {
            continue;
        };

        let Some((entity, _, _)) =
            pending_clients
                .iter()
                .find(|(_, player_uuid, participant_of)| {
                    participant_of.0 == *host_lobby && player_uuid.0 == sender
                })
        else {
            continue;
        };

        commands
            .entity(entity)
            .remove::<PendingSteamLobbyClient>()
            .insert(LobbyClient);
    }
}

fn ensure_pending_lobby_client_for_remote_world(world: &mut World, steam_id: SteamId) {
    let remote_player_uuid = u128::from(steam_id.raw());
    let host_lobby = {
        let mut host_lobbies = world.query_filtered::<Entity, (With<Lobby>, With<Host>)>();
        host_lobbies.iter(world).next()
    };
    let Some(host_lobby) = host_lobby else {
        return;
    };

    let already_known = {
        let mut lobby_clients = world.query::<(
            Option<&LobbyClient>,
            &LobbyClientSteamId,
            &LobbyClientPlayerUuid,
            Option<&PendingSteamLobbyClient>,
        )>();
        lobby_clients
            .iter(world)
            .any(|(_, client_steam_id, player_uuid, _)| {
                client_steam_id.0 == steam_id || player_uuid.0 == remote_player_uuid
            })
    };
    if already_known {
        return;
    }

    world.commands().spawn((
        PendingSteamLobbyClient,
        LobbyParticipantOf(host_lobby),
        LobbyClientSteamId(steam_id),
        LobbyClientPlayerUuid(remote_player_uuid),
    ));
}

fn ensure_pending_lobby_client_for_remote(
    commands: &mut Commands,
    lobby_clients: &Query<
        (
            Entity,
            &LobbyClientSteamId,
            &LobbyClientPlayerUuid,
            Option<&PendingSteamLobbyClient>,
        ),
        Or<(With<LobbyClient>, With<PendingSteamLobbyClient>)>,
    >,
    host_lobby: Entity,
    remote: SteamId,
) {
    let remote_player_uuid = u128::from(remote.raw());
    let already_known = lobby_clients
        .iter()
        .any(|(_, client_steam_id, player_uuid, _)| {
            client_steam_id.0 == remote || player_uuid.0 == remote_player_uuid
        });
    if already_known {
        return;
    }

    commands.spawn((
        PendingSteamLobbyClient,
        LobbyParticipantOf(host_lobby),
        LobbyClientSteamId(remote),
        LobbyClientPlayerUuid(remote_player_uuid),
    ));
}

fn despawn_lobby_client_for_remote(
    commands: &mut Commands,
    lobby_clients: &Query<
        (
            Entity,
            &LobbyClientSteamId,
            &LobbyClientPlayerUuid,
            Option<&PendingSteamLobbyClient>,
        ),
        Or<(With<LobbyClient>, With<PendingSteamLobbyClient>)>,
    >,
    remote: SteamId,
) {
    if let Some((client_entity, _, _, _)) = lobby_clients
        .iter()
        .find(|(_, client_steam_id, _, _)| client_steam_id.0 == remote)
    {
        commands.entity(client_entity).try_despawn();
    }
}

/// Tear down a client-side lobby this peer did not choose to leave, and say why.
///
/// Sessions first, then the Steam lobby, then the entity — the same order as
/// [`session::leave_lobby`], for the same reason. A promoted lobby ends with a [`LobbyLeft`];
/// one still pending never opened, so it ends with a [`LobbyJoinFailed`] instead.
fn tear_down_client_lobby(
    commands: &mut Commands,
    steam_client: &Client,
    lobby_left: &mut MessageWriter<LobbyLeft>,
    join_failed: &mut MessageWriter<LobbyJoinFailed>,
    (entity, lobby_id, host, promoted): (Entity, &LobbySteamId, &LobbyHostSteamId, bool),
    reason: LobbyLeftReason,
) {
    steam_client.networking().close_p2p_session(host.0);
    steam_client.matchmaking().leave_lobby(lobby_id.0);
    commands.entity(entity).try_despawn();

    if promoted {
        lobby_left.write(LobbyLeft { reason });
    } else {
        let reason = match reason {
            LobbyLeftReason::Kicked => "the host removed this player before the session opened",
            LobbyLeftReason::HostGone => "the host left before the session opened",
            _ => "the lobby closed before the session opened",
        };
        join_failed.write(LobbyJoinFailed {
            reason: reason.to_owned(),
        });
    }
}

/// A lobby entity is going: whoever despawned it — this crate, the core's kick path, the game —
/// the host it named is no longer this peer's host, and the session-request callback must stop
/// admitting its members.
fn forget_lobby(
    trigger: On<Remove, LobbySteamId>,
    mut commands: Commands,
    current_lobby: Res<CurrentSteamLobby>,
    lobbies: Query<&LobbySteamId>,
) {
    if let Ok(lobby_id) = lobbies.get(trigger.event_target()) {
        if current_lobby.get() == Some(lobby_id.0) {
            current_lobby.set(None);
        }
    }
    commands.remove_resource::<HostUuid>();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: u64) -> SteamId {
        SteamId::from_raw(raw)
    }

    #[test]
    fn a_session_request_from_a_non_member_is_refused() {
        assert!(!accept_session_request(&[id(1), id(2)], id(3)));
        assert!(
            !accept_session_request(&[], id(3)),
            "with no lobby there are no members, so nobody gets a session"
        );
    }

    #[test]
    fn a_session_request_from_a_member_is_accepted() {
        assert!(accept_session_request(&[id(1), id(2)], id(2)));
    }

    #[test]
    fn a_client_decodes_only_the_lobby_owner() {
        let members = [id(1), id(2), id(3)];
        assert!(accept_packet(
            PeerRole::Client,
            Some(id(1)),
            &members,
            id(1)
        ));
        assert!(
            !accept_packet(PeerRole::Client, Some(id(1)), &members, id(2)),
            "a fellow member is not the host, even though it is in the lobby"
        );
        assert!(
            !accept_packet(PeerRole::Client, None, &members, id(1)),
            "a client that does not know its host yet trusts nobody"
        );
    }

    #[test]
    fn a_host_decodes_only_members() {
        let members = [id(1), id(2)];
        assert!(accept_packet(PeerRole::Host, None, &members, id(2)));
        assert!(!accept_packet(PeerRole::Host, None, &members, id(3)));
        assert!(!accept_packet(PeerRole::Host, None, &[], id(1)));
    }
}
