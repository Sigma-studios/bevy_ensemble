use bevy::prelude::*;
use bevy_ensemble::{
    Host, Lobby, LobbyClient, LobbyMessage, LobbyParticipant, LobbyParticipantOf,
    LocalMultiplayerPlayerId, EnsembleAppExt, PendingLobby, RemoveLobbyParticipant,
    RequestLobby, SerializedLobbyPacket, crate_steam_bridge::LobbyClientPlayerUuid,
    decode_ensemble_packet, encode_ensemble_message,
};
pub use bevy_steamworks::LobbyId;
use bevy_steamworks::{
    CallbackResult, ChatMemberStateChange, Client, FriendFlags, LobbyType, SendType, SteamId,
    SteamworksEvent, SteamworksPlugin,
};
use std::collections::HashSet;
use std::sync::{
    Mutex,
    mpsc::{Receiver, Sender, channel},
};

pub const DEFAULT_STEAM_APP_ID: u32 = 480;
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

#[derive(Resource)]
struct SteamLobbyCallbackChannel {
    sender: Sender<SteamLobbyCallback>,
    receiver: Mutex<Receiver<SteamLobbyCallback>>,
}

#[derive(Message, Debug)]
enum SteamLobbyCallback {
    Created { entity: Entity, lobby_id: LobbyId },
    CreateFailed { entity: Entity },
    Joined { entity: Entity, lobby_id: LobbyId },
    JoinFailed { entity: Entity },
}

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
        let (sender, receiver) = channel();

        app.add_plugins(
            SteamworksPlugin::init_app(self.app_id)
                .expect("Steamworks initialization plugin should build with a valid app id"),
        )
        .insert_resource(SteamLobbyCallbackChannel {
            sender,
            receiver: Mutex::new(receiver),
        })
        .add_message::<JoinSteamLobby>()
        .register_ensemble_message_type::<SteamReadyHandshake>()
        .add_message::<SteamLobbyCallback>()
        .add_systems(Startup, setup_join_policy)
        .add_systems(
            Update,
            (
                populate_friend_lobbies,
                create_lobby,
                join_requested_lobbies,
                react_to_events,
                flush_lobby_callback_channel,
                apply_lobby_callbacks,
                reconcile_host_lobby_membership,
                reconcile_client_lobby_connection,
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
        match host {
            Some(_) => {
                let local_steam_id = steam_client.user().steam_id();
                for client in steam_client.matchmaking().lobby_members(lobby_id.0) {
                    if client == local_steam_id {
                        continue;
                    }
                    steam_client.networking().send_p2p_packet(
                        client.into(),
                        SendType::Reliable,
                        &packet.packet,
                    );
                }
            }
            None => {
                let host = steam_client.matchmaking().lobby_owner(lobby_id.0);
                steam_client.networking().send_p2p_packet(
                    host.into(),
                    SendType::Reliable,
                    &packet.packet,
                );
            }
        }
        return;
    }

    if let Ok((lobby_id, host)) = pending_lobby_query.get(packet.entity) {
        match host {
            Some(_) => {
                let local_steam_id = steam_client.user().steam_id();
                for client in steam_client.matchmaking().lobby_members(lobby_id.0) {
                    if client == local_steam_id {
                        continue;
                    }
                    steam_client.networking().send_p2p_packet(
                        client.into(),
                        SendType::Reliable,
                        &packet.packet,
                    );
                }
            }
            None => {
                let host = steam_client.matchmaking().lobby_owner(lobby_id.0);
                steam_client.networking().send_p2p_packet(
                    host.into(),
                    SendType::Reliable,
                    &packet.packet,
                );
            }
        }
        return;
    }

    if let Ok(client_steam_id) = lobby_client_query.get(packet.entity) {
        steam_client.networking().send_p2p_packet(
            client_steam_id.0,
            SendType::Reliable,
            &packet.packet,
        );
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
            println!("Session request received: {:?}", res);
        });
    steam_client
        .networking_messages()
        .session_failed_callback(|args| {
            info!("Session failed: {:?}", args);
        });
}

