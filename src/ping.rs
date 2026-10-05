//! Round trips and liveness.
//!
//! Two different questions, answered in two different places.
//!
//! **How long a message takes** is measured here, with [`EnsemblePing`]s that go through the
//! frame loop like any game message: through the outbound batching, through the network
//! simulator when it is on, and answered by the other side's app. That is the path a game's own
//! messages take, and what an input lead or a playout buffer has to be sized for — so it is what
//! [`PeerRtt`] and its relatives measure, and the responder's time holding the ping is reported
//! so [`PeerWireRtt`] can take it out.
//!
//! **Whether a peer is still there** is not a question for its app. A frozen app — a browser tab
//! in the background, a long load — stops answering pings while its connection is perfectly
//! healthy, and timing peers out on pongs dropped every player who looked away for a few seconds.
//! A peer is alive while *anything* is heard from it: every packet decoded from it counts, and a
//! backend whose transport answers keepalives by itself — the WebRTC socket does, from its data
//! channel handlers — reports what it heard through [`HeardFrom`] whether or not the app has read
//! it. [`PeerSilence`] is how long it has been since anything was, and [`PeerTimeout`] acts on it.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use bevy::prelude::*;

use crate::{
    Host, HostUuid, Instant, Lobby, LobbyClient, LobbyClientPlayerUuid, PendingLobby, PlayerUUID,
    ReceivedEnsembleMessage, SendMode,
    messages::{LobbyClientMessage, LobbyMessage},
    migration::{AwaitingHost, HostLossCause, host_lost},
};

/// How often each peer pings every other.
///
/// Once a second was too slow to be a signal. Two consumers need this to be a *series* and not an
/// occasional reading: the smoothed mean takes about a dozen samples to follow a genuine latency
/// shift, which at 1 Hz is fifteen seconds of a session running on a stale number, and
/// [`PeerRttJitter`] cannot exist at all without enough samples to have a spread.
///
/// Ten a second, and it costs nothing worth counting: the payload is a few bytes and it goes
/// unreliably, against a lockstep stream already sending 64 messages a second in each direction.
const PING_INTERVAL: Duration = Duration::from_millis(100);

/// How many of this peer's own pings are remembered. A pong for anything older is not matched
/// and not measured. At ten a second this is three seconds, ten times any round trip worth
/// measuring.
const OUTSTANDING_PINGS: usize = 32;

/// A round trip longer than this is not a measurement of anything a game can use, and a peer
/// that reports one is either broken or lying.
const MAX_ROUND_TRIP: Duration = Duration::from_secs(30);

/// Internal ping message sent over data channels to measure RTT.
///
/// Carries only a sequence number. The send time stays on the sender, in
/// [`OutstandingPings`], so a pong cannot claim to answer a ping that was never sent or move
/// the time it was sent at — both of which used to be possible, and one pong with a `NaN`
/// timestamp left the estimate `NaN` for the rest of the session.
#[doc(hidden)]
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnsemblePing {
    pub seq: u32,
    /// Sent on the reliable, ordered channel rather than the unreliable one. Each interval one of
    /// each goes out, because the two channels are two different paths: the reliable one carries
    /// retransmits and head-of-line blocking that a lockstep stream has to cover, and pinging
    /// only the unreliable one told a buffer sized from it that those did not exist.
    pub reliable: bool,
}

/// Internal pong response: the ping's sequence number plus how long the responder held it.
#[doc(hidden)]
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnsemblePong {
    pub seq: u32,
    pub reliable: bool,
    /// `t3 - t2` in microseconds: how long the responder spent between the ping coming off its
    /// socket seam and this pong leaving, on the responder's clock. Subtracting it from the
    /// round trip cancels the responder's clock offset and its in-app time. Clamped by the
    /// receiver to the round trip it is subtracted from, so it can only ever reduce the wire
    /// estimate to zero, never below.
    pub dwell_micros: u32,
}

/// The pings this peer has sent and would still accept an answer to: `(seq, sent_at)`.
#[derive(Resource, Debug, Default)]
pub(crate) struct OutstandingPings {
    sent: VecDeque<(u32, Instant)>,
    next_seq: u32,
}

