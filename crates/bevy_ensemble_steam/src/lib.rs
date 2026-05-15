use bevy::prelude::*;
use bevy_ensemble::{
    EnsembleAppExt, Host, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyParticipantOf,
    LocalMultiplayerPlayerId, PendingLobby, RequestLobby, SerializedLobbyPacket,
    decode_ensemble_packet, encode_ensemble_message,
};
pub use bevy_steamworks::LobbyId;
use bevy_steamworks::{
    CallbackResult, ChatMemberStateChange, ChatRoomEnterResponse, Client, FriendFlags, LobbyType,
    SteamId, SteamworksEvent, SteamworksPlugin,
    networking_types::{NetworkingIdentity, SendFlags},
};
use std::collections::HashSet;

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

#[derive(Clone, Debug)]
pub struct SteamFriendLobbySummary {
    pub lobby_id: LobbyId,
    pub host_name: String,
    pub member_count: usize,
}

#[derive(Resource, Clone, Debug, Default)]
pub struct SteamFriendLobbies(pub Vec<SteamFriendLobbySummary>);

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
        app.add_plugins(
            SteamworksPlugin::init_app(self.app_id)
                .expect("Steamworks initialization plugin should build with a valid app id"),
        )
        .add_message::<JoinSteamLobby>()
        .register_ensemble_message_type::<SteamReadyHandshake>()
        .add_systems(Startup, setup_join_policy)
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
                read_messages,
            ),
        )
        .add_observer(send_serialized_lobby_packet);
    }
}

fn send_message(steam_client: &Client, target: SteamId, data: &[u8]) {
    if let Err(e) = steam_client.networking_messages().send_message_to_user(
        NetworkingIdentity::new_steam_id(target),
        SendFlags::RELIABLE,
        data,
        0,
    ) {
        error!("Failed to send message to {:?}: {:?}", target, e);
    }
}

fn send_to_lobby(
    steam_client: &Client,
    lobby_id: &LobbySteamId,
    host: Option<&Host>,
    packet: &[u8],
) {
    match host {
        Some(_) => {
            let local_steam_id = steam_client.user().steam_id();
            for client in steam_client.matchmaking().lobby_members(lobby_id.0) {
                if client != local_steam_id {
                    send_message(steam_client, client, packet);
                }
            }
        }
        None => {
            let host = steam_client.matchmaking().lobby_owner(lobby_id.0);
            send_message(steam_client, host, packet);
        }
    }
}

fn send_serialized_lobby_packet(
    packet: On<SerializedLobbyPacket>,
    steam_client: Res<Client>,
    lobby_query: Query<(&LobbySteamId, Option<&Host>), With<Lobby>>,
    pending_lobby_query: Query<
        (&LobbySteamId, Option<&Host>),
        (With<PendingLobby>, Without<Lobby>),
    >,
    lobby_client_query: Query<&LobbyClientSteamId>,
) {
    if let Ok((lobby_id, host)) = lobby_query.get(packet.entity) {
        send_to_lobby(&steam_client, lobby_id, host, &packet.packet);
        return;
    }

    if let Ok((lobby_id, host)) = pending_lobby_query.get(packet.entity) {
        send_to_lobby(&steam_client, lobby_id, host, &packet.packet);
        return;
    }

    if let Ok(client_steam_id) = lobby_client_query.get(packet.entity) {
        send_message(&steam_client, client_steam_id.0, &packet.packet);
        return;
    }

    error!(
        "Serialized lobby packet was triggered for unsupported entity {:?}",
        packet.entity
    );
}

