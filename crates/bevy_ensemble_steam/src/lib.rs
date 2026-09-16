use bevy::prelude::*;
use bevy_ensemble::{
    AwaitingHost, EnsembleAppExt, EnsembleTransportAppExt, Host, HostLost, HostMigratable,
    HostUuid, Instant, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyJoinFailed, LobbyLeft,
    LobbyLeftReason, LobbyParticipantOf, LocalMultiplayerPlayerId, MessageAuthority, NewHostNamed,
    ParticipantDeparted, PendingLobby, RequestLobby, SerializedLobbyPacket, decode_ensemble_packet,
    encode_ensemble_message,
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

/// The peer this client treats as its host: the Steam lobby's owner when the client joined, and
/// after that whoever Steam hands ownership to.
///
/// Pinned rather than read back from `lobby_owner()` on every packet. Steam reassigns ownership
/// when the owner leaves, and trust has to move in one step: [`follow_lobby_owner`] repins it and
/// tells the core in the same frame, so the new host's packets are compared against the protocol
/// before any of them is read, and the old host's are dropped from then on.
#[derive(Component, Clone, Copy, Debug)]
pub struct LobbyHostSteamId(pub SteamId);

/// How long a Steam lobby waits for a new owner, then for the new owner to be reached.
///
/// Steam hands ownership over as soon as an owner leaves the lobby. An owner that crashed or lost
/// its connection is only dropped once Steam's servers notice, which takes tens of seconds, so the
/// wait is a minute. Reaching the new owner opens a P2P session, as a join does.
const STEAM_HOST_MIGRATION: HostMigratable = HostMigratable {
    successor_within: std::time::Duration::from_secs(60),
    reach_within: std::time::Duration::from_secs(20),
};

/// What a client should do about the lobby owner Steam names this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnerChange {
    /// The owner is still the pinned host, or Steam names nobody.
    None,
    /// Steam made this peer the owner: it hosts now.
    Promote,
    /// Steam made another member the owner: follow it.
    Follow(SteamId),
}

/// Compare the owner Steam names with the host this client pinned.
///
/// An owner of 0 means Steam's cache has no answer for the lobby right now (it is being left, or
/// its data has not arrived), not that nobody owns it, so it changes nothing.
fn owner_change(pinned: SteamId, owner: SteamId, local: SteamId) -> OwnerChange {
    if owner.raw() == 0 || owner == pinned {
        OwnerChange::None
    } else if owner == local {
        OwnerChange::Promote
    } else {
        OwnerChange::Follow(owner)
    }
}

/// Whether a host was replaced: Steam names somebody else as the owner of the lobby it hosts.
///
/// It happens only when Steam dropped this peer's connection and handed the lobby to a member.
/// The members follow the new owner, so this peer's session is over.
fn replaced_as_host(owner: SteamId, local: SteamId) -> bool {
    owner.raw() != 0 && owner != local
}

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