impl OutstandingPings {
    fn issue(&mut self, now: Instant) -> u32 {
        self.next_seq = self.next_seq.wrapping_add(1);
        let seq = self.next_seq;
        self.sent.push_back((seq, now));
        while self.sent.len() > OUTSTANDING_PINGS {
            self.sent.pop_front();
        }
        seq
    }

    /// When the ping with this sequence number was sent, if it was and is still remembered.
    ///
    /// Not removed on match: a host's ping goes to every client with one sequence number, and
    /// every client's pong for it is a measurement.
    fn sent_at(&self, seq: u32) -> Option<Instant> {
        self.sent
            .iter()
            .find(|(sent_seq, _)| *sent_seq == seq)
            .map(|(_, at)| *at)
    }
}

/// Round-trip time to a connected peer, smoothed.
///
/// Added to `LobbyClient` entities on the host (one per peer) and to the
/// lobby entity on clients (single connection to the host).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerRtt(pub Duration);

/// Estimated round-trip **wire time** to a peer: the full RTT minus the time the peer spent
/// holding the ping (its [`EnsemblePong::dwell_micros`]).
///
/// This isolates and removes the remote's in-app processing. What remains is the network
/// transit plus each side's socket-poll latency and local send/receive pipeline — the app
/// cannot observe packets below the socket poll, so on a loopback/localhost connection
/// this is dominated by frame/poll alignment rather than literal cable time, and shrinks
/// toward ~0 with an uncapped frame loop. On a real remote peer it converges to the
/// genuine network RTT. Added alongside [`PeerRtt`].
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerWireRtt(pub Duration);

/// Round trip measured on the reliable, ordered channel.
///
/// Includes whatever retransmission and head-of-line delay that channel is carrying, which
/// [`PeerRtt`] — measured on the unreliable channel — cannot see. A buffer that has to cover a
/// reliable stream, as a lockstep action stream is, sizes itself from this one.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerReliableRtt(pub Duration);

/// How much a peer's round trip varies: an EMA of each raw sample's absolute deviation from the
/// smoothed mean.
///
/// # Why this has to be measured here
///
/// Because here is the only place raw samples exist. A consumer sizing a playout buffer needs the
/// spread as well as the mean — the mean says where packets land on average, and the spread says
/// how late the unlucky ones are, which is what the buffer has to cover.
///
/// Deriving it from [`PeerRtt`] instead does not work, and quietly returns near-zero rather than
/// failing. `PeerRtt` is already smoothed, so its own variation is the *smoothed* signal's
/// variation, which is precisely the thing smoothing removed;
/// `bevy_ticked_lockstep_networking`'s adaptive buffer did exactly that, applied a second EMA on
/// top, and computed its jitter headroom from a series that had been averaged twice and sampled
/// once a second. It came out as roughly nothing on links with tens of milliseconds of real
/// spread, so a buffer that was meant to carry jitter headroom carried none.
///
/// Added alongside [`PeerRtt`], on the same entities.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerRttJitter(pub Duration);

/// How long since anything was heard from a peer.
///
/// Present from the moment a peer is known — on the host's `LobbyClient` entity, on a client's
/// lobby entity — not from the first thing it says, so a peer that never says anything is timed
/// from the start. Ticked up every frame and reset to zero whenever the peer is heard from: any
/// packet decoded from it, or anything its backend reports through [`HeardFrom`]. [`PeerTimeout`]
/// is what acts on it.
///
/// Counted in frame time rather than read off a clock, which is what makes a frozen app safe on
/// both ends. The app that stalled adds at most one frame's capped delta when it wakes, so it
/// never finds a peer silent for the whole time it was away itself; and a peer whose app stalled
/// is still heard from by a backend that answers keepalives below the app, so it is never silent
/// for that time either.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerSilence(pub Duration);

/// The old name of [`PeerSilence`], from when only a pong reset it.
#[deprecated(note = "renamed `PeerSilence`: anything heard from the peer resets it now")]
pub type PeerLastPong = PeerSilence;

