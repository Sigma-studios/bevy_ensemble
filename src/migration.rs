//! The lobby outliving its host.
//!
//! A session is a star around its host, and the host used to be the session: when it went, every
//! client despawned its lobby. Now a lobby a backend marks [`HostMigratable`] waits instead, and a
//! member takes the host role over under the same lobby entity, with the same participants and
//! the same player data.
//!
//! # Who decides
//!
//! Never the peers. Something every member already trusts — the signalling server, Steam — says
//! who the host is when a lobby is joined, and the same party says who it becomes. A backend
//! reports two facts to the core and the core does the rest:
//!
//! - [`HostLost`]: the transport says the host is gone. The lobby waits for a successor, as
//!   [`AwaitingHost`], for at most [`HostMigratable::successor_within`]. A host that merely stopped
//!   answering pings is waited for the same way, and taken back if it answers again.
//! - [`NewHostNamed`]: the arbiter named the new host. If it is this peer, the lobby is promoted
//!   on the spot; otherwise this peer follows, and reads nothing from the new host until the two
//!   have compared protocols, as at a join.
//!
//! Both end in a [`HostChanged`] message, which is what a game reads. A lobby that is never told
//! who the host became ends as it always did, with `LobbyLeft { HostGone }`.
//!
//! # A link lost is not a host lost
//!
//! Most of the time the host has not gone anywhere: the connection between it and one client
//! broke — a DTLS or SCTP failure, a Wi-Fi drop, a liveness timeout — and both are still in the
//! lobby as far as the arbiter is concerned. Nobody will name a successor, because nobody
//! left. A lobby a backend marks [`Reconnectable`] treats that as what it is:
//!
//! - **The host keeps the seat.** The backend (or the liveness check) reports [`SeatLinkLost`];
//!   the seat entity goes, quietly — no removal is announced — and the participant waits as
//!   [`AwaitingSeat`] for [`Reconnectable::within`], while the backend makes a new connection to
//!   it. Whatever the game hung on the participant's player stays, score and all. A client that
//!   does not come back in time is dropped as [`SeatRemoval::TimedOut`](crate::SeatRemoval).
//! - **The client waits for the same host.** [`HostLost`] on a reconnectable lobby waits as
//!   [`AwaitingHost`] with the old host as its own successor
//!   ([`is_reconnect`](AwaitingHost::is_reconnect)), and the link state is dropped so the new
//!   connection is verified afresh. Reaching it again is the same handshake as reaching a new
//!   host, and there is no [`HostChanged`]: the session never changed hands. A lobby that can
//!   also migrate falls back to waiting for a successor when the old host does not come back.
//!
//! Only the host can make the new connection — it is always the offerer — so the client's half
//! is to wait, and to read nothing it sends meanwhile.
//!
//! # What does not survive
//!
//! Anything in flight to or from the old host, anything this peer sent while switching, and the
//! ordering of reliable messages across the change: those were promises about a connection, and
//! the connection is gone. The game's own state is the game's: see its netcode layer.

use std::time::Duration;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    HandshakeVerified, Host, HostUuid, LivenessGrace, Lobby, LobbyClient, LobbyClientPlayerUuid,
    LobbyMessage, LobbyParticipant, LobbyParticipantOf, LocalMultiplayerPlayerId, PeerReliableRtt,
    PeerRoute, PeerRtt, PeerRttJitter, PeerSilence, PeerWireRtt, PendingLobby, PlayerUUID,
    ReceivedEnsembleMessage, RemoveLobbyParticipant, SeatRemoval, SendMode,
    handshake::ProtocolMatched,
    ping::{EnsemblePong, PeerLastPongSeq},
    registry::HeldUntilVerified,
    session::{CloseLobby, LeaveLobby, LobbyLeft, LobbyLeftReason},
};

