use bevy::prelude::*;

use crate::{
    Host, HostUuid, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyParticipant,
    LobbyParticipantOf, LocalMultiplayerPlayerId, PendingLobby, PlayerUUID, RequestLobby, SendMode,
    messages::{
        LobbyClientMessage, LobbyMessage, RemoveLobbyParticipant, StartHosting,
        SyncLobbyParticipant,
    },
    migration::{AwaitingSeat, LobbyClosed, MigratedSeat, end_client_session},
    session::LobbyLeftReason,
};

/// Spawns a host lobby entity when [`StartHosting`] is received.
///
/// Creates an entity with [`PendingLobby`], [`RequestLobby`], and [`Host`] components. The
/// [`LocalMultiplayerPlayerId`] is the backend's to insert, once it knows it; this used to insert
/// a placeholder of zero, and everything that read the resource between then and the backend's
/// answer believed it.
///
/// Ignored if a host lobby already exists.
pub(crate) fn spawn_host_lobby(
    mut commands: Commands,
    mut start_hosting: MessageReader<StartHosting>,
    existing_hosts: Query<(), (With<Host>, Or<(With<Lobby>, With<PendingLobby>)>)>,
) {
    let mut should_spawn = false;
    for _ in start_hosting.read() {
        should_spawn = true;
    }

    if !should_spawn || !existing_hosts.is_empty() {
        return;
    }

    commands.spawn((PendingLobby, RequestLobby, Host));
}

/// Keep [`HostUuid`] true on a host, and gone once there is no lobby of any kind.
///
/// A host is its own host: the moment it knows its identity, that is who its authoritative
/// messages come from. A client's `HostUuid` is the backend's to set as part of joining; this
/// only removes it when the client has left, so a stale host cannot be trusted into the next
/// session.
pub(crate) fn publish_host_uuid(
    mut commands: Commands,
    local_player_id: Option<Res<LocalMultiplayerPlayerId>>,
    host_uuid: Option<Res<HostUuid>>,
    host_lobbies: Query<(), (With<Host>, Or<(With<Lobby>, With<PendingLobby>)>)>,
    any_lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
) {
    if !host_lobbies.is_empty() {
        if let Some(local) = local_player_id
            && host_uuid.map(|host| host.0) != Some(local.0)
        {
            commands.insert_resource(HostUuid(local.0));
        }
        return;
    }
    if any_lobbies.is_empty() && host_uuid.is_some() {
        commands.remove_resource::<HostUuid>();
    }
}

/// Adds the host player as a [`LobbyParticipant`] once the lobby is active and the host knows
/// who it is.
///
/// Not keyed on `Added<Lobby>`: the backend may learn the local identity after the lobby is
/// promoted, and a participant that is only ever created on the frame the lobby appeared would
/// then never be created. The existing-participant check is what makes this idempotent.
pub(crate) fn add_host_lobby_participant(
    mut commands: Commands,
    local_player_id: Option<Res<LocalMultiplayerPlayerId>>,
    ready_host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    existing_participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
) {
    let Some(local_player_id) = local_player_id else {
        return;
    };

    for lobby in ready_host_lobbies.iter() {
        if existing_participants
            .iter()
            .any(|(participant, participant_of)| {
                participant_of.0 == lobby && participant.player_uuid == local_player_id.0
            })
        {
            continue;
        }

        commands.spawn((
            LobbyParticipant {
                player_uuid: local_player_id.0,
                is_host: true,
            },
            LobbyParticipantOf(lobby),
        ));
    }
}