fn setup_join_policy(steam_client: Res<Client>) {
    info!("Setting up join policy");
    steam_client
        .networking_messages()
        .session_request_callback(|args| {
            let res = args.accept();
            info!("Session request received: {:?}", res);
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
) {
    if existing_list.is_some() {
        return;
    }

    let current_app_id = steam_client.utils().app_id();
    let mut seen_lobbies = HashSet::new();
    let mut lobbies = Vec::new();

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

        lobbies.push(SteamFriendLobbySummary {
            lobby_id,
            host_name: friend.name(),
            member_count: steam_client.matchmaking().lobby_member_count(lobby_id),
        });
    }

    commands.insert_resource(SteamFriendLobbies(lobbies));
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

fn react_to_events(
    mut commands: Commands,
    steam_client: Res<Client>,
    mut events: MessageReader<SteamworksEvent>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    pending_host_lobbies: Query<Entity, (With<RequestLobby>, With<Host>)>,
    client_lobbies: Query<(Entity, &LobbySteamId), (With<Lobby>, Without<Host>)>,
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
                        commands.remove_resource::<LocalMultiplayerPlayerId>();
                        continue;
                    }
                    commands.insert_resource(LocalMultiplayerPlayerId(u128::from(
                        local_steam_id.raw(),
                    )));
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
                            commands.insert_resource(LocalMultiplayerPlayerId(u128::from(
                                local_steam_id.raw(),
                            )));
                            commands.entity(entity).insert(LobbySteamId(enter.lobby));
                        }
                        other => {
                            error!("Failed to enter lobby: {:?}", other);
                            commands.entity(entity).try_despawn();
                            commands.remove_resource::<LocalMultiplayerPlayerId>();
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
                                despawn_client_lobby_for_lost_connection(
                                    &mut commands,
                                    &steam_client,
                                    &client_lobbies,
                                    update.lobby,
                                );
                            } else if host_lobby.is_some() {
                                despawn_lobby_client_for_remote(
                                    &mut commands,
                                    &lobby_clients,
                                    update.user_changed,
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
                        despawn_lobby_client_for_remote(
                            &mut commands,
                            &lobby_clients,
                            remote,
                        );
                    } else {
                        despawn_client_lobby_for_remote_owner(
                            &mut commands,
                            &steam_client,
                            &client_lobbies,
                            remote,
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

fn read_messages(world: &mut World) {
    let packets: Vec<_> = {
        let steam_client = world.resource::<Client>();
        steam_client
            .networking_messages()
            .receive_messages_on_channel(0, 64)
            .into_iter()
            .filter_map(|msg| {
                let steam_id = msg.identity_peer().steam_id()?;
                Some((steam_id, msg.data().to_vec()))
            })
            .collect()
    };

    for (steam_id, packet) in packets {
        ensure_pending_lobby_client_for_remote_world(world, steam_id);
        decode_ensemble_packet(world, Some(u128::from(steam_id.raw())), &packet);
    }
}

fn send_client_handshakes(
    steam_client: Res<Client>,
    registry: Res<bevy_ensemble::EnsembleMessageRegistry>,
    pending_client_lobbies: Query<
        &LobbySteamId,
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
    for lobby_id in pending_client_lobbies.iter() {
        let host = steam_client.matchmaking().lobby_owner(lobby_id.0);
        send_message(&steam_client, host, &packet);
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
        send_message(&steam_client, remote, &packet);
    }
}

fn promote_client_lobby_on_host_handshake(
    mut commands: Commands,
    steam_client: Res<Client>,
    mut messages: MessageReader<bevy_ensemble::ReceivedEnsembleMessage<SteamReadyHandshake>>,
    pending_client_lobbies: Query<
        (Entity, &LobbySteamId),
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

        let Some((entity, _lobby_steam_id)) =
            pending_client_lobbies.iter().find(|(_, lobby_steam_id)| {
                u128::from(
                    steam_client
                        .matchmaking()
                        .lobby_owner(lobby_steam_id.0)
                        .raw(),
                ) == sender
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


fn despawn_client_lobby_for_lost_connection(
    commands: &mut Commands,
    steam_client: &Client,
    client_lobbies: &Query<(Entity, &LobbySteamId), (With<Lobby>, Without<Host>)>,
    lost_lobby_id: LobbyId,
) {
    if let Some((lobby_entity, lobby_steam_id)) = client_lobbies
        .iter()
        .find(|(_, lobby_steam_id)| lobby_steam_id.0 == lost_lobby_id)
    {
        steam_client.matchmaking().leave_lobby(lobby_steam_id.0);
        commands.entity(lobby_entity).try_despawn();
    }
}

fn despawn_client_lobby_for_remote_owner(
    commands: &mut Commands,
    steam_client: &Client,
    client_lobbies: &Query<(Entity, &LobbySteamId), (With<Lobby>, Without<Host>)>,
    remote: SteamId,
) {
    if let Some((lobby_entity, lobby_steam_id)) =
        client_lobbies.iter().find(|(_, lobby_steam_id)| {
            steam_client.matchmaking().lobby_owner(lobby_steam_id.0) == remote
        })
    {
        steam_client.matchmaking().leave_lobby(lobby_steam_id.0);
        commands.entity(lobby_entity).try_despawn();
    }
}