fn create_lobby(
    steam_client: Res<Client>,
    lobby_callback_channel: Res<SteamLobbyCallbackChannel>,
    lobbies: Query<(Entity, Option<&Host>), Added<RequestLobby>>,
) {
    for (entity, maybe_host) in lobbies.iter() {
        if maybe_host.is_none() {
            continue;
        }
        let sender = lobby_callback_channel.sender.clone();
        steam_client.matchmaking().create_lobby(
            LobbyType::FriendsOnly,
            MAX_LOBBY_PLAYERS,
            move |lobby_id| {
                info!("Lobby created: {:?}", lobby_id);

                match lobby_id {
                    Ok(lobby_id) => {
                        info!("sending created lobby id back to Bevy: {:?}", lobby_id);
                        if let Err(error) =
                            sender.send(SteamLobbyCallback::Created { entity, lobby_id })
                        {
                            error!("Failed to send created lobby id back to Bevy: {:?}", error);
                        }
                    }
                    Err(error) => {
                        error!(
                            "Steam failed to create lobby for entity {:?}: {:?}",
                            entity, error
                        );
                        if let Err(callback_error) =
                            sender.send(SteamLobbyCallback::CreateFailed { entity })
                        {
                            error!(
                                "Failed to send create failure back to Bevy: {:?}",
                                callback_error
                            );
                        }
                    }
                }
            },
        );
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
    lobby_callback_channel: Res<SteamLobbyCallbackChannel>,
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

    let entity = commands.spawn(PendingLobby).id();

    let sender = lobby_callback_channel.sender.clone();
    steam_client
        .matchmaking()
        .join_lobby(join_request.0, move |res| {
            info!("Joined lobby: {:?}", res);

            match res {
                Ok(lobby_id) => {
                    if let Err(error) = sender.send(SteamLobbyCallback::Joined { entity, lobby_id })
                    {
                        error!("Failed to send joined lobby id back to Bevy: {:?}", error);
                    }
                }
                Err(error) => {
                    error!("Steam failed to join lobby: {:?}", error);
                    if let Err(callback_error) =
                        sender.send(SteamLobbyCallback::JoinFailed { entity })
                    {
                        error!(
                            "Failed to send join failure back to Bevy: {:?}",
                            callback_error
                        );
                    }
                }
            }
        });
}

fn flush_lobby_callback_channel(
    lobby_callback_channel: Res<SteamLobbyCallbackChannel>,
    mut lobby_callbacks: MessageWriter<SteamLobbyCallback>,
) {
    let Ok(receiver) = lobby_callback_channel.receiver.lock() else {
        error!("Failed to lock lobby creation receiver");
        return;
    };

    for lobby_callback in receiver.try_iter() {
        lobby_callbacks.write(lobby_callback);
    }
}

fn apply_lobby_callbacks(
    mut commands: Commands,
    steam_client: Res<Client>,
    mut lobby_callbacks: MessageReader<SteamLobbyCallback>,
) {
    for lobby_callback in lobby_callbacks.read() {
        match *lobby_callback {
            SteamLobbyCallback::Created { entity, lobby_id } => {
                commands.insert_resource(LocalMultiplayerPlayerId(u128::from(
                    steam_client.user().steam_id().raw(),
                )));
                commands
                    .entity(entity)
                    .remove::<(PendingLobby, RequestLobby)>()
                    .insert((Lobby, LobbySteamId(lobby_id)));
            }
            SteamLobbyCallback::CreateFailed { entity }
            | SteamLobbyCallback::JoinFailed { entity } => {
                commands.remove_resource::<LocalMultiplayerPlayerId>();
                if let Ok(mut entity_commands) = commands.get_entity(entity) {
                    entity_commands.try_despawn();
                }
            }
            SteamLobbyCallback::Joined { entity, lobby_id } => {
                commands.insert_resource(LocalMultiplayerPlayerId(u128::from(
                    steam_client.user().steam_id().raw(),
                )));
                commands.entity(entity).insert(LobbySteamId(lobby_id));
            }
        }
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
        steam_client
            .networking()
            .send_p2p_packet(host.into(), SendType::Reliable, &packet);
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
        steam_client
            .networking()
            .send_p2p_packet(remote.into(), SendType::Reliable, &packet);
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

fn react_to_events(
    mut commands: Commands,
    steam_client: Res<Client>,
    lobby_callback_channel: Res<SteamLobbyCallbackChannel>,
    mut events: MessageReader<SteamworksEvent>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    client_lobbies: Query<(Entity, &LobbySteamId), (With<Lobby>, Without<Host>)>,
    pending_client_lobbies: Query<(), (With<PendingLobby>, Without<Host>)>,
    lobby_clients: Query<
        (
            Entity,
            &LobbyClientSteamId,
            &LobbyClientPlayerUuid,
            Option<&PendingSteamLobbyClient>,
        ),
        Or<(With<LobbyClient>, With<PendingSteamLobbyClient>)>,
    >,
    pending_lobby_clients: Query<
        (Entity, &LobbyClientSteamId, &LobbyClientPlayerUuid),
        With<PendingSteamLobbyClient>,
    >,
    participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let local_steam_id = steam_client.user().steam_id();

    for event in events.read() {
        match event {
            SteamworksEvent::CallbackResult(event) => match event {
                CallbackResult::LobbyEnter(enter) => {
                    info!("Lobby entered: {:?}", enter.lobby);
                }
                CallbackResult::GameLobbyJoinRequested(request) => {
                    if !client_lobbies.is_empty() || !pending_client_lobbies.is_empty() {
                        warn!(
                            "Ignoring Steam overlay join request while a client lobby is already active or pending"
                        );
                        continue;
                    }
                    info!("Game lobby join requested: {:?}", request.lobby_steam_id);
                    let entity = commands.spawn(PendingLobby).id();
                    let sender = lobby_callback_channel.sender.clone();
                    steam_client
                        .matchmaking()
                        .join_lobby(request.lobby_steam_id, move |res| {
                            info!("Joined lobby: {:?}", res);

                            match res {
                                Ok(lobby_id) => {
                                    if let Err(error) =
                                        sender.send(SteamLobbyCallback::Joined { entity, lobby_id })
                                    {
                                        error!(
                                            "Failed to send joined lobby id back to Bevy: {:?}",
                                            error
                                        );
                                    }
                                }
                                Err(error) => {
                                    error!("Steam failed to join lobby: {:?}", error);
                                    if let Err(callback_error) =
                                        sender.send(SteamLobbyCallback::JoinFailed { entity })
                                    {
                                        error!(
                                            "Failed to send join failure back to Bevy: {:?}",
                                            callback_error
                                        );
                                    }
                                }
                            }
                        });
                }
                CallbackResult::LobbyDataUpdate(data) => {
                    info!(
                        "Lobby member count: {:?}",
                        steam_client.matchmaking().lobby_member_count(data.lobby)
                    );
                }
                CallbackResult::LobbyChatUpdate(update) => {
                    info!("Lobby chat updated: {:?}", update);

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
                                    &steam_client,
                                    &lobby_clients,
                                    &participants,
                                    **host_lobby.as_ref().unwrap(),
                                    update.user_changed,
                                );
                            }
                        }
                        ChatMemberStateChange::Entered => {}
                    }
                }
                CallbackResult::LobbyCreated(lobby) => {
                    info!("Lobby created2: {:?}", lobby);
                }
                CallbackResult::P2PSessionConnectFail(failed) => {
                    info!("P2P session failed: {:?}", failed);

                    if host_lobby.is_some() {
                        despawn_lobby_client_for_remote(
                            &mut commands,
                            &steam_client,
                            &lobby_clients,
                            &participants,
                            **host_lobby.as_ref().unwrap(),
                            failed.remote,
                        );
                    } else {
                        despawn_client_lobby_for_remote_owner(
                            &mut commands,
                            &steam_client,
                            &client_lobbies,
                            failed.remote,
                        );
                    }
                }
                CallbackResult::P2PSessionRequest(request) => {
                    let Some(lobby) = host_lobby.as_ref() else {
                        error!("Lobby not found to accept P2P session request");
                        continue;
                    };
                    steam_client.networking().accept_p2p_session(request.remote);
                    ensure_pending_lobby_client_for_remote(
                        &mut commands,
                        &pending_lobby_clients,
                        &lobby_clients,
                        **lobby,
                        request.remote,
                    );
                    info!("P2P session request: {:?}", request);
                }
                CallbackResult::PersonaStateChange(_) => {}
                CallbackResult::UserStatsReceived(_) => {}
                e => {
                    info!("Unhandled event: {:?}", e);
                }
            },
        }
    }
}

