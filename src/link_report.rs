//! Everybody's connection to the host, as the host measures it, told to everybody.
//!
//! A session is a star, so only the host knows how every player is connected: it holds a
//! [`PeerRtt`] and a [`PeerRoute`] for each client, and a client holds one of each, for its own
//! link. That is enough for a player to see their own ping and not enough for a scoreboard, where
//! the useful question is *whose* connection is the slow one, or which of three people on the
//! same office network is going through a relay when the other two are not.
//!
//! So the host sends what it has, about once a second, and every peer — host included — keeps it
//! as a [`ParticipantLink`] on the participant it describes.

use std::time::Duration;

use bevy::prelude::*;

use crate::{
    Host, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyParticipant, LobbyParticipantOf,
    PeerRoute, PeerRtt, PeerWireRtt, PlayerUUID, ReceivedEnsembleMessage, SendMode,
    messages::LobbyMessage,
};

/// How often the host sends the report. A scoreboard reading, not a signal anything steers by:
/// once a second is fresh enough to watch a ping move, and costs a few dozen bytes.
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// A participant's connection to the host, as the host last measured it.
///
/// On [`LobbyParticipant`] entities, on every peer, for every participant the host has a
/// measurement of. The host's own participant never has one: it has no link to itself. Absent
/// until the first report after a join, and removed from everybody when the host changes — the
/// numbers described the old host's links.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct ParticipantLink {
    /// Round trip to the host: the host's [`PeerRtt`] for this participant.
    pub rtt: Duration,
    /// The network's part of that round trip: the host's [`PeerWireRtt`], which
    /// leaves out how long the participant's app held the ping before answering.
    ///
    /// The number to show as somebody's ping. [`rtt`](Self::rtt) also counts the time the ping
    /// waited for their next frame, so it rises with a slow machine's frame time — and on a
    /// scoreboard a player at 30 fps looked 20 ms worse connected than one at 144 on the same line.
    /// The same as `rtt` until the host has a wire measurement.
    pub wire_rtt: Duration,
    /// How this participant reaches the host. `None` until ICE has settled, or on a transport
    /// that does not report it.
    pub route: Option<PeerRoute>,
}

/// The host's report: one entry per client it has measured.
#[doc(hidden)]
#[derive(Message, Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ParticipantLinks(pub Vec<LinkEntry>);

#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LinkEntry {
    pub player: PlayerUUID,
    /// Microseconds, so the wire carries an integer and a report cannot smuggle in a `NaN`.
    pub rtt_micros: u32,
    pub wire_micros: u32,
    pub route: Option<PeerRoute>,
}

impl LinkEntry {
    fn link(self) -> ParticipantLink {
        ParticipantLink {
            rtt: Duration::from_micros(u64::from(self.rtt_micros)),
            wire_rtt: Duration::from_micros(u64::from(self.wire_micros)),
            route: self.route,
        }
    }
}

/// A duration as whole microseconds, for the wire: never past `u32::MAX`, an hour and ten minutes.
fn micros(duration: Duration) -> u32 {
    u32::try_from(duration.as_micros()).unwrap_or(u32::MAX)
}

type Participants<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static LobbyParticipant,
        &'static LobbyParticipantOf,
        Option<&'static ParticipantLink>,
    ),
>;

/// Make `lobby`'s participants carry exactly what `entries` says: a link for each one listed,
/// and none for anybody who is not. Written only on a change, so a game can watch
/// `Changed<ParticipantLink>`.
fn apply(
    commands: &mut Commands,
    lobby: Entity,
    entries: &[LinkEntry],
    participants: &Participants,
) {
    for (entity, participant, of, current) in participants.iter() {
        if of.0 != lobby {
            continue;
        }
        let wanted = entries
            .iter()
            .find(|entry| entry.player == participant.player_uuid)
            .map(|entry| entry.link());
        match (wanted, current) {
            (Some(wanted), current) if current != Some(&wanted) => {
                // `try_`: a participant this frame's roster removed is still in the query.
                commands.entity(entity).try_insert(wanted);
            }
            (None, Some(_)) => {
                commands.entity(entity).try_remove::<ParticipantLink>();
            }
            _ => {}
        }
    }
}

/// Host: gather every client's round trip and route, keep it, and send it to everyone.
pub(crate) fn send_link_reports(
    mut commands: Commands,
    time: Res<Time>,
    mut cadence: Local<crate::Cadence>,
    host: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    clients: Query<
        (
            &LobbyClientPlayerUuid,
            &LobbyParticipantOf,
            &PeerRtt,
            Option<&PeerWireRtt>,
            Option<&PeerRoute>,
        ),
        With<LobbyClient>,
    >,
    participants: Participants,
) {
    let Some(host) = host else {
        // Not hosting: the next lobby this peer hosts reports on its first frame, rather than
        // whenever the last one's clock would have come round.
        cadence.reset();
        return;
    };
    let lobby = *host;
    if !cadence.tick(time.delta(), REPORT_INTERVAL) {
        return;
    }

    let entries: Vec<LinkEntry> = clients
        .iter()
        .filter(|(_, of, ..)| of.0 == lobby)
        .map(|(uuid, _, rtt, wire, route)| LinkEntry {
            player: uuid.0,
            rtt_micros: micros(rtt.0),
            wire_micros: micros(wire.map_or(rtt.0, |wire| wire.0).min(rtt.0)),
            route: route.copied(),
        })
        .collect();

    apply(&mut commands, lobby, &entries, &participants);
    // Unreliable: the next report replaces a lost one within the second.
    commands.entity(lobby).trigger(move |entity| LobbyMessage {
        entity,
        message: ParticipantLinks(entries),
        send_mode: SendMode::Unreliable,
    });
}

/// Client: keep the host's latest report.
pub(crate) fn receive_link_reports(
    mut commands: Commands,
    mut reports: MessageReader<ReceivedEnsembleMessage<ParticipantLinks>>,
    lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    participants: Participants,
) {
    // Only the newest matters; two in one frame is a late one catching up with its successor.
    let Some(report) = reports.read().last() else {
        return;
    };
    let Some(lobby) = lobby else {
        return;
    };
    apply(&mut commands, *lobby, &report.message.0, &participants);
}
