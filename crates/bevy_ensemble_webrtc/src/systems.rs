use bevy::prelude::*;
use bevy_ensemble::{
    Host, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyJoinFailed, LobbyParticipant,
    LobbyParticipantOf, LocalMultiplayerPlayerId, PendingLobby, PublicLobbies, PublicLobbyInfo,
    RemoveLobbyParticipant, RequestLobby, SerializedLobbyPacket, decode_ensemble_packet,
    encode_ensemble_message,
};
use bevy_ensemble_sockets::{PeerSignal, PeerState};

use crate::connection::{LobbyConnection, LobbyEvent};
use crate::protocol::ClientMessage;

use crate::{
    JoinWebrtcLobby, JoinWebrtcLobbyByCode, LobbyClientWebrtcUuid, LobbyWebrtcCode, LobbyWebrtcId,
    PendingWebrtcLobbyClient, RefreshLobbyList, SignallingDisplayName,
};

/// Send the display name to the signalling server whenever it changes.
///
/// On an edge rather than every frame, and skipping the first — the name the plugin was built with
/// already went out in `Authenticate`, so re-sending it at startup would be a redundant frame for
/// every peer that never touches its name.
///
/// There is no retry, and the reason is the channel rather than optimism: this is the WebSocket to
/// the signalling server, which is TCP. If it is up the message arrives; if it is not, this peer
/// has no listing to be wrong in.
pub(crate) fn publish_display_name(
    name: Res<SignallingDisplayName>,
    lobby_conn: Res<LobbyConnection>,
    mut sent: Local<Option<String>>,
) {
    if sent.is_none() {
        *sent = Some(name.0.clone());
        return;
    }
    if sent.as_deref() == Some(name.0.as_str()) {
        return;
    }
    let _ = lobby_conn.command_tx.send(ClientMessage::SetDisplayName {
        display_name: name.0.clone(),
    });
    *sent = Some(name.0.clone());
}

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
                    warn!(
                        "the server created lobby {lobby_id} before it said `Welcome`; no local \
                         uuid to host under, so this lobby is being left unclaimed"
                    );
                    continue;
                };
                commands.insert_resource(LocalMultiplayerPlayerId(player_uuid));

                if let Some(entity) = pending_host_lobbies.iter().next() {
                    commands
                        .entity(entity)
                        .remove::<(PendingLobby, RequestLobby)>()
                        .insert((
                            Lobby,
                            LobbyWebrtcId(*lobby_id),
                            LobbyWebrtcCode(code.clone()),
                        ));
                } else {
                    warn!(
                        "lobby {lobby_id} was created with no pending host lobby to promote. \
                         The lobby exists on the server and this peer is not in it."
                    );
                }
            }

            LobbyEvent::LobbyJoined { lobby_id } => {
                info!("Joined lobby: {lobby_id}");
                let Some(player_uuid) = lobby_conn.local_player_uuid else {
                    warn!(
                        "joined lobby {lobby_id} before the server said `Welcome`; no local uuid \
                         to play under, so the join is being dropped"
                    );
                    continue;
                };
                commands.insert_resource(LocalMultiplayerPlayerId(player_uuid));

                if let Some(entity) = pending_client_lobbies.iter().next() {
                    commands.entity(entity).insert(LobbyWebrtcId(*lobby_id));
                } else {
                    warn!(
                        "joined lobby {lobby_id} with no pending client lobby to attach it to. \
                         The join succeeded on the server and nothing here is holding it."
                    );
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
                // Not merely a missing lobby: `Single` also yields `None` when *two* entities
                // match. Either way the peer is never connected to, and the joiner is left
                // waiting on a data channel this side never opens -- with, until this line
                // existed, the `info!` above as the only trace, which reads exactly like a join
                // that worked.
                let Some(lobby) = host_lobby.as_ref() else {
                    warn!(
                        "dropping the join of {player_uuid}: this peer has no single hosted \
                         lobby to attach them to, so no connection to them will be opened"
                    );
                    continue;
                };

                let already_known = lobby_clients
                    .iter()
                    .any(|(_, _, puuid)| puuid.0 == player_uuid);
                if already_known {
                    debug!("{player_uuid} is already a known client; not connecting twice");
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

                if host_lobby.is_some() {
                    if let Some((client_entity, _, _)) = lobby_clients
                        .iter()
                        .find(|(_, _, puuid)| puuid.0 == player_uuid)
                    {
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

    let _ = lobby_conn.command_tx.send(ClientMessage::JoinLobby {
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

    let _ = lobby_conn.command_tx.send(ClientMessage::JoinLobbyByCode {
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
///
/// - On the **host**: despawns lobby client entities when a peer disconnects.
/// - On a **client**: despawns the lobby entity when the host peer disconnects
///   (e.g. kicked or host left), which triggers the full leave/cleanup flow.
pub(crate) fn poll_socket_peers(
    mut commands: Commands,
    mut socket: ResMut<crate::EnsembleSocketRes>,
    mut join_failed: MessageWriter<LobbyJoinFailed>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    client_lobbies: Query<
        (Entity, Has<PendingLobby>),
        (Or<(With<Lobby>, With<PendingLobby>)>, Without<Host>),
    >,
    lobby_clients: Query<
        (Entity, &LobbyClientWebrtcUuid),
        Or<(With<LobbyClient>, With<PendingWebrtcLobbyClient>)>,
    >,
) {
    for (peer_id, state) in socket.update_peers() {
        match state {
            PeerState::Connected => {
                info!("Peer connected: {peer_id}");
            }
            // The same teardown either way -- what differs is what it means and who is told.
            // `Failed` is a connection that never opened, so if the lobby is still pending this
            // is a join that will not be completing, and somebody is watching a screen that
            // would otherwise never change.
            PeerState::Disconnected | PeerState::Failed => {
                let failed = state == PeerState::Failed;
                if failed {
                    warn!("Peer {peer_id} could not be connected to");
                } else {
                    info!("Peer disconnected: {peer_id}");
                }

                // Host side: one player is gone, and the lobby carries on without them. Tearing
                // it down here would evict everybody already playing over one arrival that could
                // not get in.
                if host_lobby.is_some() {
                    if let Some((client_entity, _)) = lobby_clients
                        .iter()
                        .find(|(_, client_uuid)| client_uuid.0 == peer_id)
                    {
                        commands.entity(client_entity).try_despawn();
                    }
                    if failed {
                        join_failed.write(LobbyJoinFailed {
                            reason: format!("A player could not connect to this lobby ({peer_id})"),
                        });
                    }
                    continue;
                }

                // Client side: that peer was the whole session.
                let mut was_joining = false;
                for (entity, pending) in client_lobbies.iter() {
                    was_joining |= pending;
                    commands.entity(entity).try_despawn();
                }
                commands.remove_resource::<LocalMultiplayerPlayerId>();
                if failed {
                    join_failed.write(LobbyJoinFailed {
                        reason: if was_joining {
                            "Could not connect to the host. Their network or yours is refusing \
                             the connection."
                                .into()
                        } else {
                            "Lost the connection to the host.".into()
                        },
                    });
                }
            }
        }
    }
}

/// When a lobby was first seen still waiting to be promoted, so a join can be given up on.
///
/// Inserted here rather than where the lobby is spawned because both kinds arrive from elsewhere:
/// a client's from a join request in this crate, a host's from `bevy_ensemble` itself. Noticing
/// them is uniform; where they came from is not.
#[derive(Component)]
pub(crate) struct PendingSince(f64);

/// Give up on a lobby that has been about to happen for too long.
///
/// The backstop under [`poll_socket_peers`], for the failures that report nothing at all: an
/// offer that never arrives, a signalling server that accepts a join and goes quiet, a relay that
/// black-holes. WebRTC reports `Failed` for a connection it actually attempted; there is no event
/// for one that was never attempted, and that case used to be indistinguishable from a slow
/// network for ever.
///
/// Despawning is the whole action, and it is enough: the lobby's removal is what tells the
/// signalling server, rebuilds the socket, releases the ticked role and lets the game's own
/// teardown run. [`LobbyJoinFailed`] carries the reason for whatever wants to say so on screen.
pub(crate) fn time_out_pending_lobbies(
    mut commands: Commands,
    time: Res<Time>,
    runtime: Res<crate::WebrtcRuntime>,
    mut join_failed: MessageWriter<LobbyJoinFailed>,
    pending: Query<(Entity, Option<&PendingSince>, Has<Host>), (With<PendingLobby>, Without<Lobby>)>,
) {
    let Some(deadline) = runtime.join_timeout else {
        return;
    };
    let now = time.elapsed_secs_f64();
    for (entity, since, is_host) in pending.iter() {
        let Some(since) = since else {
            commands.entity(entity).try_insert(PendingSince(now));
            continue;
        };
        if now - since.0 < deadline.as_secs_f64() {
            continue;
        }
        let what = if is_host { "host a lobby" } else { "join a lobby" };
        warn!(
            "giving up after {:.0}s: the attempt to {what} never completed",
            deadline.as_secs_f64()
        );
        join_failed.write(LobbyJoinFailed {
            reason: if is_host {
                "Could not open a lobby. The signalling server did not answer.".into()
            } else {
                "Could not join. The lobby never finished connecting.".into()
            },
        });
        commands.entity(entity).despawn();
    }
}

/// Each frame, pump signals between the WS handler and the EnsembleSocket:
/// 1. Drain incoming signals from LobbyConnection's signal_rx and feed them to socket.receive_signal()
/// 2. Drain outbound signals from socket.drain_signals() and send them as ClientMessage::Signal
/// What kind of signal this is, for a log line. The bodies are an SDP blob or a candidate line,
/// neither of which belongs in a log; which of the three it is, and who it is for, is the part
/// that answers questions.
fn signal_kind(signal: &PeerSignal) -> &'static str {
    match signal {
        PeerSignal::Offer(_) => "offer",
        PeerSignal::Answer(_) => "answer",
        PeerSignal::IceCandidate(_) => "candidate",
    }
}

pub(crate) fn pump_socket_signals(
    mut socket: ResMut<crate::EnsembleSocketRes>,
    lobby_conn: Res<LobbyConnection>,
) {
    if let Ok(mut signal_rx) = lobby_conn.signal_rx.lock() {
        while let Ok((sender, signal)) = signal_rx.try_recv() {
            // Both directions are logged, at the one seam every signal crosses, because the
            // interesting failures are asymmetric: a peer whose offer and answer both arrive
            // while its candidates do not is a different bug from one that never sends them, and
            // the two are indistinguishable from either end alone.
            info!("<- {} from peer {sender:#x}", signal_kind(&signal));
            socket.receive_signal(sender, signal);
        }
    }

    for outgoing in socket.drain_signals() {
        let data = serde_json::to_string(&outgoing.signal).expect("Failed to serialize PeerSignal");
        info!(
            "-> {} to peer {:#x}",
            signal_kind(&outgoing.signal),
            outgoing.peer
        );
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
    let reliable = packet.send_mode.is_reliable();

    // Resolve the target: active lobby, pending lobby, or a specific client entity.
    let host = lobby_query
        .get(packet.entity)
        .or_else(|_| pending_lobby_query.get(packet.entity))
        .ok()
        .map(|(host, _)| host);

    if let Some(host) = host {
        let data: Box<[u8]> = packet.packet.clone().into_boxed_slice();
        let peers: Vec<u128> = socket.connected_peers().collect();
        if host.is_some() {
            for peer in peers {
                socket.send_with_mode(data.clone(), peer, reliable);
            }
        } else if let Some(&peer) = peers.first() {
            socket.send_with_mode(data, peer, reliable);
        }
        return;
    }

    // Triggered on a LobbyClient entity (targeted send)
    if let Ok(client_uuid) = lobby_client_query.get(packet.entity) {
        let data: Box<[u8]> = packet.packet.clone().into_boxed_slice();
        socket.send_with_mode(data, client_uuid.0, reliable);
        return;
    }

    // `debug!`, not `error!`. The ordinary way to get here is a peer leaving: the
    // `On<Remove, LobbyClient>` observer notifies the departing client, and by the
    // time that trigger has been through the encode observer and back out as a
    // `SerializedLobbyPacket` the entity no longer resolves. Nothing is wrong -- a
    // packet aimed at somebody who has already disconnected is not an error, and a
    // project whose acceptance criterion is a clean log cannot hold that line while
    // every normal quit prints one.
    debug!(
        "Serialized lobby packet was triggered for an entity that no longer resolves \
         ({:?}); the peer has most likely just left.",
        packet.entity
    );
}

pub(crate) fn read_peer_messages(world: &mut World) {
    let packets = {
        let mut socket = world.resource_mut::<crate::EnsembleSocketRes>();
        socket.receive()
    };

    for (sender_uuid, payload) in packets {
        if !decode_ensemble_packet(world, Some(sender_uuid), &payload) {
            warn!("Failed to decode ensemble packet from peer {sender_uuid}");
        }
    }
}

/// Sends a removal notification and disconnects the WebRTC peer when a
/// [`LobbyClient`] is removed.
///
/// The removal packet must be sent directly here (not via deferred commands)
/// because observer ordering is non-deterministic and `disconnect_peer` severs
/// the connection immediately — any deferred message would arrive too late.
pub(crate) fn disconnect_removed_lobby_client(
    trigger: On<Remove, LobbyClient>,
    query: Query<(&LobbyClientWebrtcUuid, &LobbyClientPlayerUuid)>,
    registry: Res<bevy_ensemble::EnsembleMessageRegistry>,
    mut socket: ResMut<crate::EnsembleSocketRes>,
) {
    let Ok((webrtc_uuid, player_uuid)) = query.get(trigger.event_target()) else {
        return;
    };

    // We send the removal packet manually here because Bevy does not
    // guarantee observer execution order. If this observer runs before the
    // core `on_lobby_client_removed`, the peer will be disconnected before
    // the deferred LobbyClientMessage ever flushes, so the kicked client
    // would never learn it was removed.
    // TODO: fix this once observer ordering lands (bevyengine/bevy#14890)
    let packet = encode_ensemble_message(
        &registry,
        &RemoveLobbyParticipant {
            player_uuid: player_uuid.0,
        },
    );
    socket.send(packet.into_boxed_slice(), webrtc_uuid.0);
    socket.disconnect_peer(webrtc_uuid.0);
}
