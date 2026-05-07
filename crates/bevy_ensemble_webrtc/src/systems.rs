use bevy::prelude::*;
use bevy_ensemble::{
    Host, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyMessage, LobbyParticipant,
    LobbyParticipantOf, LocalMultiplayerPlayerId, PendingLobby, PublicLobbies, PublicLobbyInfo,
    RemoveLobbyParticipant, RequestLobby, SerializedLobbyPacket, decode_ensemble_packet,
    encode_ensemble_message,
};
use bevy_ensemble_sockets::PeerState;

use crate::connection::{LobbyConnection, LobbyEvent};
use crate::protocol::ClientMessage;
use crate::{
    JoinWebrtcLobby, JoinWebrtcLobbyByCode, LobbyClientWebrtcUuid, LobbyWebrtcCode, LobbyWebrtcId,
    PendingWebrtcLobbyClient, RefreshLobbyList, WebrtcReadyHandshake,
};

pub(crate) fn flush_lobby_events(
    lobby_conn: Res<LobbyConnection>,
    mut writer: MessageWriter<LobbyEvent>,
) {
    let Ok(mut rx) = lobby_conn.event_rx.lock() else {
        return;
    };

    while let Ok(event) = rx.try_recv() {
        writer.write(event);
    }
}

pub(crate) fn apply_lobby_events(
    mut commands: Commands,
    mut lobby_conn: ResMut<LobbyConnection>,
    mut socket: ResMut<crate::EnsembleSocketRes>,
    mut events: MessageReader<LobbyEvent>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    pending_host_lobbies: Query<Entity, (With<PendingLobby>, With<RequestLobby>, With<Host>)>,
    pending_client_lobbies: Query<Entity, (With<PendingLobby>, Without<Host>)>,
    active_client_lobbies: Query<Entity, (With<Lobby>, Without<Host>)>,
    lobby_clients: Query<
        (Entity, &LobbyClientWebrtcUuid, &LobbyClientPlayerUuid),
        Or<(With<LobbyClient>, With<PendingWebrtcLobbyClient>)>,
    >,
    participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    for event in events.read() {
        match event {
            LobbyEvent::Welcome { player_uuid } => {
                info!("Authenticated with signaling server, uuid: {player_uuid}");
                lobby_conn.local_player_uuid = Some(*player_uuid);
            }

            LobbyEvent::LobbyCreated { lobby_id, code } => {
                info!("Lobby created: {lobby_id} (code: {code})");
                let Some(player_uuid) = lobby_conn.local_player_uuid else {
                    continue;
                };
                commands.insert_resource(LocalMultiplayerPlayerId(player_uuid));

                if let Some(entity) = pending_host_lobbies.iter().next() {
                    commands
                        .entity(entity)
                        .remove::<(PendingLobby, RequestLobby)>()
                        .insert((Lobby, LobbyWebrtcId(*lobby_id), LobbyWebrtcCode(code.clone())));
                }
            }

            LobbyEvent::LobbyJoined { lobby_id } => {
                info!("Joined lobby: {lobby_id}");
                let Some(player_uuid) = lobby_conn.local_player_uuid else {
                    continue;
                };
                commands.insert_resource(LocalMultiplayerPlayerId(player_uuid));

                if let Some(entity) = pending_client_lobbies.iter().next() {
                    commands.entity(entity).insert(LobbyWebrtcId(*lobby_id));
                }
            }

            LobbyEvent::LobbyError { reason } => {
                error!("Lobby error: {reason}");
                commands.remove_resource::<LocalMultiplayerPlayerId>();
                for entity in pending_host_lobbies.iter() {
                    commands.entity(entity).try_despawn();
                }
                for entity in pending_client_lobbies.iter() {
                    commands.entity(entity).try_despawn();
                }
            }

            LobbyEvent::PlayerJoined { player_uuid } => {
                let player_uuid = *player_uuid;
                info!("Player joined lobby: {player_uuid}");
                let Some(lobby) = host_lobby.as_ref() else {
                    continue;
                };

                let already_known = lobby_clients
                    .iter()
                    .any(|(_, _, puuid)| puuid.0 == player_uuid);
                if already_known {
                    continue;
                }

                // Initiate WebRTC connection to the new peer
                socket.connect_peer(player_uuid);

                commands.spawn((
                    PendingWebrtcLobbyClient,
                    LobbyParticipantOf(**lobby),
                    LobbyClientWebrtcUuid(player_uuid),
                    LobbyClientPlayerUuid(player_uuid),
                ));
            }

            LobbyEvent::PlayerLeft { player_uuid } => {
                let player_uuid = *player_uuid;
                info!("Player left lobby: {player_uuid}");

                // Disconnect the WebRTC peer
                socket.disconnect_peer(player_uuid);

                if let Some(lobby) = host_lobby.as_ref() {
                    let host_entity = **lobby;

                    if let Some((client_entity, _, _)) = lobby_clients
                        .iter()
                        .find(|(_, _, puuid)| puuid.0 == player_uuid)
                    {
                        commands
                            .entity(host_entity)
                            .trigger(move |entity| LobbyMessage::<RemoveLobbyParticipant> {
                                entity,
                                message: RemoveLobbyParticipant {
                                    player_uuid,
                                },
                            });

                        if let Some((participant_entity, _, _)) =
                            participants.iter().find(|(_, p, pof)| {
                                pof.0 == host_entity && p.player_uuid == player_uuid
                            })
                        {
                            commands.entity(participant_entity).try_despawn();
                        }

                        commands.entity(client_entity).try_despawn();
                    }
                }
            }

            LobbyEvent::Disconnected { reason } => {
                info!("Disconnected from lobby: {reason}");
                for entity in pending_client_lobbies.iter() {
                    commands.entity(entity).try_despawn();
                }
                for entity in active_client_lobbies.iter() {
                    for (participant_entity, _, pof) in participants.iter() {
                        if pof.0 == entity {
                            commands.entity(participant_entity).try_despawn();
                        }
                    }
                    commands.entity(entity).try_despawn();
                }
                commands.remove_resource::<LocalMultiplayerPlayerId>();
            }

            LobbyEvent::LobbyList { lobbies } => {
                commands.insert_resource(PublicLobbies(
                    lobbies
                        .iter()
                        .map(|l| PublicLobbyInfo {
                            lobby_id: l.lobby_id,
                            code: l.code.clone(),
                            host_name: l.host_name.clone(),
                            player_count: l.player_count,
                            max_players: l.max_players,
                        })
                        .collect(),
                ));
            }
        }
    }
}

