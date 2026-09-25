use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy_ensemble::{
    Host, HostLost, HostMigratable, HostUuid, LivenessGrace, Lobby, LobbyClient,
    LobbyClientPlayerUuid, LobbyJoinFailed, LobbyLeft, LobbyLeftReason, LobbyParticipant,
    LobbyParticipantOf, LocalMultiplayerPlayerId, NewHostNamed, ParticipantDeparted, PeerRoute,
    PendingLobby, PublicLobbies, PublicLobbyInfo, RemoveLobbyParticipant, RequestLobby,
    SerializedLobbyPacket, decode_ensemble_packet, encode_ensemble_message,
};
use bevy_ensemble_sockets::{PeerSignal, PeerState};

use crate::connection::{LobbyConnection, LobbyEvent};
use crate::protocol::ClientMessage;

use crate::{
    JoinWebrtcLobby, JoinWebrtcLobbyByCode, LobbyClientWebrtcUuid, LobbyHostUuid, LobbyWebrtcCode,
    LobbyWebrtcId, PendingWebrtcLobbyClient, RefreshLobbyList, SignallingDisplayName,
};

/// Which side of the session this peer is on, as far as trust decisions go.
///
/// Derived from the lobby entities each time it is needed rather than stored, so it cannot go
/// stale: a peer is a host while it has a hosted lobby, a client while it has a joined or
/// pending-join lobby, and nothing in between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerRole {
    Host,
    Client,
    /// No lobby at all. Nothing arriving over a data channel or a signal is expected.
    None,
}

/// Whether an SDP offer from `from` should be answered.
///
/// A client answers its host and nobody else: in this protocol the host is always the offerer,
/// so an offer from another lobby member is that member trying to become the peer this client
/// talks to -- its "host" in every way that matters -- and answering it would hand the session
/// over. A host never legitimately receives an offer for the same reason, and with no lobby there
/// is nothing to be joining.
pub(crate) fn accept_offer_from(role: PeerRole, host: Option<u128>, from: u128) -> bool {
    match role {
        PeerRole::Client => host == Some(from),
        PeerRole::Host | PeerRole::None => false,
    }
}

/// Whether a data-channel packet from `from` should be decoded at all.
///
/// A client reads its host only. A host reads the peers the signalling server told it joined --
/// the ones it holds a `LobbyClient` or `PendingWebrtcLobbyClient` entity for -- which is what
/// `known_clients` is. Anything else is a connection that should not exist, and the safe thing to
/// do with its traffic is nothing.
pub(crate) fn accept_packet_from(
    role: PeerRole,
    host: Option<u128>,
    known_clients: &[u128],
    from: u128,
) -> bool {
    match role {
        PeerRole::Client => host == Some(from),
        PeerRole::Host => known_clients.contains(&from),
        PeerRole::None => false,
    }
}

/// Whether a peer going away ends a client's session.
///
/// Only the host's does. Another peer's disconnect is not this client's business -- it never
/// connected to them on purpose -- and used to take the whole lobby down with it.
pub(crate) fn client_session_ends_with(host: Option<u128>, peer: u128) -> bool {
    host == Some(peer)
}

/// How many refused packets per peer are logged at `warn!` before dropping to `debug!`.
///
/// Three is enough to notice and not enough to bury the log under a peer that keeps sending.
const UNTRUSTED_DROPS_LOGGED_LOUDLY: u32 = 3;

/// Packets dropped by [`read_peer_messages`] because their sender was not trusted, per sender.
#[derive(Resource, Default, Debug)]
pub(crate) struct UntrustedPacketDrops(HashMap<u128, u32>);

/// How often the signalling server is told this peer is still here, in seconds.
///
/// Well under any idle timeout a server would reasonably have, and cheap: one tiny frame over an
/// otherwise silent WebSocket. Without it a host sitting in a lobby waiting for players is
/// indistinguishable, to the server, from one that went away.
const KEEP_ALIVE_INTERVAL_SECS: f64 = 20.0;

/// How long to wait between attempts to rebuild a lost signalling connection, in seconds.
const RECONNECT_INTERVAL_SECS: f64 = 5.0;