/// This lobby survives its host, and how long it waits for what.
///
/// Inserted on a lobby entity by the backend, when the party that decides who hosts — the
/// signalling server, Steam — has said it will name a successor. A lobby without it ends when its
/// host goes, as every lobby used to; a backend that knows nothing about host migration never
/// inserts it, and nothing changes for it.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostMigratable {
    /// How long a lobby that has lost its host waits to be told who the new one is. Should
    /// outlast however long the arbiter takes to notice the host is gone.
    pub successor_within: Duration,
    /// How long, once the new host is named, a member has to reach it — and the new host waits
    /// for each member to arrive — before giving up on the other.
    pub reach_within: Duration,
}

impl Default for HostMigratable {
    fn default() -> Self {
        Self {
            successor_within: Duration::from_secs(30),
            reach_within: Duration::from_secs(20),
        }
    }
}

/// A game's override of the waits its backend put in [`HostMigratable`]. Either may be left
/// `None` to keep the backend's.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostMigrationTimeouts {
    pub successor_within: Option<Duration>,
    pub reach_within: Option<Duration>,
}

/// A broken link in this lobby is made again rather than ended: the host holds a lost client's
/// seat, and a client that lost its host waits for it. See the module notes.
///
/// Inserted on a lobby entity — the host's and every client's — by a backend that can reach a
/// lost peer again: whose arbiter still lists both ends in the lobby, and whose host can open a
/// new connection to a member at any time. A lobby without it ends a broken link as it always
/// did: the host removes the seat, the client leaves (or, in a [`HostMigratable`] lobby, waits for
/// a successor).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reconnectable {
    /// How long the host keeps a lost client's seat, and a client waits for its host to reach it
    /// again. Long enough for a new connection to be negotiated and checked, twice over; short
    /// enough that the players left waiting are not waiting on somebody who has gone.
    pub within: Duration,
}

impl Default for Reconnectable {
    fn default() -> Self {
        Self {
            within: Duration::from_secs(20),
        }
    }
}

/// Why a lobby is waiting for a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostLossCause {
    /// The backend said the host is gone.
    Transport,
    /// The host stopped answering pings for [`PeerTimeout`](crate::PeerTimeout). It may only be
    /// frozen, and is taken back if it answers.
    Silence,
}

/// On a client's lobby: its host is gone, or the link to it is, and it is waiting.
///
/// Added when the host is lost; `successor` is filled in when the arbiter names the new host, and
/// the component goes once this peer and the new host have compared protocols. If `waited`
/// reaches `limit` first, the session ends.
///
/// In a [`Reconnectable`] lobby, `successor` is the old host from the start: the link broke, the
/// host did not leave, and it is the one expected to reach this peer again. See
/// [`is_reconnect`](Self::is_reconnect).
///
/// Nothing from the host is being applied while this is on, by definition: a game holds its
/// clock and says so on screen, rather than letting the player carry on in a world nobody else
/// is in.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct AwaitingHost {
    /// The host that was lost.
    pub previous: PlayerUUID,
    /// The host this lobby is waiting to be reached by: the one the arbiter named, or the old
    /// one again after a broken link. `None` while nobody has been named.
    pub successor: Option<PlayerUUID>,
    pub cause: HostLossCause,
    pub waited: Duration,
    pub limit: Duration,
}

impl AwaitingHost {
    /// Whether this is a wait for the same host to reach this peer again, after the link to it
    /// broke: nobody left, and nobody is taking over.
    pub fn is_reconnect(&self) -> bool {
        self.successor == Some(self.previous)
    }
}

/// On a participant of a lobby this peer has just started hosting: a member the arbiter says is
/// still in the lobby, that has not reached this peer yet. Removed when it does; the participant
/// is dropped if `waited` reaches `limit` first.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct AwaitingSeat {
    pub waited: Duration,
    pub limit: Duration,
}

/// On a client's lobby, next to [`HandshakeVerified`]: the host whose protocol was compared.
///
/// The marker alone said "the host was verified" without saying which, and the host can change.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifiedHost(pub PlayerUUID);