pub(crate) fn create_lobby(
    lobby_conn: Res<LobbyConnection>,
    webrtc_runtime: Res<crate::WebrtcRuntime>,
    lobbies: Query<(Entity, Option<&Host>), Added<RequestLobby>>,
) {
    for (_entity, maybe_host) in lobbies.iter() {
        if maybe_host.is_none() {
            continue;
        }
        let _ = lobby_conn.command_tx.send(ClientMessage::CreateLobby {
            max_players: webrtc_runtime.max_players,
        });
    }
}

pub(crate) fn join_requested_lobbies(
    mut commands: Commands,
    lobby_conn: Res<LobbyConnection>,
    mut join_requests: MessageReader<JoinWebrtcLobby>,
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

    let _ = lobby_conn
        .command_tx
        .send(ClientMessage::JoinLobby {
            lobby_id: join_request.0,
        });
}

pub(crate) fn join_requested_lobbies_by_code(
    mut commands: Commands,
    lobby_conn: Res<LobbyConnection>,
    mut join_requests: MessageReader<JoinWebrtcLobbyByCode>,
    existing_client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
    pending_client_lobbies: Query<(), (With<PendingLobby>, Without<Host>)>,
) {
    let Some(join_request) = join_requests.read().last().cloned() else {
        return;
    };
    if !existing_client_lobbies.is_empty() || !pending_client_lobbies.is_empty() {
        warn!("Ignoring join request while a client lobby is already active or pending");
        return;
    }

    commands.spawn(PendingLobby);

    let _ = lobby_conn
        .command_tx
        .send(ClientMessage::JoinLobbyByCode {
            code: join_request.0,
        });
}

pub(crate) fn refresh_lobby_list(
    lobby_conn: Res<LobbyConnection>,
    mut requests: MessageReader<RefreshLobbyList>,
) {
    if requests.read().next().is_some() {
        let _ = lobby_conn.command_tx.send(ClientMessage::ListLobbies);
    }
}