fn read_messages(world: &mut World) {
    let mut packets = Vec::new();

    {
        let steam_client = world.resource::<Client>();

        while let Some(size) = steam_client
            .networking()
            .is_p2p_packet_available_on_channel(0)
        {
            let mut buf = vec![0; size];

            let Some((steam_id, size)) = steam_client
                .networking()
                .read_p2p_packet_from_channel(&mut buf, 0)
            else {
                error!("No message received");
                continue;
            };

            buf.truncate(size);
            packets.push((steam_id, buf));
        }
    }

    for (steam_id, packet) in packets {
        ensure_pending_lobby_client_for_remote_world(world, steam_id);
        decode_ensemble_packet(world, Some(u128::from(steam_id.raw())), &packet);
    }
}

fn reconcile_host_lobby_membership(
    mut commands: Commands,
    steam_client: Res<Client>,
    host_lobby: Option<Single<(Entity, &LobbySteamId), (With<Lobby>, With<Host>)>>,
    lobby_clients: Query<
        (
            Entity,
            &LobbyClientSteamId,
            &LobbyClientPlayerUuid,
            Option<&PendingSteamLobbyClient>,
        ),
        Or<(With<LobbyClient>, With<PendingSteamLobbyClient>)>,
    >,
    participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let Some(host_lobby) = host_lobby else {
        return;
    };

    let local_steam_id = steam_client.user().steam_id();
    let current_members: HashSet<_> = steam_client
        .matchmaking()
        .lobby_members(host_lobby.1 .0)
        .into_iter()
        .filter(|member| *member != local_steam_id)
        .collect();

    let stale_remotes: Vec<_> = lobby_clients
        .iter()
        .filter_map(|(_, client_steam_id, _, _)| {
            (!current_members.contains(&client_steam_id.0)).then_some(client_steam_id.0)
        })
        .collect();

    for remote in stale_remotes {
        despawn_lobby_client_for_remote(
            &mut commands,
            &steam_client,
            &lobby_clients,
            &participants,
            host_lobby.0,
            remote,
        );
    }
}