/// Creates [`LobbyParticipant`] entities for newly connected remote clients.
///
/// When a [`LobbyClient`] component is added to an entity that also has
/// [`LobbyClientPlayerUuid`], this system spawns a corresponding participant.
///
/// A participant that already exists is a member a new host inherited, arriving at last: it stops
/// [`AwaitingSeat`], and its seat is marked so its protocol is asked for again if need be.
///
/// # Seated only if the seat is still there
///
/// The work is queued, and the queued command looks at the seat again when it runs. A seat can be
/// despawned between this system reading it and its commands being applied — a liveness timeout
/// or a refused handshake in the same frame, whose commands may well be applied first. The
/// removal observer then finds no participant to remove and announces the departure of somebody
/// nobody was told had arrived; a participant spawned after that, by a command that did not
/// look, is a ghost: the host lists it and announces it to everyone, and nothing will ever remove
/// it, since the seat whose removal would have is already gone.
pub(crate) fn add_remote_lobby_participants(
    mut commands: Commands,
    added_lobby_clients: Query<Entity, (With<LobbyClient>, Added<LobbyClient>)>,
) {
    for seat in added_lobby_clients.iter() {
        commands.queue(move |world: &mut World| seat_participant(world, seat));
    }
}

fn seat_participant(world: &mut World, seat: Entity) {
    let Ok(seat_ref) = world.get_entity(seat) else {
        return;
    };
    if !seat_ref.contains::<LobbyClient>() {
        return;
    }
    let (Some(player_uuid), Some(seat_of)) = (
        seat_ref.get::<LobbyClientPlayerUuid>().map(|uuid| uuid.0),
        seat_ref.get::<LobbyParticipantOf>().map(|of| of.0),
    ) else {
        return;
    };

    let existing = world
        .query::<(
            Entity,
            &LobbyParticipant,
            &LobbyParticipantOf,
            Has<AwaitingSeat>,
        )>()
        .iter(world)
        .find(|(_, participant, of, _)| of.0 == seat_of && participant.player_uuid == player_uuid)
        .map(|(participant, _, _, awaiting_seat)| (participant, awaiting_seat));
    if let Some((participant, awaiting_seat)) = existing {
        if awaiting_seat {
            world.entity_mut(participant).remove::<AwaitingSeat>();
            world.entity_mut(seat).insert(MigratedSeat);
        }
        return;
    }

    world.spawn((
        LobbyParticipant {
            player_uuid,
            is_host: false,
        },
        LobbyParticipantOf(seat_of),
    ));
}

/// Sends existing participant data to newly connected lobby clients.
///
/// When a new [`LobbyClient`] joins, this system sends a
/// [`SyncLobbyParticipant`] message for every existing participant so the
/// new client can build its local roster.
pub(crate) fn sync_existing_participants_to_new_lobby_clients(
    mut commands: Commands,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    added_lobby_clients: Query<
        (Entity, &LobbyParticipantOf),
        (With<LobbyClient>, Added<LobbyClient>),
    >,
) {
    for (client_entity, participant_of) in added_lobby_clients.iter() {
        for (participant, existing_participant_of) in participants.iter() {
            if existing_participant_of.0 != participant_of.0 {
                continue;
            }

            let message = SyncLobbyParticipant {
                player_uuid: participant.player_uuid,
                is_host: participant.is_host,
            };
            commands
                .entity(client_entity)
                .trigger(move |entity| LobbyClientMessage::<SyncLobbyParticipant> {
                    entity,
                    message,
                    send_mode: SendMode::Reliable,
                });
        }
    }
}

/// Keeps the host's participant identity in sync with [`LocalMultiplayerPlayerId`].
///
/// If the platform backend updates the local player's UUID after the host participant
/// was already created, this system patches the participant to match.
pub(crate) fn sync_host_lobby_participant_identity(
    mut commands: Commands,
    local_player_id: Option<Res<LocalMultiplayerPlayerId>>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let Some(local_player_id) = local_player_id else {
        return;
    };

    for host_lobby in host_lobbies.iter() {
        for (participant_entity, participant, participant_of) in participants.iter() {
            if participant_of.0 != host_lobby || !participant.is_host {
                continue;
            }

            if participant.player_uuid == local_player_id.0 {
                continue;
            }

            commands
                .entity(participant_entity)
                .try_insert(LobbyParticipant {
                    player_uuid: local_player_id.0,
                    is_host: true,
                });
        }
    }
}