/// On a seat a new host made for a member it inherited, so its protocol is asked for again if the
/// first announcement did not land.
#[derive(Component, Clone, Copy, Debug)]
pub(crate) struct MigratedSeat;

/// Backend → core: the transport says this lobby's host is gone.
///
/// Trigger it on a client's lobby. A migratable, promoted lobby waits for a successor; any other
/// ends as it would have, with `LobbyLeft { HostGone }`. Ignored on a host's own lobby.
#[derive(EntityEvent, Clone, Debug)]
pub struct HostLost {
    pub entity: Entity,
}

/// Backend → core, on a host: the link to this seated client broke, and the client is not known
/// to have left.
///
/// Trigger it on the client's `LobbyClient` seat, in place of despawning it. In a
/// [`Reconnectable`] lobby the seat goes quietly — nobody is told the player left — and the
/// participant waits as [`AwaitingSeat`] while the backend connects to the client again: a new
/// seat for the same player takes the participant back. In any other lobby the seat is removed,
/// and everybody told, as a timeout ([`SeatRemoval::TimedOut`]).
///
/// The liveness check reports a client that went silent the same way.
#[derive(EntityEvent, Clone, Debug)]
pub struct SeatLinkLost {
    pub entity: Entity,
}

/// On a `LobbyClient` seat as it is despawned: its link broke and its player is being held, so
/// the removal is not a departure. The removal observers — the core's, a backend's — read it to
/// announce nothing and kick nobody. See [`SeatLinkLost`].
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeldSeat;

/// Backend → core: the arbiter named the host of this lobby.
///
/// Trigger it on this peer's lobby, before the backend lets anything from the new host through.
/// `members` is who the arbiter says is still in the lobby, when it says: participants missing
/// from it are dropped, since the host that would have announced their leaving is the one that
/// is gone.
#[derive(EntityEvent, Clone, Debug)]
pub struct NewHostNamed {
    pub entity: Entity,
    pub previous: PlayerUUID,
    pub new_host: PlayerUUID,
    pub members: Option<Vec<PlayerUUID>>,
}

/// Backend → core: the arbiter says this player left the lobby, at a moment no host could say so.
///
/// On a host, the player's seat or participant is removed and everyone is told. On a lobby
/// waiting for its host, the participant is removed locally. Otherwise the roster is the host's
/// business, and this is ignored.
#[derive(EntityEvent, Clone, Debug)]
pub struct ParticipantDeparted {
    pub entity: Entity,
    pub player_uuid: PlayerUUID,
}

/// The host of `lobby` changed from `previous` to `new`. Written on every peer that stays in the
/// lobby, in the same update that changed [`HostUuid`].
///
/// `promoted` is whether this peer is the new host. Nothing from the new host has been read by
/// the time this is written; what was in flight to or from the old host is lost.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct HostChanged {
    pub lobby: Entity,
    pub previous: PlayerUUID,
    pub new: PlayerUUID,
    pub promoted: bool,
}

/// Sent by a host closing its lobby: the session is over for everyone, and nobody takes it over.
#[doc(hidden)]
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LobbyClosed;

/// On a host's lobby between [`CloseLobby`] and the leave it becomes: long enough for the
/// [`LobbyClosed`] announcement to be flushed.
#[derive(Component, Clone, Copy, Debug)]
pub(crate) struct ClosingLobby;