/// Keep the signalling server's idea of this peer's name equal to [`SignallingDisplayName`].
///
/// Compared against what *this connection* authenticated with, rather than against what this
/// system last sent. Those differ exactly when the socket has been rebuilt underneath it — leaving
/// a lobby, or recovering a dropped signalling socket — and the difference was a bug: a peer that
/// had set its name, then left a lobby, was re-authenticated by the new socket and never corrected,
/// because nothing this system could see had changed. It went back to being called whatever the
/// plugin was built with, and stayed there.
///
/// Comparing against the connection also means a name set before the first frame is not missed,
/// which the old first-frame skip did miss: a game that fills the resource in at startup, from a
/// save or a page's storage, had it silently dropped.
///
/// There is no retry, and the reason is the channel rather than optimism: this is the WebSocket to
/// the signalling server, which is TCP. If it is up the message arrives; if it is not, this peer
/// has no listing to be wrong in.
pub(crate) fn publish_display_name(
    name: Res<SignallingDisplayName>,
    mut lobby_conn: ResMut<LobbyConnection>,
) {
    if lobby_conn.announced_name == name.0 {
        return;
    }
    let _ = lobby_conn.command_tx.send(ClientMessage::SetDisplayName {
        display_name: name.0.clone(),
    });
    lobby_conn.announced_name = name.0.clone();
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
    mut lobby_left: MessageWriter<LobbyLeft>,
    mut join_failed: MessageWriter<LobbyJoinFailed>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    pending_host_lobbies: Query<Entity, (With<PendingLobby>, With<RequestLobby>, With<Host>)>,
    pending_client_lobbies: Query<Entity, (With<PendingLobby>, Without<Host>)>,
    active_client_lobbies: Query<Entity, (With<Lobby>, Without<Host>)>,
    all_lobbies: Query<(Entity, Has<PendingLobby>), Or<(With<Lobby>, With<PendingLobby>)>>,
    lobby_clients: Query<
        (Entity, &LobbyClientWebrtcUuid, &LobbyClientPlayerUuid),
        Or<(With<LobbyClient>, With<PendingWebrtcLobbyClient>)>,
    >,
    participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
    runtime: Res<crate::WebrtcRuntime>,
) {
    // Queries see the world as it was before this frame's events, and a promotion is only
    // applied once they have all been read: a join or a leave the server sends right after
    // naming this peer host has to find the lobby it now hosts and the seats opened for it here.
    let mut hosted = host_lobby.as_deref().copied();
    let mut seated: Vec<(Entity, u128)> = Vec::new();
    for event in events.read() {
        match event {
            LobbyEvent::Welcome { player_uuid } => {
                info!("Authenticated with signaling server, uuid: {player_uuid}");
                lobby_conn.local_player_uuid = Some(*player_uuid);
                // The identity is known from here on, before any lobby exists. Nothing else
                // puts one in place: `StartHosting` no longer inserts a placeholder, so a host
                // whose lobby is still being created would otherwise have no id at all.
                commands.insert_resource(LocalMultiplayerPlayerId(*player_uuid));
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
                // A host is its own authority.
                commands.insert_resource(HostUuid(player_uuid));

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

            LobbyEvent::LobbyJoined {
                lobby_id,
                host_uuid,
                existing_members,
            } => {
                info!(
                    "Joined lobby: {lobby_id}, hosted by {host_uuid:#x} with {} other member(s)",
                    existing_members.len()
                );
                let Some(player_uuid) = lobby_conn.local_player_uuid else {
                    warn!(
                        "joined lobby {lobby_id} before the server said `Welcome`; no local uuid \
                         to play under, so the join is being dropped"
                    );
                    continue;
                };
                commands.insert_resource(LocalMultiplayerPlayerId(player_uuid));
                // Set before the host's offer can be answered (`pump_socket_signals` runs after
                // this system), so no data channel exists to a peer this side does not trust.
                commands.insert_resource(HostUuid(*host_uuid));

                if let Some(entity) = pending_client_lobbies.iter().next() {
                    commands
                        .entity(entity)
                        .insert((LobbyWebrtcId(*lobby_id), LobbyHostUuid(*host_uuid)));
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
                commands.remove_resource::<HostUuid>();
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
                let Some(lobby) = hosted else {
                    warn!(
                        "dropping the join of {player_uuid}: this peer has no single hosted \
                         lobby to attach them to, so no connection to them will be opened"
                    );
                    continue;
                };

                let already_known = lobby_clients
                    .iter()
                    .any(|(_, _, puuid)| puuid.0 == player_uuid)
                    || seated.iter().any(|(_, uuid)| *uuid == player_uuid);
                if already_known {
                    debug!("{player_uuid} is already a known client; not connecting twice");
                    continue;
                }

                // Initiate WebRTC connection to the new peer
                socket.connect_peer(player_uuid);

                let seat = commands
                    .spawn((
                        PendingWebrtcLobbyClient,
                        LobbyParticipantOf(lobby),
                        LobbyClientWebrtcUuid(player_uuid),
                        LobbyClientPlayerUuid(player_uuid),
                    ))
                    .id();
                seated.push((seat, player_uuid));
            }

            LobbyEvent::PlayerLeft { player_uuid } => {
                let player_uuid = *player_uuid;
                info!("Player left lobby: {player_uuid}");

                let seat = lobby_clients
                    .iter()
                    .find(|(_, _, puuid)| puuid.0 == player_uuid)
                    .map(|(seat, _, _)| seat)
                    .or_else(|| {
                        seated
                            .iter()
                            .find(|(_, uuid)| *uuid == player_uuid)
                            .map(|(seat, _)| *seat)
                    });
                if hosted.is_some()
                    && let Some(seat) = seat
                {
                    seated.retain(|(entity, _)| *entity != seat);
                    commands.entity(seat).try_despawn();
                    continue;
                }
                // Somebody the server says is gone at a moment no host could say so: a member a
                // new host inherited and has no seat for yet, or one that left while this peer
                // waited for its host. The core decides which, if either, this is.
                if let Some((lobby, _)) = all_lobbies.iter().next() {
                    commands
                        .entity(lobby)
                        .trigger(move |entity| ParticipantDeparted {
                            entity,
                            player_uuid,
                        });
                }
            }

            // The lobby survives its host: waiting for the server to name a successor is worth
            // as long as the server takes to notice a silent host, plus the keep-alive it would
            // have to miss, plus margin; reaching the successor, as long as a join may take.
            LobbyEvent::LobbyMigratable { idle_timeout_secs } => {
                let Some((lobby, _)) = all_lobbies.iter().next() else {
                    continue;
                };
                let successor_within =
                    std::time::Duration::from_secs(u64::from(*idle_timeout_secs))
                        + std::time::Duration::from_secs_f64(KEEP_ALIVE_INTERVAL_SECS)
                        + std::time::Duration::from_secs(10);
                let reach_within = runtime
                    .join_timeout
                    .unwrap_or(std::time::Duration::from_secs(15))
                    + std::time::Duration::from_secs(5);
                commands.entity(lobby).try_insert(HostMigratable {
                    successor_within,
                    reach_within,
                });
            }

            // The server named a new host. This peer either becomes it — opening a connection to
            // every member, as a host does to a joiner — or points its transport at it, and the
            // core does the rest once `NewHostNamed` lands. Both before any of the new host's
            // traffic can be read: its offer is only answered once `LobbyHostUuid` names it.
            LobbyEvent::HostChanged {
                lobby_id,
                previous_host,
                new_host,
                code,
                members,
            } => {
                let Some(me) = lobby_conn.local_player_uuid else {
                    continue;
                };
                let Some((lobby, _)) = all_lobbies.iter().next() else {
                    continue;
                };
                let (previous, new_host) = (*previous_host, *new_host);
                info!(
                    "lobby {lobby_id}: the server named {new_host:#x} host in place of \
                     {previous:#x}"
                );
                socket.disconnect_peer(previous);
                if new_host == me {
                    commands
                        .entity(lobby)
                        .try_remove::<LobbyHostUuid>()
                        .try_insert(LobbyWebrtcCode(code.clone()));
                    hosted = Some(lobby);
                    for member in members.iter().copied().filter(|member| *member != me) {
                        if lobby_clients.iter().any(|(_, _, uuid)| uuid.0 == member)
                            || seated.iter().any(|(_, uuid)| *uuid == member)
                        {
                            continue;
                        }
                        // A stale entry would make the offer a no-op.
                        socket.disconnect_peer(member);
                        socket.connect_peer(member);
                        let seat = commands
                            .spawn((
                                PendingWebrtcLobbyClient,
                                LobbyParticipantOf(lobby),
                                LobbyClientWebrtcUuid(member),
                                LobbyClientPlayerUuid(member),
                            ))
                            .id();
                        seated.push((seat, member));
                    }
                } else {
                    hosted = None;
                    // So its offer makes a fresh connection, not a restart of a dead one.
                    socket.disconnect_peer(new_host);
                    commands
                        .entity(lobby)
                        .try_insert((LobbyHostUuid(new_host), LobbyWebrtcCode(code.clone())));
                }
                let members = members.clone();
                commands.entity(lobby).trigger(move |entity| NewHostNamed {
                    entity,
                    previous,
                    new_host,
                    members: Some(members),
                });
            }

            // The server removing this peer from its lobby: the host closed it, or left one that
            // could not be handed over -- to anybody, or to a peer that never declared it could
            // take it.
            LobbyEvent::Disconnected { reason } => {
                info!("Disconnected from lobby: {reason}");
                let lobbies: Vec<Entity> = pending_client_lobbies
                    .iter()
                    .chain(active_client_lobbies.iter())
                    .collect();
                if lobbies.is_empty() {
                    commands.remove_resource::<LocalMultiplayerPlayerId>();
                    commands.remove_resource::<HostUuid>();
                    continue;
                }
                commands.queue(move |world: &mut World| {
                    end_client_lobbies(world, &lobbies, LobbyLeftReason::HostGone);
                });
            }

            // The WebSocket is gone. A lobby on either side is over: the server drops a lobby
            // whose host it cannot reach and tells the members so, and a client with no
            // signalling cannot be told anything. Despawning a lobby with a server id rebuilds
            // the connection through `detect_lobby_leave`; when there is no such lobby,
            // `reconnect_signalling` does it instead, with a backoff.
            LobbyEvent::SignallingClosed => {
                warn!("lost the signalling server");
                lobby_conn.signalling_lost = true;
                // Before anything is despawned: the seats are despawned below while their lobby
                // still stands, which reads as each of them being kicked. Nobody here is being
                // removed from a session that goes on; the session is over for all of them.
                socket.disconnect_all();
                let mut had_lobby = false;
                let mut was_joining = false;
                for (lobby, pending) in all_lobbies.iter() {
                    had_lobby = true;
                    was_joining |= pending;
                    for (participant_entity, _, pof) in participants.iter() {
                        if pof.0 == lobby {
                            commands.entity(participant_entity).try_despawn();
                        }
                    }
                    for (client_entity, _, _) in lobby_clients.iter() {
                        commands.entity(client_entity).try_despawn();
                    }
                    commands.entity(lobby).try_despawn();
                }
                commands.remove_resource::<LocalMultiplayerPlayerId>();
                commands.remove_resource::<HostUuid>();
                if was_joining {
                    join_failed.write(LobbyJoinFailed {
                        reason: "Lost the signalling server before the lobby was ready.".into(),
                    });
                }
                if had_lobby {
                    lobby_left.write(LobbyLeft {
                        reason: LobbyLeftReason::SignallingLost,
                    });
                }
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

/// End this client's session in `lobbies`, judged when the command is applied rather than when
/// the event was read.
///
/// The server's word that a lobby is over can land on the same frame as the core ending it on its
/// own -- the host's `LobbyClosed` over the data channel, most often, which the server's
/// `Disconnected` races -- and whichever is applied second must find nothing left to end, or the
/// game hears `LobbyLeft` twice for one session.
fn end_client_lobbies(world: &mut World, lobbies: &[Entity], reason: LobbyLeftReason) {
    let mut ended = false;
    for &lobby in lobbies {
        if let Ok(entity) = world.get_entity_mut(lobby) {
            entity.despawn();
            ended = true;
        }
    }
    if !ended {
        return;
    }
    world.remove_resource::<LocalMultiplayerPlayerId>();
    world.remove_resource::<HostUuid>();
    world.write_message(LobbyLeft { reason });
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
/// - On a **client**: despawns the lobby entity when the *host* peer disconnects
///   (e.g. kicked or host left), which triggers the full leave/cleanup flow. Any other peer
///   going away is logged and ignored; it was never this client's session.
/// - `Reconnecting` on either side is logged and nothing else: the socket is restarting ICE
///   with the data channels kept, and it reports `Connected` or `Failed` when that is settled.
pub(crate) fn poll_socket_peers(
    mut commands: Commands,
    mut socket: ResMut<crate::EnsembleSocketRes>,
    mut join_failed: MessageWriter<LobbyJoinFailed>,
    mut lobby_left: MessageWriter<LobbyLeft>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    client_lobbies: Query<
        (Entity, Has<PendingLobby>, Option<&LobbyHostUuid>),
        (Or<(With<Lobby>, With<PendingLobby>)>, Without<Host>),
    >,
    lobby_clients: Query<
        (Entity, &LobbyClientWebrtcUuid),
        Or<(With<LobbyClient>, With<PendingWebrtcLobbyClient>)>,
    >,
    migratable: Query<(), With<HostMigratable>>,
) {
    for (peer_id, state) in socket.update_peers() {
        match state {
            PeerState::Connecting => {
                debug!("peer {peer_id:#x}: connecting");
            }
            PeerState::Connected => {
                info!("Peer connected: {peer_id}");
                // Back from a restart, or never away: liveness runs on its normal clock.
                for entity in liveness_entity(
                    peer_id,
                    host_lobby.as_deref(),
                    &lobby_clients,
                    &client_lobbies,
                ) {
                    commands.entity(entity).try_remove::<LivenessGrace>();
                }
            }
            // The path is gone and ICE is restarting to find another; the data channels are
            // still open and everything queued on them will arrive once it has. Nothing to tear
            // down: `Connected` follows if it works, `Failed` if it does not, and the arm below
            // handles that one. A host keeps the client's seat, a client keeps its lobby.
            PeerState::Reconnecting => {
                info!(
                    "peer {peer_id:#x}: the path to it was lost; ICE is restarting, the \
                     session stands"
                );
                // The liveness check must outlast the restart, or it ends the session the
                // restart was about to save. The socket gives up after `ICE_RESTART_TIMEOUT`
                // and reports `Failed`, which tears down on its own.
                for entity in liveness_entity(
                    peer_id,
                    host_lobby.as_deref(),
                    &lobby_clients,
                    &client_lobbies,
                ) {
                    commands.entity(entity).try_insert(LivenessGrace {
                        extra: bevy_ensemble_sockets::ICE_RESTART_TIMEOUT,
                    });
                }
            }
            // The same teardown either way -- what differs is what it means and who is told.
            // `Failed` is a connection that never opened, or one whose ICE restart found no
            // path in time; if the lobby is still pending this is a join that will not be
            // completing, and somebody is watching a screen that would otherwise never change.
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

                // Client side: the host was the whole session. Anybody else was a connection
                // this side never asked for, and its ending changes nothing.
                let mut was_joining = false;
                let mut host_gone = false;
                for (entity, pending, host) in client_lobbies.iter() {
                    if !client_session_ends_with(host.map(|host| host.0), peer_id) {
                        debug!(
                            "peer {peer_id:#x} went away and it is not the host \
                             ({:#x?}); the lobby stands",
                            host.map(|host| host.0)
                        );
                        continue;
                    }
                    // A lobby that outlives its host waits to be told who hosts it now.
                    if !pending && migratable.contains(entity) {
                        info!(
                            "lost the connection to the host {peer_id:#x}; waiting for the \
                             server to name the lobby's next host"
                        );
                        commands
                            .entity(entity)
                            .trigger(|entity| HostLost { entity });
                        continue;
                    }
                    host_gone = true;
                    was_joining |= pending;
                    commands.entity(entity).try_despawn();
                }
                if !host_gone {
                    continue;
                }
                commands.remove_resource::<LocalMultiplayerPlayerId>();
                commands.remove_resource::<HostUuid>();
                lobby_left.write(LobbyLeft {
                    reason: LobbyLeftReason::HostGone,
                });
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

/// The entity whose liveness clock `peer_id` runs on: the host's `LobbyClient` for that peer,
/// or a client's lobby if the peer is its host. Empty for a peer this side does not track.
fn liveness_entity(
    peer_id: u128,
    host_lobby: Option<&Entity>,
    lobby_clients: &Query<
        (Entity, &LobbyClientWebrtcUuid),
        Or<(With<LobbyClient>, With<PendingWebrtcLobbyClient>)>,
    >,
    client_lobbies: &Query<
        (Entity, Has<PendingLobby>, Option<&LobbyHostUuid>),
        (Or<(With<Lobby>, With<PendingLobby>)>, Without<Host>),
    >,
) -> Vec<Entity> {
    if host_lobby.is_some() {
        return lobby_clients
            .iter()
            .filter(|(_, uuid)| uuid.0 == peer_id)
            .map(|(entity, _)| entity)
            .collect();
    }
    client_lobbies
        .iter()
        .filter(|(_, _, host)| host.is_some_and(|host| host.0 == peer_id))
        .map(|(entity, _, _)| entity)
        .collect()
}

/// Record which kind of ICE pair each peer ended up on.
///
/// Separate from [`poll_socket_peers`] because the two answer different questions at different
/// moments: that one reports a connection appearing or going away, this one reports what the
/// connection turned out to *be*, which is only known once ICE has nominated a pair — after the
/// peer is already connected.
///
/// The entity mapping is the same in both: a host carries one `LobbyClient` per peer, a client
/// carries the lobby itself.
///
/// # Why this reconciles rather than applies the report
///
/// [`EnsembleSocket::update_routes`] reports a route *once per change*, which is right for a fact
/// that is settled when ICE nominates a pair and never normally changes again. It is also the
/// whole difficulty: the report arrives when the data channel opens, and on a client that is
/// strictly **before** the lobby it belongs on exists. A joiner is still holding a `PendingLobby`
/// at that moment — promotion to `Lobby` rides the host handshake, which travels over the very
/// channel whose opening produced the report. Applied straight, the one report this session will
/// ever make is written to nothing, and the overlay says `route: pending` until the peer leaves.
///
/// So the drain is kept only for the line in the log, and the components are reconciled from the
/// socket's own record of what it has settled on. That is idempotent, costs one comparison per
/// peer per frame, and lands the moment there is an entity to land on — whenever that is.
///
/// [`EnsembleSocket::update_routes`]: bevy_ensemble_sockets::EnsembleSocket::update_routes
pub(crate) fn poll_peer_routes(
    mut commands: Commands,
    mut socket: ResMut<crate::EnsembleSocketRes>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    client_lobbies: Query<
        (Entity, Option<&PeerRoute>),
        (Or<(With<Lobby>, With<PendingLobby>)>, Without<Host>),
    >,
    lobby_clients: Query<
        (Entity, &LobbyClientWebrtcUuid, Option<&PeerRoute>),
        Or<(With<LobbyClient>, With<PendingWebrtcLobbyClient>)>,
    >,
) {
    // Said once, when it changes: a relayed session is worth a line, and a line a frame is not.
    for (peer_id, route) in socket.update_routes() {
        if matches!(route, bevy_ensemble_sockets::PeerRoute::Relayed) {
            info!("peer {peer_id:#x} is connected through the relay, not directly");
        }
    }

    // Collected first: `route` borrows the socket, and `update_routes` above needed it mutably.
    let peers: Vec<u128> = socket.connected_peers().collect();
    let routes: Vec<(u128, PeerRoute)> = peers
        .iter()
        .filter_map(|peer| {
            socket.route(*peer).map(|route| {
                let route = match route {
                    bevy_ensemble_sockets::PeerRoute::Direct => PeerRoute::Direct,
                    bevy_ensemble_sockets::PeerRoute::Relayed => PeerRoute::Relayed,
                };
                (*peer, route)
            })
        })
        .collect();
    if routes.is_empty() {
        return;
    }

    if host_lobby.is_some() {
        for (entity, uuid, current) in lobby_clients.iter() {
            let Some((_, route)) = routes.iter().find(|(peer, _)| *peer == uuid.0) else {
                continue;
            };
            if current != Some(route) {
                commands.entity(entity).try_insert(*route);
            }
        }
        return;
    }

    // A client has one connection that matters, the one to its host.
    let Some((_, route)) = routes.first() else {
        return;
    };
    for (entity, current) in client_lobbies.iter() {
        if current != Some(route) {
            commands.entity(entity).try_insert(*route);
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
    pending: Query<
        (Entity, Option<&PendingSince>, Has<Host>),
        (With<PendingLobby>, Without<Lobby>),
    >,
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
        let what = if is_host {
            "host a lobby"
        } else {
            "join a lobby"
        };
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

/// How long an offer this peer cannot answer yet is held for the lobby event that would let it.
///
/// That event is already on its way when the offer arrives — the server sends it first, on the
/// same WebSocket — so the wait is a frame or two. The rest is margin for a slow frame; an offer
/// still unanswerable after it is from a peer this side will never answer.
const DEFERRED_OFFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How many peers' offers are held at once. A handful covers every honest case, and the cap is
/// what keeps a member sending offers in a loop from growing the buffer without bound.
const MAX_DEFERRED_OFFERS: usize = 8;

/// Signals from peers whose offer arrived before this side had a reason to answer it.
///
/// Lobby events and peer signals leave the WebSocket task on separate channels. The server sends
/// a joiner `LobbyJoined` before the host's offer, but if the task has pushed the offer and not
/// yet the event when the frame drains them, the offer is judged before its sender is known to be
/// the host. It used to be refused, and an offer is sent once: the join then waited out its
/// timeout for a data channel nobody was going to open. Now it is held — along with every later
/// signal from the same peer, which mean nothing until the offer is applied — and judged again
/// each frame until its sender can be answered or the hold runs out.
#[derive(Resource, Default)]
pub(crate) struct DeferredSignals(Vec<DeferredPeer>);

struct DeferredPeer {
    sender: u128,
    since: bevy_ensemble::Instant,
    signals: Vec<PeerSignal>,
}

impl DeferredSignals {
    /// Route one signal as it arrives: returned if it is to be applied now, kept or dropped if
    /// not. `answerable` is whether an offer from `sender` would be accepted this frame.
    fn admit(
        &mut self,
        sender: u128,
        signal: PeerSignal,
        answerable: bool,
        now: bevy_ensemble::Instant,
    ) -> Option<PeerSignal> {
        if let Some(held) = self.0.iter_mut().find(|held| held.sender == sender) {
            held.signals.push(signal);
            return None;
        }
        // An offer is the one signal that *creates* a connection; the others only apply to one
        // that exists, and the socket already discards those for unknown peers. So the trust
        // decision is made here, once, on the offer.
        if answerable || !matches!(signal, PeerSignal::Offer(_)) {
            return Some(signal);
        }
        if self.0.len() >= MAX_DEFERRED_OFFERS {
            warn!(
                "ignoring an offer from peer {sender:#x}: this peer cannot answer it, and already \
                 holds {MAX_DEFERRED_OFFERS} others waiting to be answerable"
            );
            return None;
        }
        debug!("holding an offer from peer {sender:#x} until this peer can answer it");
        self.0.push(DeferredPeer {
            sender,
            since: now,
            signals: vec![signal],
        });
        None
    }

    /// Everything held from peers that can now be answered, in the order it arrived. A peer held
    /// past [`DEFERRED_OFFER_TIMEOUT`] is dropped with its signals.
    fn release(
        &mut self,
        answerable: impl Fn(u128) -> bool,
        now: bevy_ensemble::Instant,
    ) -> Vec<(u128, PeerSignal)> {
        let mut released = Vec::new();
        self.0.retain_mut(|held| {
            let sender = held.sender;
            if answerable(sender) {
                released.extend(
                    std::mem::take(&mut held.signals)
                        .into_iter()
                        .map(|s| (sender, s)),
                );
                return false;
            }
            if now.duration_since(held.since) < DEFERRED_OFFER_TIMEOUT {
                return true;
            }
            warn!(
                "ignoring an offer from peer {sender:#x}: held for {}s and this peer still has no \
                 reason to answer it, so that peer has no business opening a connection",
                DEFERRED_OFFER_TIMEOUT.as_secs()
            );
            false
        });
        released
    }
}

/// Each frame, pump signals between the WS handler and the EnsembleSocket:
/// 1. Drain incoming signals from LobbyConnection's signal_rx and feed them to socket.receive_signal()
/// 2. Drain outbound signals from socket.drain_signals() and send them as ClientMessage::Signal
pub(crate) fn pump_socket_signals(
    mut socket: ResMut<crate::EnsembleSocketRes>,
    mut deferred: ResMut<DeferredSignals>,
    lobby_conn: Res<LobbyConnection>,
    host_lobbies: Query<(), (Or<(With<Lobby>, With<PendingLobby>)>, With<Host>)>,
    client_lobbies: Query<
        Option<&LobbyHostUuid>,
        (Or<(With<Lobby>, With<PendingLobby>)>, Without<Host>),
    >,
) {
    let (role, host) = if let Some(host) = client_lobbies.iter().next() {
        (PeerRole::Client, host.map(|host| host.0))
    } else if !host_lobbies.is_empty() {
        (PeerRole::Host, None)
    } else {
        (PeerRole::None, None)
    };
    let now = bevy_ensemble::Instant::now();

    for (sender, signal) in deferred.release(|sender| accept_offer_from(role, host, sender), now) {
        info!(
            "<- {} from peer {sender:#x}, held until it could be answered",
            signal_kind(&signal)
        );
        socket.receive_signal(sender, signal);
    }

    if let Ok(mut signal_rx) = lobby_conn.signal_rx.lock() {
        while let Ok((sender, signal)) = signal_rx.try_recv() {
            // Both directions are logged, at the one seam every signal crosses, because the
            // interesting failures are asymmetric: a peer whose offer and answer both arrive
            // while its candidates do not is a different bug from one that never sends them, and
            // the two are indistinguishable from either end alone.
            info!("<- {} from peer {sender:#x}", signal_kind(&signal));
            let answerable = accept_offer_from(role, host, sender);
            if let Some(signal) = deferred.admit(sender, signal, answerable, now) {
                socket.receive_signal(sender, signal);
            }
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
    display_name: Res<SignallingDisplayName>,
    mut removed: RemovedComponents<LobbyWebrtcId>,
) {
    for _entity in removed.read() {
        info!("Lobby entity removed, sending LeaveLobby and rebuilding connection");

        // Send LeaveLobby on the current connection
        let _ = lobby_conn.command_tx.send(ClientMessage::LeaveLobby);

        // Disconnect all WebRTC peers
        socket.disconnect_all();

        // The session's identity goes with it. `Welcome` on the rebuilt connection restores
        // the local id; the host is whoever the next lobby says.
        commands.remove_resource::<LocalMultiplayerPlayerId>();
        commands.remove_resource::<HostUuid>();

        // Rebuild the WS connection from scratch, under the name the player is going by now and
        // not the one the plugin was built with.
        // The old WS task will naturally exit when its channels are dropped.
        let (new_socket, lobby_connection) = webrtc_runtime.build_socket(&display_name.0);
        commands.insert_resource(new_socket);
        commands.insert_resource(lobby_connection);
    }
}

pub(crate) fn send_serialized_lobby_packet(
    packet: On<SerializedLobbyPacket>,
    socket: ResMut<crate::EnsembleSocketRes>,
    host_uuid: Option<Res<HostUuid>>,
    lobby_query: Query<(Option<&Host>, Option<&LobbyHostUuid>), With<Lobby>>,
    pending_lobby_query: Query<
        (Option<&Host>, Option<&LobbyHostUuid>),
        (With<PendingLobby>, Without<Lobby>),
    >,
    lobby_client_query: Query<&LobbyClientWebrtcUuid>,
) {
    let reliable = packet.send_mode.is_reliable();

    // Resolve the target: active lobby, pending lobby, or a specific client entity.
    let lobby = lobby_query
        .get(packet.entity)
        .or_else(|_| pending_lobby_query.get(packet.entity))
        .ok();

    if let Some((host, lobby_host)) = lobby {
        let data: Box<[u8]> = packet.packet.clone().into_boxed_slice();
        if host.is_some() {
            let peers: Vec<u128> = socket.connected_peers().collect();
            for peer in peers {
                socket.send_with_mode(data.clone(), peer, reliable);
            }
            return;
        }
        // A client talks to its host, by name -- not to whichever connected peer a hash map
        // happens to list first, which with a second connection open is a coin toss.
        let host = lobby_host
            .map(|host| host.0)
            .or(host_uuid.map(|host| host.0));
        let Some(host) = host else {
            debug!("dropping a lobby packet: this client does not know its host yet");
            return;
        };
        if socket.connected_peers().any(|peer| peer == host) {
            socket.send_with_mode(data, host, reliable);
        } else {
            debug!("dropping a lobby packet: the host {host:#x} is not connected yet");
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

/// Drain the socket and decode what the trusted peers sent.
///
/// The trust decision is [`accept_packet_from`]: a client reads its host, a host reads the
/// peers the signalling server told it joined. It is made here, before decoding, so that no
/// packet from anybody else reaches a message reader at all -- the per-type `HostOnly` check in
/// `bevy_ensemble` is the second line, not the first.
pub(crate) fn read_peer_messages(world: &mut World) {
    let packets = {
        let mut socket = world.resource_mut::<crate::EnsembleSocketRes>();
        socket.receive()
    };
    if packets.is_empty() {
        return;
    }

    let (role, host) = {
        let mut client_lobbies = world.query_filtered::<Option<&LobbyHostUuid>, (
            Or<(With<Lobby>, With<PendingLobby>)>,
            Without<Host>,
        )>();
        if let Some(host) = client_lobbies.iter(world).next() {
            (PeerRole::Client, host.map(|host| host.0))
        } else {
            let mut host_lobbies = world.query_filtered::<(), (With<Lobby>, With<Host>)>();
            if host_lobbies.iter(world).next().is_some() {
                (PeerRole::Host, None)
            } else {
                (PeerRole::None, None)
            }
        }
    };
    let known_clients: Vec<u128> = if role == PeerRole::Host {
        let mut clients = world.query_filtered::<&LobbyClientWebrtcUuid, Or<(
            With<LobbyClient>,
            With<PendingWebrtcLobbyClient>,
        )>>();
        clients.iter(world).map(|uuid| uuid.0).collect()
    } else {
        Vec::new()
    };

    for (sender_uuid, payload, received_at) in packets {
        if !accept_packet_from(role, host, &known_clients, sender_uuid) {
            let count = {
                let mut drops = world.get_resource_or_insert_with(UntrustedPacketDrops::default);
                let count = drops.0.entry(sender_uuid).or_insert(0);
                *count += 1;
                *count
            };
            if count <= UNTRUSTED_DROPS_LOGGED_LOUDLY {
                warn!(
                    "dropping a packet from peer {sender_uuid:#x}: this peer is {role:?} and \
                     that one is not its host ({host:#x?}) nor a client it was told joined; \
                     {count} so far, later ones at debug level"
                );
            } else {
                debug!("dropping a packet from untrusted peer {sender_uuid:#x} ({count} so far)");
            }
            continue;
        }
        if !decode_ensemble_packet(world, Some(sender_uuid), &payload, received_at) {
            warn!("Failed to decode ensemble packet from peer {sender_uuid}");
        }
    }
}

/// Tell the signalling server this peer is still here, every [`KEEP_ALIVE_INTERVAL_SECS`].
///
/// Only once `Welcome` has arrived -- before that there is no session to keep alive -- and never
/// on a connection already known to be gone.
pub(crate) fn send_keep_alives(
    lobby_conn: Res<LobbyConnection>,
    time: Res<Time>,
    mut next_at: Local<f64>,
) {
    if lobby_conn.local_player_uuid.is_none() || lobby_conn.signalling_lost {
        return;
    }
    let now = time.elapsed_secs_f64();
    if now < *next_at {
        return;
    }
    *next_at = now + KEEP_ALIVE_INTERVAL_SECS;
    let _ = lobby_conn.command_tx.send(ClientMessage::KeepAlive);
}

/// Rebuild a signalling connection that was lost while no lobby was there to rebuild it.
///
/// Losing the server while in a lobby ends the lobby, and ending a lobby with a server id
/// rebuilds the connection (`detect_lobby_leave`). Losing it while idle -- in a menu, or after a
/// join was refused -- used to leave a dead WebSocket behind for the rest of the run, so the next
/// host or join silently went nowhere. Retried on a backoff rather than every frame so a server
/// that is down is not hammered.
pub(crate) fn reconnect_signalling(
    mut commands: Commands,
    lobby_conn: Res<LobbyConnection>,
    webrtc_runtime: Res<crate::WebrtcRuntime>,
    display_name: Res<SignallingDisplayName>,
    time: Res<Time>,
    mut next_attempt: Local<f64>,
    lobbies_with_id: Query<(), With<LobbyWebrtcId>>,
) {
    if !lobby_conn.signalling_lost || !lobbies_with_id.is_empty() {
        return;
    }
    let now = time.elapsed_secs_f64();
    if now < *next_attempt {
        return;
    }
    *next_attempt = now + RECONNECT_INTERVAL_SECS;

    info!("reconnecting to the signalling server");
    let (new_socket, lobby_connection) = webrtc_runtime.build_socket(&display_name.0);
    commands.insert_resource(new_socket);
    commands.insert_resource(lobby_connection);
}

#[cfg(test)]
mod trust_tests {
    use super::{PeerRole, accept_offer_from, accept_packet_from, client_session_ends_with};

    const HOST: u128 = 0xA;
    const OTHER: u128 = 0xB;

    #[test]
    fn a_client_accepts_an_offer_only_from_its_host() {
        assert!(accept_offer_from(PeerRole::Client, Some(HOST), HOST));
        assert!(!accept_offer_from(PeerRole::Client, Some(HOST), OTHER));
        // Not yet told who the host is: nobody is trusted, rather than the first to ask.
        assert!(!accept_offer_from(PeerRole::Client, None, HOST));
        // A host is the offerer; an offer to it is never legitimate. Nor with no lobby.
        assert!(!accept_offer_from(PeerRole::Host, None, OTHER));
        assert!(!accept_offer_from(PeerRole::None, None, HOST));
    }

    #[test]
    fn a_client_reads_only_its_host_and_a_host_only_its_known_clients() {
        assert!(accept_packet_from(PeerRole::Client, Some(HOST), &[], HOST));
        assert!(!accept_packet_from(
            PeerRole::Client,
            Some(HOST),
            &[],
            OTHER
        ));
        assert!(!accept_packet_from(PeerRole::Client, None, &[], HOST));
        assert!(accept_packet_from(PeerRole::Host, None, &[OTHER], OTHER));
        assert!(!accept_packet_from(PeerRole::Host, None, &[OTHER], HOST));
        assert!(!accept_packet_from(PeerRole::None, None, &[OTHER], OTHER));
    }

    #[test]
    fn only_the_host_leaving_ends_a_client_session() {
        assert!(client_session_ends_with(Some(HOST), HOST));
        assert!(!client_session_ends_with(Some(HOST), OTHER));
        assert!(!client_session_ends_with(None, OTHER));
    }
}

/// Whether a seat being removed is its player being removed from a session that goes on — a
/// kick, a timeout, a refused protocol — rather than the seat going with the host's own lobby.
///
/// Only the first is news to the player. When the host leaves, its lobby is despawned and the
/// seats follow it; a relationship despawns its sources through deferred commands, so by the time
/// a seat's removal is observed the lobby is already gone. Telling each of those players it had
/// been removed made every host that left kick everybody on its way out: each client ended its
/// session as `Kicked` rather than finding out the host was gone.
pub(crate) fn seat_removal_is_a_kick(
    seat_of: Option<&LobbyParticipantOf>,
    hosted_lobbies: &Query<(), (With<Lobby>, With<Host>)>,
) -> bool {
    seat_of.is_some_and(|seat_of| hosted_lobbies.contains(seat_of.0))
}

/// Sends a removal notification and disconnects the WebRTC peer when a
/// [`LobbyClient`] is removed.
///
/// The removal packet must be sent directly here (not via deferred commands)
/// because observer ordering is non-deterministic and `disconnect_peer` severs
/// the connection immediately — any deferred message would arrive too late.
///
/// Sent only for a kick; see [`seat_removal_is_a_kick`].
pub(crate) fn disconnect_removed_lobby_client(
    trigger: On<Remove, LobbyClient>,
    query: Query<(
        &LobbyClientWebrtcUuid,
        &LobbyClientPlayerUuid,
        Option<&LobbyParticipantOf>,
    )>,
    hosted_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    registry: Res<bevy_ensemble::EnsembleMessageRegistry>,
    mut socket: ResMut<crate::EnsembleSocketRes>,
) {
    let Ok((webrtc_uuid, player_uuid, seat_of)) = query.get(trigger.event_target()) else {
        return;
    };

    if seat_removal_is_a_kick(seat_of, &hosted_lobbies) {
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
    }
    socket.disconnect_peer(webrtc_uuid.0);
}

#[cfg(test)]
mod departure_tests {
    use bevy::prelude::*;
    use bevy_ensemble::{Host, Lobby, LobbyClient, LobbyParticipantOf};

    use super::seat_removal_is_a_kick;

    #[derive(Resource, Default)]
    struct Verdicts(Vec<bool>);

    fn record_verdict(
        trigger: On<Remove, LobbyClient>,
        seats: Query<Option<&LobbyParticipantOf>>,
        hosted_lobbies: Query<(), (With<Lobby>, With<Host>)>,
        mut verdicts: ResMut<Verdicts>,
    ) {
        let seat_of = seats.get(trigger.event_target()).ok().flatten();
        verdicts
            .0
            .push(seat_removal_is_a_kick(seat_of, &hosted_lobbies));
    }

    /// A seat removed on its own is a kick; the seats that go with the host's lobby are not. The
    /// second half rests on the lobby being gone by the time its seats' removal is observed,
    /// which is Bevy's to guarantee, and this is what notices if it stops.
    #[test]
    fn a_seat_removed_with_its_lobby_is_not_told_it_was_kicked() {
        let mut app = App::new();
        app.init_resource::<Verdicts>().add_observer(record_verdict);
        let world = app.world_mut();
        let lobby = world.spawn((Lobby, Host)).id();
        let kicked = world.spawn((LobbyClient, LobbyParticipantOf(lobby))).id();
        world.spawn((LobbyClient, LobbyParticipantOf(lobby)));
        world.spawn((LobbyClient, LobbyParticipantOf(lobby)));

        world.despawn(kicked);
        world.flush();
        assert_eq!(world.resource::<Verdicts>().0, [true], "a kick");

        world.despawn(lobby);
        world.flush();
        assert_eq!(
            world.resource::<Verdicts>().0,
            [true, false, false],
            "the host leaving is not two more kicks"
        );
    }
}

#[cfg(test)]
mod deferred_signal_tests {
    use std::time::Duration;

    use bevy_ensemble::Instant;
    use bevy_ensemble_sockets::PeerSignal;

    use super::{DEFERRED_OFFER_TIMEOUT, DeferredSignals, MAX_DEFERRED_OFFERS, signal_kind};

    const HOST: u128 = 0xA;
    const OTHER: u128 = 0xB;

    fn offer() -> PeerSignal {
        PeerSignal::Offer(String::new())
    }

    fn candidate() -> PeerSignal {
        PeerSignal::IceCandidate(String::new())
    }

    fn kinds(signals: &[(u128, PeerSignal)]) -> Vec<(u128, &'static str)> {
        signals
            .iter()
            .map(|(sender, signal)| (*sender, signal_kind(signal)))
            .collect()
    }

    /// The offer that arrived a frame before `LobbyJoined` named its sender: held, with the
    /// candidates behind it, and applied in order once the sender is the host.
    #[test]
    fn a_deferred_offer_is_answered_once_its_sender_is_the_host() {
        let now = Instant::now();
        let mut deferred = DeferredSignals::default();

        assert!(deferred.admit(HOST, offer(), false, now).is_none());
        assert!(
            deferred.admit(HOST, candidate(), false, now).is_none(),
            "a candidate means nothing before its offer, so it waits behind it"
        );
        assert!(
            deferred.release(|_| false, now).is_empty(),
            "not answerable yet"
        );

        let released = deferred.release(|sender| sender == HOST, now);
        assert_eq!(
            kinds(&released),
            [(HOST, "offer"), (HOST, "candidate")],
            "released in arrival order"
        );
        assert!(deferred.release(|_| true, now).is_empty(), "and only once");
    }

    #[test]
    fn a_deferred_offer_expires() {
        let now = Instant::now();
        let mut deferred = DeferredSignals::default();
        deferred.admit(OTHER, offer(), false, now);

        let later = now + DEFERRED_OFFER_TIMEOUT + Duration::from_millis(1);
        assert!(deferred.release(|_| false, later).is_empty());
        assert!(
            deferred.release(|_| true, later).is_empty(),
            "an offer dropped on expiry is not answered later"
        );
    }

    /// Nothing is delayed that did not need to be: an answerable offer, and every signal that is
    /// not an offer from a peer with nothing held.
    #[test]
    fn a_signal_with_no_reason_to_wait_is_applied_at_once() {
        let now = Instant::now();
        let mut deferred = DeferredSignals::default();
        assert!(deferred.admit(HOST, offer(), true, now).is_some());
        assert!(deferred.admit(OTHER, candidate(), false, now).is_some());
    }

    #[test]
    fn no_more_offers_are_held_than_the_cap() {
        let now = Instant::now();
        let mut deferred = DeferredSignals::default();
        for sender in 0..(MAX_DEFERRED_OFFERS as u128 + 3) {
            deferred.admit(sender, offer(), false, now);
        }
        assert_eq!(deferred.release(|_| true, now).len(), MAX_DEFERRED_OFFERS);
    }
}