/// Broadcasts participant changes from the host to all clients.
///
/// When a [`LobbyParticipant`] is added or changed on a host lobby, this system
/// triggers a [`LobbyMessage<SyncLobbyParticipant>`] to propagate the change.
pub(crate) fn broadcast_changed_lobby_participants(
    mut commands: Commands,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    changed_participants: Query<
        (&LobbyParticipant, &LobbyParticipantOf),
        Or<(Added<LobbyParticipant>, Changed<LobbyParticipant>)>,
    >,
    lobby_clients: Query<(), With<LobbyClient>>,
) {
    if host_lobbies.is_empty() {
        return;
    }

    for (participant, participant_of) in changed_participants.iter() {
        if lobby_clients.get(participant_of.0).is_ok() {
            continue;
        }

        let message = SyncLobbyParticipant {
            player_uuid: participant.player_uuid,
            is_host: participant.is_host,
        };
        commands
            .entity(participant_of.0)
            .trigger(move |entity| LobbyMessage::<SyncLobbyParticipant> {
                entity,
                message,
                send_mode: SendMode::Reliable,
            });
    }
}

/// One change to a client's picture of its lobby, as its host said it.
///
/// The host says three things about a lobby to its clients: this player is in it
/// ([`SyncLobbyParticipant`]), this player is not ([`RemoveLobbyParticipant`] — or, when it
/// names the client itself, "you are out"), and the lobby is over ([`LobbyClosed`]). Each is its
/// own wire type, and each type has its own message buffer, so the order they were said in is
/// lost the moment they are decoded — and the order is the meaning. `Sync(X)` then `Remove(X)`
/// is somebody who came and went; read the other way round it is somebody who is here for good.
/// That is what happened whenever the two landed in one frame (a hitch, a backgrounded tab, a
/// join and a drop in quick succession): the removal was applied first, found nobody, and the
/// sync then spawned a participant that no message would ever remove.
///
/// So these types are decoded into this one stream as well as their own, in the order they came
/// off the wire, and [`apply_lobby_state`] is the only thing that reads it.
#[derive(Clone, Debug)]
pub(crate) enum LobbyStateChange {
    Sync(SyncLobbyParticipant),
    Remove(RemoveLobbyParticipant),
    Closed,
}

impl From<SyncLobbyParticipant> for LobbyStateChange {
    fn from(sync: SyncLobbyParticipant) -> Self {
        Self::Sync(sync)
    }
}

impl From<RemoveLobbyParticipant> for LobbyStateChange {
    fn from(remove: RemoveLobbyParticipant) -> Self {
        Self::Remove(remove)
    }
}

impl From<LobbyClosed> for LobbyStateChange {
    fn from(_: LobbyClosed) -> Self {
        Self::Closed
    }
}

/// A [`LobbyStateChange`] as it arrived. See there. Who sent it was settled at decode: these are
/// host-only types, and a client decodes them only from its host.
#[derive(Message, Clone, Debug)]
pub(crate) struct LobbyStateUpdate {
    pub(crate) change: LobbyStateChange,
}