/// Peers heard from since liveness last looked, by their [`PlayerUUID`].
///
/// Every decoded packet marks its sender here. A backend whose transport hears from peers below
/// the app — keepalives answered by the socket itself — marks them too, each frame, so that a peer
/// whose app is frozen but whose connection is not stays alive. Drained every frame into
/// [`PeerSilence`].
#[derive(Resource, Debug, Default)]
pub struct HeardFrom(HashSet<PlayerUUID>);

impl HeardFrom {
    /// Something arrived from `peer`.
    pub fn mark(&mut self, peer: PlayerUUID) {
        self.0.insert(peer);
    }
}

/// The newest pong sequence number accepted from a peer, so a duplicate or a replay of an old
/// pong cannot be folded into the estimate twice.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct PeerLastPongSeq(pub u32);

/// How long a peer may go without being heard from before it is treated as gone.
///
/// On the host, a client past this is despawned as if it had disconnected — with
/// [`SeatRemoval::TimedOut`] on it, so it is told it timed out rather than that it was kicked —
/// and everyone else is told it left. On a client, a host past this ends the session with
/// [`LobbyLeft { reason: PeerTimeout }`](crate::LobbyLeft) — or, in a lobby that can migrate,
/// starts the wait for a new host, during which an answer from the old one still counts. `None`
/// disables the check.
///
/// The transport's own disconnect detection is not enough on its own: a NAT binding that
/// expired, or a process that died while its OS keeps acknowledging, look connected to the
/// transport for as long as it takes ICE to give up, which is many seconds and sometimes never.
///
/// What this measures is the connection, not the app — see [`PeerSilence`] — so the timeout only
/// has to outlast a bad patch on a real link: ten seconds rides out a Wi-Fi drop or a burst of
/// loss, and a player who really left stands in the session no longer than that. It used to be
/// five seconds of *pongs*, which a backgrounded tab could not send, and players were dropped for
/// looking away.
#[derive(Resource, Debug, Clone, Copy)]
pub struct PeerTimeout(pub Option<Duration>);

/// Why a host is removing a client's seat, on the `LobbyClient` entity as it is despawned.
///
/// The removal observers read it to tell the player which it was. Absent means
/// [`Kicked`](Self::Kicked): despawning a seat is how a game kicks.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SeatRemoval {
    Kicked,
    /// Nothing was heard from the client for [`PeerTimeout`], or it never reached a new host.
    TimedOut,
    /// The player went of their own accord. Only ever news to everybody else.
    Left,
}

/// Extra silence a peer may have before [`PeerTimeout`] acts on it.
///
/// A transport that knows the silence has a cause and an end -- an ICE restart in flight --
/// inserts this on the peer's entity (the `LobbyClient` on a host, the lobby on a client) and
/// removes it when the path is back. Without it the liveness check would end the session before a
/// restart that takes fifteen seconds had a chance, and the restart would be pointless. Capped:
/// the extra is added to the timeout, not substituted for it, so a peer that never comes back
/// still goes.
#[derive(Component, Debug, Clone, Copy)]
pub struct LivenessGrace {
    /// Added to [`PeerTimeout`] while present.
    pub extra: Duration,
}

impl Default for PeerTimeout {
    fn default() -> Self {
        Self(Some(Duration::from_secs(10)))
    }
}