/// Everything on a client's lobby entity that describes its link to one particular host, and so
/// goes when the host changes: whether that host's protocol was verified and which host it was,
/// the round trip and its spread, the liveness clock, the route the transport took to reach it.
///
/// Named once, here, so that a component added to the link is added to what a change of host
/// strips — as a tuple written out at the one place that strips it, it was a list somebody had
/// to remember to extend. A participant's [`ParticipantLink`](crate::ParticipantLink) is the same
/// kind of thing, measured by the host, and is stripped alongside.
///
/// # Why not a child entity
///
/// The cleaner shape would be the host's own link entity under the lobby, despawned on a change
/// of host, as a host's links are its `LobbyClient` seats. It is not that because these are the
/// components games, the lockstep bridge and every backend read *on the lobby entity* — a ping
/// display, an adaptive input buffer, a relay badge, the WebRTC backend's ICE-restart grace — and
/// because the lobby entity is what a client's outgoing packets are addressed to. Moving them is
/// a change to every one of those readers and to the transport seam, for a reset that this one
/// named bundle already makes complete.
pub(crate) type HostLinkState = (
    HandshakeVerified,
    VerifiedHost,
    PeerRtt,
    PeerWireRtt,
    PeerReliableRtt,
    PeerRttJitter,
    PeerSilence,
    PeerLastPongSeq,
    LivenessGrace,
    PeerRoute,
);

/// What a lobby still gets when neither the backend nor the game said.
const DEFAULT_REACH_WITHIN: Duration = Duration::from_secs(20);

fn waits(world: &World, lobby: Entity) -> Option<HostMigratable> {
    let migratable = *world.get::<HostMigratable>(lobby)?;
    let overrides = world
        .get_resource::<HostMigrationTimeouts>()
        .copied()
        .unwrap_or_default();
    Some(HostMigratable {
        successor_within: overrides
            .successor_within
            .unwrap_or(migratable.successor_within),
        reach_within: overrides.reach_within.unwrap_or(migratable.reach_within),
    })
}

/// End a client's session: the lobby goes, the identity goes with it, and [`LobbyLeft`] says why.
/// What every host-loss path did before there was anything to wait for.
pub(crate) fn end_client_session(world: &mut World, lobby: Entity, reason: LobbyLeftReason) {
    // Two words that the session is over can land in one frame -- the host's `LobbyClosed` and the
    // platform's own notice, say -- and only the first one ends anything.
    let Ok(entity) = world.get_entity_mut(lobby) else {
        return;
    };
    entity.despawn();
    world.remove_resource::<LocalMultiplayerPlayerId>();
    world.write_message(LobbyLeft { reason });
}

pub(crate) fn on_host_lost(lost: On<HostLost>, mut commands: Commands) {
    let lobby = lost.entity;
    commands.queue(move |world: &mut World| host_lost(world, lobby, HostLossCause::Transport));
}