fn reconcile_client_lobby_connection(
    mut commands: Commands,
    steam_client: Res<Client>,
    client_lobbies: Query<
        (Entity, &LobbySteamId),
        (
            Without<Host>,
            Or<(With<Lobby>, With<PendingLobby>)>,
        ),
    >,
) {
    let local_steam_id = steam_client.user().steam_id();

    let stale_lobbies: Vec<_> = client_lobbies
        .iter()
        .filter_map(|(lobby_entity, lobby_steam_id)| {
            let owner = steam_client.matchmaking().lobby_owner(lobby_steam_id.0);
            let members = steam_client.matchmaking().lobby_members(lobby_steam_id.0);
            let host_missing = owner == local_steam_id || !members.contains(&owner);
            host_missing.then_some((lobby_entity, lobby_steam_id.0, owner))
        })
        .collect();

    for (lobby_entity, lobby_id, owner) in stale_lobbies {
        if owner != local_steam_id {
            steam_client.networking().close_p2p_session(owner);
        }
        steam_client.matchmaking().leave_lobby(lobby_id);
        commands.entity(lobby_entity).try_despawn();
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
    pending_lobby_clients: &Query<
        (Entity, &LobbyClientSteamId, &LobbyClientPlayerUuid),
        With<PendingSteamLobbyClient>,
    >,
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
        })
        || pending_lobby_clients
            .iter()
            .any(|(_, client_steam_id, player_uuid)| {
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
    steam_client: &Client,
    lobby_clients: &Query<
        (
            Entity,
            &LobbyClientSteamId,
            &LobbyClientPlayerUuid,
            Option<&PendingSteamLobbyClient>,
        ),
        Or<(With<LobbyClient>, With<PendingSteamLobbyClient>)>,
    >,
    participants: &Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
    host_lobby: Entity,
    remote: SteamId,
) {
    if let Some((client_entity, _, player_uuid, _)) = lobby_clients
        .iter()
        .find(|(_, client_steam_id, _, _)| client_steam_id.0 == remote)
    {
        steam_client.networking().close_p2p_session(remote);
        commands
            .entity(host_lobby)
            .trigger(move |entity| LobbyMessage::<RemoveLobbyParticipant> {
                entity,
                message: RemoveLobbyParticipant {
                    player_uuid: player_uuid.0,
                },
            });
        if let Some((participant_entity, _, _)) =
            participants
                .iter()
                .find(|(_, participant, participant_of)| {
                    participant_of.0 == host_lobby && participant.player_uuid == player_uuid.0
                })
        {
            commands.entity(participant_entity).try_despawn();
        }
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
        let host = steam_client.matchmaking().lobby_owner(lobby_steam_id.0);
        steam_client.networking().close_p2p_session(host);
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
        steam_client.networking().close_p2p_session(remote);
        steam_client.matchmaking().leave_lobby(lobby_steam_id.0);
        commands.entity(lobby_entity).try_despawn();
    }
}