/// A client applies what its host says about the lobby, in the order the host said it.
///
/// Everything read in one run is applied against the roster as this run leaves it, not as the
/// world held it when the run began: `Commands` are not applied until later, so a participant
/// this run spawned is not yet visible to a query, and one it despawned still is. `roster` is
/// that running picture. Without it a player announced twice in one run — once by
/// [`sync_existing_participants_to_new_lobby_clients`], once by
/// [`broadcast_changed_lobby_participants`], which is how every join is announced — got two
/// entities, and one announced and removed in one run was never removed.
///
/// A removal naming this peer is a kick, and a [`LobbyClosed`] ends the session: either is the
/// last word, and anything after it in the same run belongs to a session that is over.
///
/// On a host, or with no lobby, everything is read and dropped: the host is the one who says
/// these things, and a peer in no lobby has nothing to apply them to — keeping them unread would
/// apply them to whatever lobby it joins next.
pub(crate) fn apply_lobby_state(
    mut commands: Commands,
    mut updates: MessageReader<LobbyStateUpdate>,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    client_lobbies: Query<Entity, (Or<(With<Lobby>, With<PendingLobby>)>, Without<Host>)>,
    existing_participants: Query<(Entity, &LobbyParticipant, &LobbyParticipantOf)>,
) {
    let updates: Vec<LobbyStateUpdate> = updates.read().cloned().collect();
    if updates.is_empty() {
        return;
    }
    let Some(client_lobby) = client_lobbies.iter().next() else {
        return;
    };

    let mut roster: Vec<(PlayerUUID, Entity)> = existing_participants
        .iter()
        .filter(|(_, _, of)| of.0 == client_lobby)
        .map(|(entity, participant, _)| (participant.player_uuid, entity))
        .collect();

    for update in updates {
        match update.change {
            LobbyStateChange::Sync(sync) => {
                let participant = LobbyParticipant {
                    player_uuid: sync.player_uuid,
                    is_host: sync.is_host,
                };
                // Updated rather than skipped when already known, so that a later message still
                // has the last word on whoever it describes.
                if let Some((_, entity)) = roster
                    .iter()
                    .find(|(player_uuid, _)| *player_uuid == sync.player_uuid)
                {
                    commands.entity(*entity).try_insert(participant);
                    continue;
                }
                let entity = commands
                    .spawn((participant, LobbyParticipantOf(client_lobby)))
                    .id();
                roster.push((sync.player_uuid, entity));
            }
            LobbyStateChange::Remove(remove) => {
                if local_player
                    .as_ref()
                    .is_some_and(|local| local.0 == remove.player_uuid)
                {
                    let reason = match remove.reason {
                        crate::SeatRemoval::Kicked => LobbyLeftReason::Kicked,
                        crate::SeatRemoval::TimedOut => LobbyLeftReason::TimedOut,
                        crate::SeatRemoval::Left => LobbyLeftReason::Left,
                    };
                    info!("the host removed this peer from the lobby: {reason:?}");
                    commands.queue(move |world: &mut World| {
                        end_client_session(world, client_lobby, reason)
                    });
                    return;
                }
                if let Some(at) = roster
                    .iter()
                    .position(|(player_uuid, _)| *player_uuid == remove.player_uuid)
                {
                    let (_, entity) = roster.swap_remove(at);
                    commands.entity(entity).try_despawn();
                }
            }
            LobbyStateChange::Closed => {
                info!("the host closed the lobby");
                commands.queue(move |world: &mut World| {
                    end_client_session(world, client_lobby, LobbyLeftReason::HostGone)
                });
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy::ecs::schedule::ScheduleBuildSettings;

    use super::*;
    use crate::observers::on_lobby_client_removed;

    /// Despawns every new seat. Run before `add_remote_lobby_participants` with no sync point
    /// between, so its despawn is applied first while that system has already read the seat —
    /// the order a liveness timeout or a refused handshake can land in.
    fn drop_new_seats(mut commands: Commands, seats: Query<Entity, Added<LobbyClient>>) {
        for seat in seats.iter() {
            commands.entity(seat).despawn();
        }
    }

    #[test]
    fn a_seat_removed_before_its_participant_is_made_leaves_no_participant() {
        let mut world = World::new();
        world.add_observer(on_lobby_client_removed);
        let lobby = world.spawn((Lobby, Host)).id();
        world.spawn((
            LobbyClient,
            LobbyClientPlayerUuid(2),
            LobbyParticipantOf(lobby),
        ));

        let mut schedule = Schedule::default();
        schedule.set_build_settings(ScheduleBuildSettings {
            auto_insert_apply_deferred: false,
            ..default()
        });
        schedule.add_systems((drop_new_seats, add_remote_lobby_participants).chain());
        schedule.run(&mut world);
        schedule.run(&mut world);

        let participants = world
            .query::<&LobbyParticipant>()
            .iter(&world)
            .map(|participant| participant.player_uuid)
            .collect::<Vec<_>>();
        assert!(
            participants.is_empty(),
            "the seat is gone, so is its player: a participant made after the seat's removal \
             is one nothing will ever remove ({participants:?})"
        );
    }
}