/// Both host and client send pings to all connected peers ten times a second.
///
/// Uses the standard [`LobbyMessage`] pipeline so pings are routed through
/// whichever transport backend is active.
pub(crate) fn send_pings(
    mut commands: Commands,
    lobbies: Query<(Entity, Option<&AwaitingHost>), With<Lobby>>,
    time: Res<Time>,
    mut outstanding: ResMut<OutstandingPings>,
    mut cadence: Local<crate::Cadence>,
) {
    if lobbies.is_empty() {
        // The next session starts its own clock rather than inheriting what was left of this one.
        cadence.reset();
        return;
    }
    if !cadence.tick(time.delta(), PING_INTERVAL) {
        return;
    }

    let seq = outstanding.issue(Instant::now());
    for (lobby, awaiting) in lobbies.iter() {
        // A lobby waiting on a host that has been named but not reached has nobody to ping. One
        // that has lost its host and heard of no successor keeps pinging the old one: an answer
        // is how a host that only froze is taken back.
        if awaiting.is_some_and(|awaiting| awaiting.successor.is_some()) {
            continue;
        }
        commands.entity(lobby).trigger(move |entity| LobbyMessage {
            entity,
            message: EnsemblePing {
                seq,
                reliable: false,
            },
            send_mode: SendMode::Unreliable,
        });
        // The reliable twin goes with no delay: a ping held to be packed with the next message
        // measures the packing, not the path.
        commands.entity(lobby).trigger(move |entity| LobbyMessage {
            entity,
            message: EnsemblePing {
                seq,
                reliable: true,
            },
            send_mode: SendMode::ReliableNoDelay,
        });
    }
}

/// When we receive a ping, immediately echo it back as a pong.
///
/// On the host: sends a targeted [`LobbyClientMessage`] back to the specific
/// client that sent the ping.
/// On a client: sends a [`LobbyMessage`] which routes to the host.
pub(crate) fn respond_to_pings(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<EnsemblePing>>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    lobby_clients: Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
) {
    for message in messages.read() {
        let Some(sender) = message.sender else {
            continue;
        };
        // t2 = when the ping came off our socket; t3 = now, as we emit the pong. Both are
        // instants, so the dwell is the time this app actually held the packet — a frame's
        // worth on a game at 60 Hz — and not, as it was when both reads came from the same
        // frame's `Time`, identically zero.
        let t2 = message.received_at;
        let dwell = Instant::now().saturating_duration_since(t2);
        let send_mode = if message.message.reliable {
            SendMode::ReliableNoDelay
        } else {
            SendMode::Unreliable
        };
        let pong = EnsemblePong {
            seq: message.message.seq,
            reliable: message.message.reliable,
            dwell_micros: dwell.as_micros().min(u128::from(u32::MAX)) as u32,
        };

        if host_lobby.is_some() {
            // Host: respond to the specific client that sent this ping
            if let Some((client_entity, _)) =
                lobby_clients.iter().find(|(_, uuid)| uuid.0 == sender)
            {
                commands
                    .entity(client_entity)
                    .trigger(move |entity| LobbyClientMessage {
                        entity,
                        message: pong,
                        send_mode,
                    });
            }
        } else if let Some(lobby) = client_lobby.as_ref() {
            // Client: respond to host via lobby message
            commands
                .entity(**lobby)
                .trigger(move |entity| LobbyMessage {
                    entity,
                    message: pong,
                    send_mode,
                });
        }
    }
}

/// Exponential smoothing factor for RTT samples (weight given to the new sample).
const RTT_SMOOTHING: f64 = 0.2;

/// Blend a new sample into the previous smoothed value (or seed it on the first sample).
fn smooth(previous: Option<f64>, sample: f64) -> f64 {
    match previous {
        Some(prev) => (1.0 - RTT_SMOOTHING) * prev + RTT_SMOOTHING * sample,
        None => sample,
    }
}

/// [`smooth`], on durations. The blend is done in `f64` seconds: a weighted mean of two
/// durations, ten times a second per peer, where the rounding back to nanoseconds is far below
/// anything measured.
fn smooth_duration(previous: Option<Duration>, sample: Duration) -> Duration {
    Duration::from_secs_f64(smooth(
        previous.map(|previous| previous.as_secs_f64()),
        sample.as_secs_f64(),
    ))
}

/// [`smooth_jitter`], on durations.
fn smooth_jitter_duration(
    previous_jitter: Option<Duration>,
    previous_mean: Option<Duration>,
    sample: Duration,
) -> Duration {
    Duration::from_secs_f64(smooth_jitter(
        previous_jitter.map(|jitter| jitter.as_secs_f64()),
        previous_mean.map(|mean| mean.as_secs_f64()),
        sample.as_secs_f64(),
    ))
}