/// Poll the EnsembleSocket for peer connect/disconnect events.
/// Despawns lobby client entities when a peer disconnects.
pub(crate) fn poll_socket_peers(
    mut commands: Commands,
    mut socket: ResMut<crate::EnsembleSocketRes>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    lobby_clients: Query<
        (Entity, &LobbyClientWebrtcUuid, &LobbyClientPlayerUuid),
        Or<(With<LobbyClient>, With<PendingWebrtcLobbyClient>)>,
    >,
    participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    for (peer_id, state) in socket.update_peers() {
        match state {
            PeerState::Connected => {
                info!("Peer connected: {peer_id}");
            }
            PeerState::Disconnected => {
                info!("Peer disconnected: {peer_id}");

                let Some(lobby) = host_lobby.as_ref() else {
                    continue;
                };
                let host_entity = **lobby;

                if let Some((client_entity, _, player_uuid)) = lobby_clients
                    .iter()
                    .find(|(_, client_uuid, _)| client_uuid.0 == peer_id)
                {
                    commands
                        .entity(host_entity)
                        .trigger(move |entity| LobbyMessage::<RemoveLobbyParticipant> {
                            entity,
                            message: RemoveLobbyParticipant {
                                player_uuid: player_uuid.0,
                            },
                        });

                    if let Some((participant_entity, _, _)) =
                        participants.iter().find(|(_, p, pof)| {
                            pof.0 == host_entity && p.player_uuid == player_uuid.0
                        })
                    {
                        commands.entity(participant_entity).try_despawn();
                    }

                    commands.entity(client_entity).try_despawn();
                }
            }
        }
    }
}

/// Each frame, pump signals between the WS handler and the EnsembleSocket:
/// 1. Drain incoming signals from LobbyConnection's signal_rx and feed them to socket.receive_signal()
/// 2. Drain outbound signals from socket.drain_signals() and send them as ClientMessage::Signal
pub(crate) fn pump_socket_signals(
    mut socket: ResMut<crate::EnsembleSocketRes>,
    lobby_conn: Res<LobbyConnection>,
) {
    if let Ok(mut signal_rx) = lobby_conn.signal_rx.lock() {
        while let Ok((sender, signal)) = signal_rx.try_recv() {
            socket.receive_signal(sender, signal);
        }
    }

    for outgoing in socket.drain_signals() {
        let data = serde_json::to_string(&outgoing.signal)
            .expect("Failed to serialize PeerSignal");
        let _ = lobby_conn.command_tx.send(ClientMessage::Signal {
            receiver_uuid: outgoing.peer,
            data,
        });
    }
}

/// Detects when a lobby entity with a server-assigned ID is despawned.
/// Sends LeaveLobby to the server, disconnects all peers, then
/// rebuilds the WS connection.
pub(crate) fn detect_lobby_leave(
    mut commands: Commands,
    lobby_conn: Res<LobbyConnection>,
    mut socket: ResMut<crate::EnsembleSocketRes>,
    webrtc_runtime: Res<crate::WebrtcRuntime>,
    mut removed: RemovedComponents<LobbyWebrtcId>,
) {
    for _entity in removed.read() {
        info!("Lobby entity removed, sending LeaveLobby and rebuilding connection");

        // Send LeaveLobby on the current connection
        let _ = lobby_conn.command_tx.send(ClientMessage::LeaveLobby);

        // Disconnect all WebRTC peers
        socket.disconnect_all();

        // Rebuild the WS connection from scratch.
        // The old WS task will naturally exit when its channels are dropped.
        let (new_socket, lobby_connection) = webrtc_runtime.build_socket();
        commands.insert_resource(new_socket);
        commands.insert_resource(lobby_connection);
    }
}

pub(crate) fn send_serialized_lobby_packet(
    packet: On<SerializedLobbyPacket>,
    socket: ResMut<crate::EnsembleSocketRes>,
    lobby_query: Query<(Option<&Host>, Option<&LobbyWebrtcId>), With<Lobby>>,
    pending_lobby_query: Query<
        (Option<&Host>, Option<&LobbyWebrtcId>),
        (With<PendingLobby>, Without<Lobby>),
    >,
    lobby_client_query: Query<&LobbyClientWebrtcUuid>,
) {
    let data: Box<[u8]> = packet.packet.clone().into_boxed_slice();

    // Triggered on a lobby entity (active)
    if let Ok((host, _)) = lobby_query.get(packet.entity) {
        let peers: Vec<u128> = socket.connected_peers().collect();
        match host {
            Some(_) => {
                for peer in peers {
                    socket.send(data.clone(), peer);
                }
            }
            None => {
                if let Some(&peer) = peers.first() {
                    socket.send(data, peer);
                }
            }
        }
        return;
    }

    // Triggered on a pending lobby entity
    if let Ok((host, _)) = pending_lobby_query.get(packet.entity) {
        let peers: Vec<u128> = socket.connected_peers().collect();
        match host {
            Some(_) => {
                for peer in peers {
                    socket.send(data.clone(), peer);
                }
            }
            None => {
                if let Some(&peer) = peers.first() {
                    socket.send(data, peer);
                }
            }
        }
        return;
    }

    // Triggered on a LobbyClient entity (targeted send)
    if let Ok(client_uuid) = lobby_client_query.get(packet.entity) {
        socket.send(data, client_uuid.0);
        return;
    }

    error!(
        "Serialized lobby packet was triggered for unsupported entity {:?}",
        packet.entity
    );
}