/// The host of `lobby` is gone, for `cause`: wait for a successor, or end the session.
pub(crate) fn host_lost(world: &mut World, lobby: Entity, cause: HostLossCause) {
    let Ok(entity) = world.get_entity(lobby) else {
        return;
    };
    if entity.contains::<Host>() {
        return;
    }
    let promoted = entity.contains::<Lobby>() && !entity.contains::<PendingLobby>();
    let awaiting = entity.get::<AwaitingHost>().copied();
    // Already waiting for this host to come back: a second word about the same loss -- the
    // transport's after the liveness check's, a `Failed` after a `Disconnected` -- changes
    // nothing, and must not restart the clock.
    if awaiting.is_some_and(|awaiting| awaiting.is_reconnect()) {
        return;
    }
    let reconnect = entity
        .get::<Reconnectable>()
        .copied()
        .filter(|_| promoted && awaiting.is_none());
    if let Some(reconnect) = reconnect {
        let Some(host) = world.get_resource::<HostUuid>().map(|host| host.0) else {
            end_client_session(world, lobby, LobbyLeftReason::HostGone);
            return;
        };
        info!(
            "lost the link to the host {host:#x} ({cause:?}); waiting up to {:.0}s for it to \
             reach this peer again",
            reconnect.within.as_secs_f64()
        );
        forget_host_link(world, lobby, &[host]);
        world.entity_mut(lobby).insert(AwaitingHost {
            previous: host,
            successor: Some(host),
            cause,
            waited: Duration::ZERO,
            limit: reconnect.within,
        });
        return;
    }
    let Some(waits) = waits(world, lobby).filter(|_| promoted) else {
        // Nothing to keep: a join that never finished, or a lobby nobody will name a host for.
        let reason = match cause {
            HostLossCause::Transport => LobbyLeftReason::HostGone,
            HostLossCause::Silence => LobbyLeftReason::PeerTimeout,
        };
        end_client_session(world, lobby, reason);
        return;
    };

    let waiting = match awaiting {
        // Already waiting for a successor: a transport's word only firms up why.
        Some(awaiting) if awaiting.successor.is_none() => AwaitingHost {
            cause: if cause == HostLossCause::Transport {
                HostLossCause::Transport
            } else {
                awaiting.cause
            },
            ..awaiting
        },
        // The successor was named and is gone too: wait for the next one, afresh.
        Some(awaiting) => AwaitingHost {
            previous: awaiting.successor.unwrap_or(awaiting.previous),
            successor: None,
            cause,
            waited: Duration::ZERO,
            limit: waits.successor_within,
        },
        None => {
            let Some(host) = world.get_resource::<HostUuid>().map(|host| host.0) else {
                end_client_session(world, lobby, LobbyLeftReason::HostGone);
                return;
            };
            info!(
                "the host {host:#x} is gone ({cause:?}); waiting up to {:.0}s for the lobby to \
                 name a new one",
                waits.successor_within.as_secs_f64()
            );
            AwaitingHost {
                previous: host,
                successor: None,
                cause,
                waited: Duration::ZERO,
                limit: waits.successor_within,
            }
        }
    };
    world.entity_mut(lobby).insert(waiting);
}

/// Drop everything this client's lobby knew about its link to `hosts`: packets held until they
/// were verified, protocols matched, and the [`HostLinkState`] on the lobby entity. What comes
/// next — a new host, or a new connection to the same one — is verified from scratch.
fn forget_host_link(world: &mut World, lobby: Entity, hosts: &[PlayerUUID]) {
    for uuid in hosts {
        if let Some(mut held) = world.get_mut::<HeldUntilVerified>(lobby) {
            held.discard(*uuid);
        }
        if let Some(mut matched) = world.get_mut::<ProtocolMatched>(lobby) {
            matched.0.remove(uuid);
        }
    }
    world.entity_mut(lobby).remove::<HostLinkState>();
}

pub(crate) fn on_seat_link_lost(lost: On<SeatLinkLost>, mut commands: Commands) {
    let seat = lost.entity;
    commands.queue(move |world: &mut World| seat_link_lost(world, seat));
}

/// The link to a seated client broke: hold its place in a [`Reconnectable`] lobby, and remove
/// the seat as a timeout in any other.
fn seat_link_lost(world: &mut World, seat: Entity) {
    let Ok(seat_ref) = world.get_entity(seat) else {
        return;
    };
    if !seat_ref.contains::<LobbyClient>() {
        return;
    }
    let (Some(uuid), Some(lobby)) = (
        seat_ref.get::<LobbyClientPlayerUuid>().map(|uuid| uuid.0),
        seat_ref.get::<LobbyParticipantOf>().map(|of| of.0),
    ) else {
        world.despawn(seat);
        return;
    };
    let within = world
        .get_entity(lobby)
        .ok()
        .filter(|lobby| lobby.contains::<Lobby>() && lobby.contains::<Host>())
        .and_then(|lobby| lobby.get::<Reconnectable>())
        .map(|reconnect| reconnect.within);
    let participant = world
        .query::<(Entity, &LobbyParticipant, &LobbyParticipantOf)>()
        .iter(world)
        .find(|(_, participant, of)| participant.player_uuid == uuid && of.0 == lobby)
        .map(|(participant, _, _)| participant);
    match (within, participant) {
        (Some(within), Some(participant)) => {
            info!(
                "the link to {uuid:#x} broke; holding its seat for up to {:.0}s while it is \
                 reached again",
                within.as_secs_f64()
            );
            world.entity_mut(participant).insert(AwaitingSeat {
                waited: Duration::ZERO,
                limit: within,
            });
            // In one go, so the removal observers find the mark.
            let mut seat = world.entity_mut(seat);
            seat.insert(HeldSeat);
            seat.despawn();
        }
        // Nothing to hold it in: removed, and everybody told, as a broken link always was.
        _ => {
            let mut seat = world.entity_mut(seat);
            seat.insert(SeatRemoval::TimedOut);
            seat.despawn();
        }
    }
}