/// Fold one raw sample into the jitter estimate.
///
/// The deviation is taken against the *previous* mean rather than the updated one, so the sample
/// being measured has not already been folded into the thing it is measured against — otherwise a
/// large sample partly moves the mean toward itself and reports a smaller deviation than it is.
///
/// Zero on the first sample: one reading has a mean and no spread, and seeding the estimate with
/// its distance from nothing would claim an enormous one.
fn smooth_jitter(previous_jitter: Option<f64>, previous_mean: Option<f64>, sample: f64) -> f64 {
    let Some(previous_mean) = previous_mean else {
        return 0.0;
    };
    let deviation = (sample - previous_mean).abs();
    match previous_jitter {
        Some(prev) => (1.0 - RTT_SMOOTHING) * prev + RTT_SMOOTHING * deviation,
        None => deviation,
    }
}

/// One accepted pong, reduced to the two numbers the estimators fold.
struct Sample {
    e2e: Duration,
    wire: Duration,
}

/// Turn a pong into a sample, or say why it is not one.
///
/// Everything a peer controls is checked here: the sequence number must be one this peer sent
/// and still remembers, the resulting round trip must be finite, non-negative and plausible, and
/// the dwell the peer claims is clamped into the round trip it is subtracted from.
fn sample_from(
    outstanding: &OutstandingPings,
    pong: &EnsemblePong,
    received_at: Instant,
) -> Option<Sample> {
    let sent_at = outstanding.sent_at(pong.seq)?;
    let e2e = received_at.checked_duration_since(sent_at)?;
    if e2e > MAX_ROUND_TRIP {
        return None;
    }
    // In integer time, so a dwell can neither be negative nor take the wire estimate below zero.
    let dwell = Duration::from_micros(u64::from(pong.dwell_micros)).min(e2e);
    Some(Sample {
        e2e,
        wire: e2e - dwell,
    })
}

/// When we receive a pong, compute the round trip and store both the full end-to-end RTT
/// ([`PeerRtt`]) and the wire estimate ([`PeerWireRtt`], the RTT minus the peer's dwell).
///
/// On the host: updates the components on the `LobbyClient` entity for that peer.
/// On clients: updates them on the lobby entity itself.
pub(crate) fn receive_pongs(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<EnsemblePong>>,
    outstanding: Res<OutstandingPings>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    lobby_clients: Query<
        (
            Entity,
            &LobbyClientPlayerUuid,
            Option<&PeerRtt>,
            Option<&PeerWireRtt>,
            Option<&PeerRttJitter>,
            Option<&PeerReliableRtt>,
            Option<&PeerLastPongSeq>,
        ),
        With<LobbyClient>,
    >,
    client_lobby_rtt: Query<
        (
            Option<&PeerRtt>,
            Option<&PeerWireRtt>,
            Option<&PeerRttJitter>,
            Option<&PeerReliableRtt>,
            Option<&PeerLastPongSeq>,
        ),
        (With<Lobby>, Without<Host>),
    >,
) {
    // A peer may answer one sequence number once per channel. Tracked per run as well as per
    // entity, because two pongs in one frame both see the pre-run component.
    let mut accepted_this_run: Vec<(u128, u32, bool)> = Vec::new();

    for message in messages.read() {
        let pong = message.message;
        let Some(sender) = message.sender else {
            continue;
        };
        let Some(sample) = sample_from(&outstanding, &pong, message.received_at) else {
            debug!("ignoring a pong from {sender:#x} that answers no ping this peer sent");
            continue;
        };
        if accepted_this_run.contains(&(sender, pong.seq, pong.reliable)) {
            continue;
        }

        let (entity, prev_rtt, prev_wire, prev_jitter, prev_reliable, prev_seq) =
            if host_lobby.is_some() {
                let Some((entity, _, prev_rtt, prev_wire, prev_jitter, prev_reliable, prev_seq)) =
                    lobby_clients.iter().find(|(_, uuid, ..)| uuid.0 == sender)
                else {
                    continue;
                };
                (
                    entity,
                    prev_rtt,
                    prev_wire,
                    prev_jitter,
                    prev_reliable,
                    prev_seq,
                )
            } else if let Some(lobby_entity) = client_lobby.as_ref() {
                let (prev_rtt, prev_wire, prev_jitter, prev_reliable, prev_seq) = client_lobby_rtt
                    .get(**lobby_entity)
                    .unwrap_or((None, None, None, None, None));
                (
                    **lobby_entity,
                    prev_rtt,
                    prev_wire,
                    prev_jitter,
                    prev_reliable,
                    prev_seq,
                )
            } else {
                continue;
            };

        // The reliable and unreliable pongs for one sequence number arrive separately and both
        // count; a second pong on the *same* channel for a sequence already folded does not.
        // The sequence high-water mark is per channel pair, kept on the unreliable one.
        if !pong.reliable && prev_seq.is_some_and(|last| pong.seq <= last.0) {
            continue;
        }
        accepted_this_run.push((sender, pong.seq, pong.reliable));

        // `try_`: a seat can be despawned in the same frame its pong is read — a liveness
        // timeout, a disconnect — and an insert on an entity that is gone is an error.
        if pong.reliable {
            commands
                .entity(entity)
                .try_insert(PeerReliableRtt(smooth_duration(
                    prev_reliable.map(|p| p.0),
                    sample.e2e,
                )));
            continue;
        }

        let previous_mean = prev_rtt.map(|p| p.0);
        commands.entity(entity).try_insert((
            PeerRtt(smooth_duration(previous_mean, sample.e2e)),
            PeerWireRtt(smooth_duration(prev_wire.map(|p| p.0), sample.wire)),
            // Folded from the raw `e2e` against the mean as it stood *before* this sample.
            PeerRttJitter(smooth_jitter_duration(
                prev_jitter.map(|p| p.0),
                previous_mean,
                sample.e2e,
            )),
            PeerLastPongSeq(pong.seq),
        ));
    }
}