pub(crate) fn read_peer_messages(world: &mut World) {
    let mut packets = Vec::new();

    {
        let mut socket = world.resource_mut::<crate::EnsembleSocketRes>();
        for (sender_uuid, packet) in socket.receive() {
            packets.push((sender_uuid, packet.to_vec()));
        }
    }

    for (sender_uuid, payload) in packets {
        if !decode_ensemble_packet(world, Some(sender_uuid), &payload) {
            warn!("Failed to decode ensemble packet from peer {sender_uuid}");
        }
    }
}

/// Sends client handshakes to the host peer.
///
/// IMPORTANT: This must keep sending even after the client lobby is promoted from
/// `PendingLobby` to `Lobby`. The host creates its `PendingWebrtcLobbyClient` entity
/// from the `PlayerJoined` signaling event, which can arrive AFTER the host's own
/// handshake has already promoted the client. If we stop sending here, the host may
/// never receive a client handshake to promote with.
pub(crate) fn send_client_handshakes(
    registry: Res<bevy_ensemble::EnsembleMessageRegistry>,
    socket: ResMut<crate::EnsembleSocketRes>,
    client_lobbies: Query<
        &LobbyWebrtcId,
        (Without<Host>, Or<(With<PendingLobby>, With<Lobby>)>),
    >,
    time: Res<Time>,
    mut cooldown: Local<f32>,
) {
    *cooldown -= time.delta_secs();
    if *cooldown > 0.0 {
        return;
    }
    *cooldown = 0.5;

    if client_lobbies.is_empty() {
        return;
    }

    let packet = encode_ensemble_message(&registry, &WebrtcReadyHandshake { from_host: false });
    let data: Box<[u8]> = packet.into_boxed_slice();

    let peers: Vec<u128> = socket.connected_peers().collect();
    for peer in peers {
        socket.send(data.clone(), peer);
    }
}

pub(crate) fn send_host_handshakes(
    registry: Res<bevy_ensemble::EnsembleMessageRegistry>,
    socket: ResMut<crate::EnsembleSocketRes>,
    host_lobbies: Query<&LobbyWebrtcId, (With<Lobby>, With<Host>)>,
    time: Res<Time>,
    mut cooldown: Local<f32>,
) {
    *cooldown -= time.delta_secs();
    if *cooldown > 0.0 {
        return;
    }
    *cooldown = 0.5;

    if host_lobbies.is_empty() {
        return;
    }

    let packet = encode_ensemble_message(&registry, &WebrtcReadyHandshake { from_host: true });
    let data: Box<[u8]> = packet.into_boxed_slice();

    let peers: Vec<u128> = socket.connected_peers().collect();
    for peer in peers {
        socket.send(data.clone(), peer);
    }
}

pub(crate) fn promote_client_lobby_on_host_handshake(
    mut commands: Commands,
    mut messages: MessageReader<bevy_ensemble::ReceivedEnsembleMessage<WebrtcReadyHandshake>>,
    pending_client_lobbies: Query<Entity, (With<PendingLobby>, Without<Lobby>, Without<Host>)>,
) {
    for message in messages.read() {
        if !message.message.from_host {
            continue;
        }

        let Some(entity) = pending_client_lobbies.iter().next() else {
            continue;
        };

        commands
            .entity(entity)
            .remove::<PendingLobby>()
            .insert(Lobby);
    }
}

pub(crate) fn promote_host_client_on_client_handshake(
    mut commands: Commands,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    mut messages: MessageReader<bevy_ensemble::ReceivedEnsembleMessage<WebrtcReadyHandshake>>,
    pending_clients: Query<
        (Entity, &LobbyClientPlayerUuid, &LobbyParticipantOf),
        (With<PendingWebrtcLobbyClient>, With<LobbyClientWebrtcUuid>),
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
            .remove::<PendingWebrtcLobbyClient>()
            .insert(LobbyClient);
    }
}