pub(crate) fn on_new_host_named(named: On<NewHostNamed>, mut commands: Commands) {
    let named = named.event().clone();
    commands.queue(move |world: &mut World| new_host_named(world, named));
}

/// Promote this peer or follow the new host: one world update, so nothing observes a lobby that
/// is half one and half the other.
fn new_host_named(world: &mut World, named: NewHostNamed) {
    let NewHostNamed {
        entity: lobby,
        previous,
        new_host,
        members,
    } = named;
    let Some(local) = world
        .get_resource::<LocalMultiplayerPlayerId>()
        .map(|local| local.0)
    else {
        warn!("told {new_host:#x} hosts the lobby, but this peer does not know who it is");
        return;
    };
    let Ok(entity) = world.get_entity(lobby) else {
        return;
    };
    if entity.contains::<Host>() {
        if new_host != local {
            warn!(
                "this peer hosts the lobby and was told {new_host:#x} does; the backend ends a \
                 session it has been replaced in"
            );
        }
        return;
    }
    let pending = entity.contains::<PendingLobby>();
    let reach_within = waits(world, lobby).map_or(DEFAULT_REACH_WITHIN, |waits| waits.reach_within);
    let cause = entity
        .get::<AwaitingHost>()
        .map_or(HostLossCause::Transport, |awaiting| awaiting.cause);

    // Whatever was held from, or matched with, a host this peer will not hear from again, and
    // everything on the lobby entity that described the link to it.
    let stale: Vec<PlayerUUID> = [
        Some(previous),
        world.get_resource::<HostUuid>().map(|host| host.0),
    ]
    .into_iter()
    .flatten()
    .filter(|uuid| *uuid != new_host)
    .collect();
    forget_host_link(world, lobby, &stale);

    // The roster. Entities that stay are kept, with whatever the game put on them.
    let participants: Vec<(Entity, PlayerUUID)> = world
        .query::<(Entity, &LobbyParticipant, &LobbyParticipantOf)>()
        .iter(world)
        .filter(|(_, _, of)| of.0 == lobby)
        .map(|(entity, participant, _)| (entity, participant.player_uuid))
        .collect();
    for (participant, uuid) in participants {
        let gone = uuid == previous && uuid != local && uuid != new_host
            || members.as_ref().is_some_and(|members| {
                !members.contains(&uuid) && uuid != local && uuid != new_host
            });
        if gone {
            world.despawn(participant);
            continue;
        }
        let is_host = uuid == new_host;
        // Measured by the old host, of links to the old host.
        world
            .entity_mut(participant)
            .insert(LobbyParticipant {
                player_uuid: uuid,
                is_host,
            })
            .remove::<crate::ParticipantLink>();
        if new_host == local && !is_host {
            world.entity_mut(participant).insert(AwaitingSeat {
                waited: Duration::ZERO,
                limit: reach_within,
            });
        }
    }

    let promoted = new_host == local;
    if promoted {
        info!("this peer hosts the lobby now, in place of {previous:#x}");
        world
            .entity_mut(lobby)
            .remove::<(PendingLobby, AwaitingHost)>()
            .insert((Lobby, Host));
        world.insert_resource(HostUuid(local));
    } else {
        info!("{new_host:#x} hosts the lobby now, in place of {previous:#x}");
        world.insert_resource(HostUuid(new_host));
        if !pending {
            world.entity_mut(lobby).insert(AwaitingHost {
                previous,
                successor: Some(new_host),
                cause,
                waited: Duration::ZERO,
                limit: reach_within,
            });
        }
    }
    world.write_message(HostChanged {
        lobby,
        previous,
        new: new_host,
        promoted,
    });
}