/// Close this peer's messaging session with `user`.
///
/// Every packet goes out through `ISteamNetworkingMessages`, which opens a session with its
/// recipient on the first send. Sessions used to be closed with `close_p2p_session`, which is the
/// older `ISteamNetworking` API and has no hold on those: nothing was ever closed, and Steam kept
/// each session until its own idle timeout. The safe wrapper has no close for the messages
/// interface, so this is raw FFI, like [`request_lobby_data`].
pub(crate) fn close_session(user: SteamId) -> bool {
    unsafe {
        let mut identity: steamworks_sys::SteamNetworkingIdentity = std::mem::zeroed();
        steamworks_sys::SteamAPI_SteamNetworkingIdentity_Clear(&mut identity);
        steamworks_sys::SteamAPI_SteamNetworkingIdentity_SetSteamID64(&mut identity, user.raw());
        let messages = steamworks_sys::SteamAPI_SteamNetworkingMessages_SteamAPI_v002();
        steamworks_sys::SteamAPI_ISteamNetworkingMessages_CloseSessionWithUser(messages, &identity)
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
        .register_backend_handshake_message_type::<SteamReadyHandshake>(
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
                follow_lobby_owner
                    .after(react_to_events)
                    // Leaving hands the Steam lobby to somebody else: read the owner before this
                    // peer's own leave can look like it was replaced.
                    .before(session::leave_lobby),
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
                session::close_lobby,
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
    migratable: Query<(), With<HostMigratable>>,
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
                            .insert((Lobby, LobbySteamId(lobby.lobby), STEAM_HOST_MIGRATION));
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
                            commands.entity(entity).insert((
                                LobbySteamId(enter.lobby),
                                LobbyHostSteamId(host),
                                STEAM_HOST_MIGRATION,
                            ));
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
                                        lobby,
                                        reason,
                                    );
                                }
                            } else if let Some(lobby) = host_lobby.as_deref().copied() {
                                let connected = despawn_lobby_client_for_remote(
                                    &mut commands,
                                    &lobby_clients,
                                    update.user_changed,
                                );
                                // A connected seat's removal takes its participant with it. A
                                // member this host inherited and never reached has no seat to
                                // remove, but it may have a participant.
                                if !connected {
                                    let player_uuid = u128::from(update.user_changed.raw());
                                    commands.entity(lobby).trigger(move |entity| {
                                        ParticipantDeparted {
                                            entity,
                                            player_uuid,
                                        }
                                    });
                                }
                            } else if let Some(lobby) =
                                client_lobbies.iter().find(|(_, lobby_id, host, _)| {
                                    lobby_id.0 == update.lobby && host.0 == update.user_changed
                                })
                            {
                                info!("The host {:?} left the lobby", update.user_changed);
                                let (entity, lobby_id, host, promoted) = lobby;
                                if promoted
                                    && migratable.contains(entity)
                                    && !lobby_is_closed(&steam_client, lobby_id.0)
                                {
                                    // Steam is about to name a new owner, or already has, in which
                                    // case `follow_lobby_owner` follows it this frame.
                                    if steam_client.matchmaking().lobby_owner(lobby_id.0) == host.0
                                    {
                                        commands
                                            .entity(entity)
                                            .trigger(|entity| HostLost { entity });
                                    }
                                } else {
                                    tear_down_client_lobby(
                                        &mut commands,
                                        &steam_client,
                                        lobby,
                                        LobbyLeftReason::HostGone,
                                    );
                                }
                            } else if let Some((entity, _, _, _)) = client_lobbies
                                .iter()
                                .find(|(_, lobby_id, _, _)| lobby_id.0 == update.lobby)
                            {
                                // Another member. The core drops it from the roster only if no
                                // host is there to do so.
                                let player_uuid = u128::from(update.user_changed.raw());
                                commands.entity(entity).trigger(move |entity| {
                                    ParticipantDeparted {
                                        entity,
                                        player_uuid,
                                    }
                                });
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
                        let (entity, lobby_id, _, _) = lobby;
                        if migratable.contains(entity) {
                            // The next send opens a new session. While the host is still in the
                            // lobby it may answer on it, so the liveness check decides, and a
                            // pong can still take it back. A host that left is lost.
                            let still_member = steam_client
                                .matchmaking()
                                .lobby_members(lobby_id.0)
                                .contains(&remote);
                            if !still_member {
                                commands
                                    .entity(entity)
                                    .trigger(|entity| HostLost { entity });
                            }
                            continue;
                        }
                        tear_down_client_lobby(
                            &mut commands,
                            &steam_client,
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

/// Follow Steam's choice of lobby owner.
///
/// Read from `lobby_owner()` every frame. It is a read of Steam's local cache, so it costs nothing,
/// and it does not depend on the order in which Steam delivers the member-left and data-update
/// callbacks. On a client whose pinned host is no longer the owner, this peer is promoted or
/// follows the new owner. Either way the pin changes and `NewHostNamed` is triggered in the same
/// frame, with Steam's member list. A host that Steam no longer names as owner was replaced, and
/// its session ends.
fn follow_lobby_owner(
    mut commands: Commands,
    steam_client: Res<Client>,
    mut lobby_left: MessageWriter<LobbyLeft>,
    client_lobbies: Query<
        (
            Entity,
            &LobbySteamId,
            &LobbyHostSteamId,
            Option<&AwaitingHost>,
            Has<Lobby>,
        ),
        (
            Or<(With<Lobby>, With<PendingLobby>)>,
            Without<Host>,
            With<HostMigratable>,
        ),
    >,
    host_lobbies: Query<(Entity, &LobbySteamId), (With<Lobby>, With<Host>, With<HostMigratable>)>,
    lobby_clients: Query<
        (
            Entity,
            &LobbyClientSteamId,
            &LobbyClientPlayerUuid,
            Option<&PendingSteamLobbyClient>,
        ),
        Or<(With<LobbyClient>, With<PendingSteamLobbyClient>)>,
    >,
) {
    let local = steam_client.user().steam_id();
    let matchmaking = steam_client.matchmaking();

    for (entity, lobby_id, pinned, awaiting, promoted) in client_lobbies.iter() {
        let owner = matchmaking.lobby_owner(lobby_id.0);
        let change = owner_change(pinned.0, owner, local);
        if change == OwnerChange::None {
            continue;
        }
        if lobby_is_closed(&steam_client, lobby_id.0) {
            info!("the host closed the lobby; leaving rather than following {owner:?}");
            tear_down_client_lobby(
                &mut commands,
                &steam_client,
                (entity, lobby_id, pinned, promoted),
                LobbyLeftReason::HostGone,
            );
            continue;
        }
        let previous = pinned.0;
        match awaiting {
            Some(awaiting) => info!(
                "Steam named {owner:?} owner in place of {previous:?}, {:.1}s after the host was \
                 lost",
                awaiting.waited.as_secs_f64()
            ),
            None => info!("Steam named {owner:?} owner in place of {previous:?}"),
        }
        let members: Vec<SteamId> = matchmaking
            .lobby_members(lobby_id.0)
            .into_iter()
            .filter(|member| *member != previous)
            .collect();
        close_session(previous);
        if change == OwnerChange::Promote {
            // A host opens a seat for every member, as it does for each one that enters.
            commands.entity(entity).remove::<LobbyHostSteamId>();
            for member in members.iter().copied().filter(|member| *member != local) {
                ensure_pending_lobby_client_for_remote(
                    &mut commands,
                    &lobby_clients,
                    entity,
                    member,
                );
            }
        } else {
            commands.entity(entity).insert(LobbyHostSteamId(owner));
        }
        let (previous, new_host) = (u128::from(previous.raw()), u128::from(owner.raw()));
        let members: Vec<u128> = members
            .iter()
            .map(|member| u128::from(member.raw()))
            .collect();
        commands.entity(entity).trigger(move |entity| NewHostNamed {
            entity,
            previous,
            new_host,
            members: Some(members),
        });
    }

    for (entity, lobby_id) in host_lobbies.iter() {
        let owner = matchmaking.lobby_owner(lobby_id.0);
        if !replaced_as_host(owner, local) {
            continue;
        }
        warn!(
            "Steam handed the lobby this peer hosts to {owner:?}: this peer's connection to Steam \
             was lost, and the members follow the new owner; ending the session"
        );
        for (_, seat, _, _) in lobby_clients.iter() {
            close_session(seat.0);
        }
        // The seats go with the lobby, after it, so none of them is told it was kicked.
        commands.entity(entity).try_despawn();
        lobby_left.write(LobbyLeft {
            reason: LobbyLeftReason::SignallingLost,
        });
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
//
// Sent while joining, and again while following a new host: the new host seats this peer
// when this handshake arrives, as it does for a joiner.
fn send_client_handshakes(
    steam_client: Res<Client>,
    registry: Res<bevy_ensemble::EnsembleMessageRegistry>,
    client_lobbies: Query<
        (
            &LobbySteamId,
            Option<&LobbyHostSteamId>,
            Has<Lobby>,
            Option<&AwaitingHost>,
        ),
        (Or<(With<PendingLobby>, With<Lobby>)>, Without<Host>),
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
    for (lobby_id, pinned_host, promoted, awaiting) in client_lobbies.iter() {
        let reaching_new_host = awaiting.is_some_and(|awaiting| awaiting.successor.is_some());
        if promoted && !reaching_new_host {
            continue;
        }
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

/// Despawn `remote`'s seat, and say whether it was a connected one: a `LobbyClient`, whose removal
/// takes its participant with it, rather than a seat still waiting for its first handshake.
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
) -> bool {
    let Some((client_entity, _, _, pending)) = lobby_clients
        .iter()
        .find(|(_, client_steam_id, _, _)| client_steam_id.0 == remote)
    else {
        return false;
    };
    commands.entity(client_entity).try_despawn();
    pending.is_none()
}

/// Tear down a client-side lobby this peer did not choose to leave, and say why.
///
/// Sessions first, then the Steam lobby, then the entity — the same order as
/// [`session::leave_lobby`], for the same reason. A promoted lobby ends with a [`LobbyLeft`];
/// one still pending never opened, so it ends with a [`LobbyJoinFailed`] instead.
///
/// The entity goes, and the reason is written, when the command is applied: the core can end the
/// same session in the same frame (the host's `LobbyClosed` arriving with Steam's word that the
/// host left), and only whichever is applied first says so.
fn tear_down_client_lobby(
    commands: &mut Commands,
    steam_client: &Client,
    (entity, lobby_id, host, promoted): (Entity, &LobbySteamId, &LobbyHostSteamId, bool),
    reason: LobbyLeftReason,
) {
    close_session(host.0);
    steam_client.matchmaking().leave_lobby(lobby_id.0);

    commands.queue(move |world: &mut World| {
        let Ok(lobby) = world.get_entity_mut(entity) else {
            return;
        };
        lobby.despawn();
        if promoted {
            world.write_message(LobbyLeft { reason });
        } else {
            let reason = match reason {
                LobbyLeftReason::Kicked => "the host removed this player before the session opened",
                LobbyLeftReason::HostGone => "the host left before the session opened",
                _ => "the lobby closed before the session opened",
            };
            world.write_message(LobbyJoinFailed {
                reason: reason.to_owned(),
            });
        }
    });
}

/// Lobby data a host sets when it closes its lobby, so members know not to take it over.
///
/// The core tells members over P2P too, but the host closes its sessions and leaves the Steam lobby
/// one frame later, which can drop the message; Steam then hands the lobby to a member. Lobby data
/// goes through Steam's servers ahead of the host's departure, so a member that sees the host go
/// can check it first.
pub(crate) const CLOSED_LOBBY_KEY: &str = "bevy_ensemble_closed";

fn lobby_is_closed(steam_client: &Client, lobby: LobbyId) -> bool {
    steam_client
        .matchmaking()
        .lobby_data(lobby, CLOSED_LOBBY_KEY)
        .is_some()
}

/// A lobby entity is going: whoever despawned it — this crate, the core's kick path, the game —
/// the host it named is no longer this peer's host, and the session-request callback must stop
/// admitting its members.
///
/// And this peer leaves the Steam lobby, whoever despawned it. The paths in this crate leave on
/// their own, but the core's do not know Steam exists: a client the core removed — kicked, timed
/// out on pings, refused on protocol — used to stay a member of the Steam lobby, still listed to
/// everyone, still admitted by their session callbacks, and a member Steam could hand the lobby
/// to. Leaving twice is harmless.
fn forget_lobby(
    trigger: On<Remove, LobbySteamId>,
    mut commands: Commands,
    steam_client: Res<Client>,
    current_lobby: Res<CurrentSteamLobby>,
    lobbies: Query<(&LobbySteamId, Option<&LobbyHostSteamId>)>,
) {
    if let Ok((lobby_id, host)) = lobbies.get(trigger.event_target()) {
        // The core's teardowns -- a host that never came back, a closed lobby -- leave the session
        // with the host open otherwise, until Steam times it out.
        if let Some(host) = host {
            close_session(host.0);
        }
        steam_client.matchmaking().leave_lobby(lobby_id.0);
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
    fn an_unchanged_owner_is_not_a_migration() {
        assert_eq!(owner_change(id(1), id(1), id(2)), OwnerChange::None);
    }

    #[test]
    fn an_owner_of_zero_names_nobody() {
        assert_eq!(owner_change(id(1), id(0), id(2)), OwnerChange::None);
        assert!(
            !replaced_as_host(id(0), id(2)),
            "a host whose lobby Steam has no owner for is not replaced by nobody"
        );
    }

    #[test]
    fn this_peer_named_owner_promotes() {
        assert_eq!(owner_change(id(1), id(2), id(2)), OwnerChange::Promote);
    }

    #[test]
    fn another_member_named_owner_is_followed() {
        assert_eq!(
            owner_change(id(1), id(3), id(2)),
            OwnerChange::Follow(id(3))
        );
    }

    #[test]
    fn after_repinning_only_the_new_owner_is_decoded() {
        let members = [id(2), id(3)];
        let OwnerChange::Follow(new_owner) = owner_change(id(1), id(3), id(2)) else {
            panic!("another member named owner is followed");
        };
        assert!(accept_packet(
            PeerRole::Client,
            Some(new_owner),
            &members,
            id(3)
        ));
        assert!(
            !accept_packet(PeerRole::Client, Some(new_owner), &members, id(1)),
            "the old host's packets are dropped once the pin moves"
        );
    }

    #[test]
    fn a_host_steam_names_someone_else_owner_of_was_replaced() {
        assert!(replaced_as_host(id(3), id(2)));
        assert!(!replaced_as_host(id(2), id(2)));
    }

    #[test]
    fn a_host_decodes_only_members() {
        let members = [id(1), id(2)];
        assert!(accept_packet(PeerRole::Host, None, &members, id(2)));
        assert!(!accept_packet(PeerRole::Host, None, &members, id(3)));
        assert!(!accept_packet(PeerRole::Host, None, &[], id(1)));
    }
}