/// Start the liveness clock the moment a peer is known, not the moment it first speaks.
///
/// A client that connected and never answered used to have no silence clock at all, and so could
/// never time out; the only peers that could be found dead were ones that had once been alive.
pub(crate) fn arm_peer_liveness(
    mut commands: Commands,
    new_clients: Query<Entity, (Added<LobbyClient>, Without<PeerSilence>)>,
    new_client_lobbies: Query<Entity, (Added<Lobby>, Without<Host>, Without<PeerSilence>)>,
) {
    for entity in new_clients.iter().chain(new_client_lobbies.iter()) {
        commands.entity(entity).try_insert(PeerSilence::default());
    }
}

/// Add this frame to every peer's [`PeerSilence`].
pub(crate) fn tick_silence(time: Res<Time>, mut peers: Query<&mut PeerSilence>) {
    let delta = time.delta();
    for mut silence in peers.iter_mut() {
        silence.0 += delta;
    }
}

/// End the silence of every peer heard from this frame.
///
/// On a host, a peer is the `LobbyClient` with its uuid. On a client, the only peer whose
/// silence is kept is the host, on the lobby entity; anything from anybody else says nothing
/// about the host.
pub(crate) fn hear_peers(
    mut heard: ResMut<HeardFrom>,
    host_uuid: Option<Res<HostUuid>>,
    host_lobby: Option<Single<(), (With<Lobby>, With<Host>)>>,
    mut clients: Query<(&LobbyClientPlayerUuid, &mut PeerSilence), With<LobbyClient>>,
    mut client_lobbies: Query<
        &mut PeerSilence,
        (
            Or<(With<Lobby>, With<PendingLobby>)>,
            Without<Host>,
            Without<LobbyClient>,
        ),
    >,
) {
    if heard.0.is_empty() {
        return;
    }
    if host_lobby.is_some() {
        for (uuid, mut silence) in clients.iter_mut() {
            if heard.0.contains(&uuid.0) {
                silence.0 = Duration::ZERO;
            }
        }
    } else {
        let from_host = host_uuid
            .as_ref()
            .is_none_or(|host| heard.0.contains(&host.0));
        if from_host {
            for mut silence in client_lobbies.iter_mut() {
                silence.0 = Duration::ZERO;
            }
        }
    }
    heard.0.clear();
}