pub(crate) fn on_participant_departed(departed: On<ParticipantDeparted>, mut commands: Commands) {
    let (lobby, uuid) = (departed.entity, departed.player_uuid);
    commands.queue(move |world: &mut World| participant_departed(world, lobby, uuid));
}

fn participant_departed(world: &mut World, lobby: Entity, uuid: PlayerUUID) {
    let Ok(entity) = world.get_entity(lobby) else {
        return;
    };
    let hosting = entity.contains::<Host>();
    if !hosting && !entity.contains::<AwaitingHost>() {
        return;
    }
    if hosting {
        let seat = world
            .query_filtered::<(Entity, &LobbyClientPlayerUuid, &LobbyParticipantOf), With<LobbyClient>>()
            .iter(world)
            .find(|(_, seat, of)| seat.0 == uuid && of.0 == lobby)
            .map(|(seat, _, _)| seat);
        if let Some(seat) = seat {
            // Removing the seat tells everyone, as a disconnect always has.
            world.despawn(seat);
            return;
        }
    }
    let participant = world
        .query::<(Entity, &LobbyParticipant, &LobbyParticipantOf)>()
        .iter(world)
        .find(|(_, participant, of)| participant.player_uuid == uuid && of.0 == lobby)
        .map(|(participant, _, _)| participant);
    let Some(participant) = participant else {
        return;
    };
    world.despawn(participant);
    if hosting {
        world.trigger(LobbyMessage::<RemoveLobbyParticipant> {
            entity: lobby,
            message: RemoveLobbyParticipant {
                player_uuid: uuid,
                reason: crate::SeatRemoval::Left,
            },
            send_mode: SendMode::Reliable,
        });
    }
}

/// A host that stopped answering and was being waited for answered again: nobody was named in its
/// place, so it is still the host.
pub(crate) fn take_back_a_silent_host(
    mut commands: Commands,
    mut pongs: MessageReader<ReceivedEnsembleMessage<EnsemblePong>>,
    waiting: Query<(Entity, &AwaitingHost)>,
) {
    for pong in pongs.read() {
        for (lobby, awaiting) in waiting.iter() {
            if awaiting.cause == HostLossCause::Silence
                && awaiting.successor.is_none()
                && pong.sender == Some(awaiting.previous)
            {
                info!(
                    "the host {:#x} answered again after {:.1}s; the session goes on",
                    awaiting.previous,
                    awaiting.waited.as_secs_f64()
                );
                // In one go with the removal, so the liveness check never sees the lobby
                // un-waiting with the old silence still on its clock.
                commands
                    .entity(lobby)
                    .try_remove::<AwaitingHost>()
                    .try_insert(PeerSilence::default());
            }
        }
    }
}

/// Advance every wait, and end what ran out.
pub(crate) fn run_host_migration_clocks(
    mut commands: Commands,
    time: Res<Time>,
    mut waiting: Query<(Entity, &mut AwaitingHost)>,
    mut seats: Query<(
        Entity,
        &mut AwaitingSeat,
        &LobbyParticipant,
        &LobbyParticipantOf,
    )>,
    hosted: Query<(), (With<Lobby>, With<Host>)>,
) {
    let delta = time.delta();
    for (lobby, mut awaiting) in waiting.iter_mut() {
        awaiting.waited += delta;
        if awaiting.waited < awaiting.limit {
            continue;
        }
        if awaiting.is_reconnect() {
            commands.queue(move |world: &mut World| reconnect_ran_out(world, lobby));
            continue;
        }
        let reason = match (awaiting.successor, awaiting.cause) {
            (None, HostLossCause::Silence) => LobbyLeftReason::PeerTimeout,
            _ => LobbyLeftReason::HostGone,
        };
        match awaiting.successor {
            Some(successor) => warn!(
                "leaving the session: could not reach the new host {successor:#x} in {:.0}s",
                awaiting.limit.as_secs_f64()
            ),
            None => warn!(
                "leaving the session: no new host was named in {:.0}s",
                awaiting.limit.as_secs_f64()
            ),
        }
        commands.queue(move |world: &mut World| end_client_session(world, lobby, reason));
    }

    for (participant, mut seat, player, of) in seats.iter_mut() {
        seat.waited += delta;
        if seat.waited < seat.limit || !hosted.contains(of.0) {
            continue;
        }
        let (lobby, uuid) = (of.0, player.player_uuid);
        warn!(
            "dropping {uuid:#x} from the lobby: no connection to it was made in {:.0}s",
            seat.limit.as_secs_f64()
        );
        commands.entity(participant).try_despawn();
        commands.entity(lobby).trigger(move |entity| LobbyMessage {
            entity,
            message: RemoveLobbyParticipant {
                player_uuid: uuid,
                reason: crate::SeatRemoval::TimedOut,
            },
            send_mode: SendMode::Reliable,
        });
    }
}

/// The host did not reach this peer again in time. A lobby that can migrate goes on to wait for
/// a successor — the host may have gone after all, and the arbiter not noticed yet — for what is
/// left of that wait; any other ends.
fn reconnect_ran_out(world: &mut World, lobby: Entity) {
    let Some(awaiting) = world.get::<AwaitingHost>(lobby).copied() else {
        return;
    };
    if !awaiting.is_reconnect() || awaiting.waited < awaiting.limit {
        return;
    }
    if let Some(waits) = waits(world, lobby)
        && waits.successor_within > awaiting.waited
    {
        info!(
            "the host {:#x} did not reach this peer again in {:.0}s; waiting for the lobby to \
             name a new one",
            awaiting.previous,
            awaiting.waited.as_secs_f64()
        );
        world.entity_mut(lobby).insert(AwaitingHost {
            successor: None,
            limit: waits.successor_within,
            ..awaiting
        });
        return;
    }
    warn!(
        "leaving the session: the host {:#x} did not reach this peer again in {:.0}s",
        awaiting.previous,
        awaiting.waited.as_secs_f64()
    );
    let reason = match awaiting.cause {
        HostLossCause::Transport => LobbyLeftReason::HostGone,
        HostLossCause::Silence => LobbyLeftReason::PeerTimeout,
    };
    end_client_session(world, lobby, reason);
}

/// A host closing its lobby tells everyone first, then leaves on the next frame, once that has
/// been flushed.
pub(crate) fn close_lobbies(
    mut commands: Commands,
    mut requests: MessageReader<CloseLobby>,
    hosted: Query<Entity, (With<Lobby>, With<Host>, Without<ClosingLobby>)>,
) {
    if requests.read().next().is_none() {
        return;
    }
    if hosted.is_empty() {
        warn!("ignoring CloseLobby: this peer hosts no lobby; a member leaves with LeaveLobby");
    }
    for lobby in hosted.iter() {
        commands
            .entity(lobby)
            .try_insert(ClosingLobby)
            .trigger(|entity| LobbyMessage::new_no_delay(entity, LobbyClosed));
    }
}

/// In [`First`]: the announcement went out in the previous frame's [`Last`], so the host can go.
pub(crate) fn finish_closing_lobbies(
    closing: Query<(), With<ClosingLobby>>,
    mut leave: MessageWriter<LeaveLobby>,
) {
    if !closing.is_empty() {
        leave.write(LeaveLobby);
    }
}