/// Act on [`PeerTimeout`].
///
/// On the host, a client that has not answered in time is despawned exactly as a backend does
/// on disconnect, so `on_lobby_client_removed` tells everyone else and the roster shrinks. On a
/// client, a host that has not answered in time is lost: a lobby that can migrate waits for a
/// successor (see [`AwaitingHost`]), and any other ends the session — the lobby goes, the identity
/// goes with it (as it does when a backend loses the host), and `LobbyLeft` says why.
pub(crate) fn detect_dead_peers(
    mut commands: Commands,
    timeout: Res<PeerTimeout>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    clients: Query<
        (
            Entity,
            &LobbyClientPlayerUuid,
            &PeerSilence,
            Option<&LivenessGrace>,
        ),
        With<LobbyClient>,
    >,
    client_lobbies: Query<
        (Entity, &PeerSilence, Option<&LivenessGrace>),
        (
            Or<(With<Lobby>, With<PendingLobby>)>,
            Without<Host>,
            Without<AwaitingHost>,
        ),
    >,
) {
    let Some(base) = timeout.0 else {
        return;
    };
    let limit_for =
        |grace: Option<&LivenessGrace>| base + grace.map_or(Duration::ZERO, |grace| grace.extra);

    if host_lobby.is_some() {
        for (entity, uuid, silence, grace) in clients.iter() {
            let limit = limit_for(grace);
            if silence.0 > limit {
                info!(
                    "dropping client {:#x}: nothing heard from it for {:.1?} (limit {limit:.1?})",
                    uuid.0, silence.0
                );
                // Inserted before the despawn, in one command, so the removal observers find it.
                commands
                    .entity(entity)
                    .try_insert(SeatRemoval::TimedOut)
                    .try_despawn();
            }
        }
        return;
    }

    // A lobby already waiting for a host is on the migration's clock instead of this one.
    for (entity, silence, grace) in client_lobbies.iter() {
        let limit = limit_for(grace);
        if silence.0 > limit {
            warn!(
                "nothing heard from the host for {:.1?} (limit {limit:.1?})",
                silence.0
            );
            // Waits for a successor if the lobby can have one, and ends the session as a
            // `PeerTimeout` if it cannot.
            commands.queue(move |world: &mut World| {
                host_lost(world, entity, HostLossCause::Silence);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a series of raw round trips through both estimators, as `receive_pongs` does.
    fn estimate(samples: &[f64]) -> (f64, f64) {
        let (mut mean, mut jitter) = (None, None);
        for sample in samples {
            jitter = Some(smooth_jitter(jitter, mean, *sample));
            mean = Some(smooth(mean, *sample));
        }
        (mean.unwrap_or(0.0), jitter.unwrap_or(0.0))
    }

    #[test]
    fn one_sample_has_a_mean_and_no_spread() {
        let (mean, jitter) = estimate(&[0.050]);
        assert_eq!(mean, 0.050);
        assert_eq!(
            jitter, 0.0,
            "a first sample has nothing to deviate from; seeding the estimate with its distance \
             from zero would claim a 50ms spread on a link that has shown none"
        );
    }

    #[test]
    fn a_steady_link_reports_no_spread() {
        let (mean, jitter) = estimate(&[0.050; 40]);
        assert!((mean - 0.050).abs() < 0.001);
        assert!(
            jitter < 0.001,
            "a link that always answers in 50ms is not jittery, and reported {jitter}"
        );
    }

    #[test]
    fn an_unsteady_link_reports_the_spread_and_not_the_mean() {
        // Same mean as above, ±20ms around it.
        let samples: Vec<f64> = (0..40)
            .map(|index| if index % 2 == 0 { 0.030 } else { 0.070 })
            .collect();
        let (mean, jitter) = estimate(&samples);

        assert!(
            (mean - 0.050).abs() < 0.005,
            "the mean should be unmoved by symmetric jitter, and was {mean}"
        );
        assert!(
            jitter > 0.010,
            "±20ms of spread reported as {jitter} — this is the number a playout buffer sizes its \
             headroom from, and a buffer given zero headroom stalls on every late packet"
        );
    }

    #[test]
    fn the_spread_is_measured_against_the_mean_before_the_sample_joined_it() {
        // Folding the sample into the mean first drags the mean toward it, so the deviation comes
        // out smaller than it was — the estimator would under-report exactly the large samples it
        // exists to catch.
        let naive = {
            let (mut mean, mut jitter) = (None, None);
            for sample in [0.050, 0.050, 0.050, 0.150] {
                mean = Some(smooth(mean, sample));
                jitter = Some(smooth_jitter(jitter, mean, sample));
            }
            jitter.unwrap()
        };
        let (_, correct) = estimate(&[0.050, 0.050, 0.050, 0.150]);

        assert!(
            correct > naive,
            "measuring against the updated mean under-reports the spike ({naive} vs {correct})"
        );
    }

    fn epoch() -> Instant {
        Instant::now()
    }

    fn after(base: Instant, secs: f64) -> Instant {
        base + Duration::from_secs_f64(secs)
    }

    fn outstanding_at(base: Instant, offsets: &[f64]) -> OutstandingPings {
        let mut outstanding = OutstandingPings::default();
        for at in offsets {
            outstanding.issue(after(base, *at));
        }
        outstanding
    }

    fn pong(seq: u32, dwell_micros: u32) -> EnsemblePong {
        EnsemblePong {
            seq,
            reliable: false,
            dwell_micros,
        }
    }

    #[test]
    fn an_unsolicited_pong_is_not_a_sample() {
        let base = epoch();
        let outstanding = outstanding_at(base, &[1.0]);
        assert!(sample_from(&outstanding, &pong(999, 0), after(base, 1.05)).is_none());
    }

    #[test]
    fn a_pong_answering_a_real_ping_is_measured_from_our_own_clock() {
        let base = epoch();
        let outstanding = outstanding_at(base, &[1.0]);
        let sample =
            sample_from(&outstanding, &pong(1, 10_000), after(base, 1.05)).expect("a real ping");
        assert!((sample.e2e.as_secs_f64() - 0.05).abs() < 1e-6);
        assert!((sample.wire.as_secs_f64() - 0.04).abs() < 1e-6);
    }

    #[test]
    fn a_claimed_dwell_cannot_exceed_the_round_trip() {
        // The peer controls the dwell it reports. A dwell longer than the round trip would make
        // the wire estimate negative, and a huge one used to zero it for the rest of the session.
        let base = epoch();
        let outstanding = outstanding_at(base, &[1.0]);
        let sample = sample_from(&outstanding, &pong(1, u32::MAX), after(base, 1.05))
            .expect("still a real ping");
        assert_eq!(sample.wire, Duration::ZERO);
        assert!((sample.e2e.as_secs_f64() - 0.05).abs() < 1e-6);
    }

    #[test]
    fn an_absurd_round_trip_is_not_a_sample() {
        let base = epoch();
        let outstanding = outstanding_at(base, &[1.0]);
        assert!(
            sample_from(
                &outstanding,
                &pong(1, 0),
                after(base, 1.0 + MAX_ROUND_TRIP.as_secs_f64() + 1.0)
            )
            .is_none()
        );
        assert!(
            sample_from(&outstanding, &pong(1, 0), after(base, 0.5)).is_none(),
            "answered before it was sent"
        );
    }

    #[test]
    fn old_pings_are_forgotten() {
        let base = epoch();
        let mut outstanding = OutstandingPings::default();
        for i in 0..(OUTSTANDING_PINGS as u32 + 5) {
            outstanding.issue(after(base, f64::from(i)));
        }
        assert!(
            outstanding.sent_at(1).is_none(),
            "the oldest have been forgotten"
        );
        assert!(outstanding.sent_at(OUTSTANDING_PINGS as u32 + 5).is_some());
    }
}
