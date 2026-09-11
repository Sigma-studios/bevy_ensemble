use std::collections::VecDeque;
use std::time::Duration;

use bevy::prelude::*;

use crate::{
    Host, Lobby, LobbyClient, LobbyClientPlayerUuid, LocalMultiplayerPlayerId, PendingLobby,
    ReceivedEnsembleMessage, SendMode,
    messages::{LobbyClientMessage, LobbyMessage},
    session::{LobbyLeft, LobbyLeftReason},
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
const PING_INTERVAL_SECS: f32 = 0.1;

/// How many of this peer's own pings are remembered. A pong for anything older is not matched
/// and not measured. At ten a second this is three seconds, ten times any round trip worth
/// measuring.
const OUTSTANDING_PINGS: usize = 32;

/// A round trip longer than this is not a measurement of anything a game can use, and a peer
/// that reports one is either broken or lying.
const MAX_ROUND_TRIP_SECS: f64 = 30.0;

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
}

/// Internal pong response: the ping's sequence number plus how long the responder held it.
#[doc(hidden)]
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnsemblePong {
    pub seq: u32,
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
    sent: VecDeque<(u32, f64)>,
    next_seq: u32,
}

impl OutstandingPings {
    fn issue(&mut self, now: f64) -> u32 {
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
    fn sent_at(&self, seq: u32) -> Option<f64> {
        self.sent
            .iter()
            .find(|(sent_seq, _)| *sent_seq == seq)
            .map(|(_, at)| *at)
    }
}

/// Round-trip time to a connected peer, in seconds.
///
/// Added to `LobbyClient` entities on the host (one per peer) and to the
/// lobby entity on clients (single connection to the host).
#[derive(Component, Debug, Clone, Copy)]
pub struct PeerRtt(pub f64);

/// Estimated round-trip **wire time** to a peer, in seconds: the full RTT minus the time
/// the peer spent holding the ping (its [`EnsemblePong::dwell_micros`]).
///
/// This isolates and removes the remote's in-app processing. What remains is the network
/// transit plus each side's socket-poll latency and local send/receive pipeline — the app
/// cannot observe packets below the socket poll, so on a loopback/localhost connection
/// this is dominated by frame/poll alignment rather than literal cable time, and shrinks
/// toward ~0 with an uncapped frame loop. On a real remote peer it converges to the
/// genuine network RTT. Added alongside [`PeerRtt`].
#[derive(Component, Debug, Clone, Copy)]
pub struct PeerWireRtt(pub f64);

/// How much a peer's round trip varies, in seconds: an EMA of each raw sample's absolute
/// deviation from the smoothed mean.
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
#[derive(Component, Debug, Clone, Copy)]
pub struct PeerRttJitter(pub f64);

/// Seconds elapsed since the last pong was received from a peer.
///
/// Present from the moment a peer is known — on the host's `LobbyClient` entity, on a client's
/// lobby entity — not from its first pong, so a peer that never answers is timed from the
/// start. Reset to `0.0` on each pong and ticked up every frame; [`PeerTimeout`] is what acts on
/// it.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct PeerLastPong(pub f64);

/// The newest pong sequence number accepted from a peer, so a duplicate or a replay of an old
/// pong cannot be folded into the estimate twice.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct PeerLastPongSeq(pub u32);

/// How long a peer may go without answering a ping before it is treated as gone.
///
/// On the host, a client past this is despawned as if it had disconnected, which tells everyone
/// else. On a client, a host past this ends the session with
/// [`LobbyLeft { reason: PeerTimeout }`](crate::LobbyLeft). `None` disables the check.
///
/// The transport's own disconnect detection is not enough on its own: a NAT binding that
/// expired, a process that froze while its OS keeps acknowledging, or a tab in the background
/// all look connected to the transport for as long as it takes ICE to give up, which is many
/// seconds and sometimes never. Five seconds is long enough that no link a game would play on
/// trips it — the satellite preset is a 600 ms round trip — and short enough that a session does
/// not sit waiting on somebody who is not there.
#[derive(Resource, Debug, Clone, Copy)]
pub struct PeerTimeout(pub Option<Duration>);

impl Default for PeerTimeout {
    fn default() -> Self {
        Self(Some(Duration::from_secs(5)))
    }
}

/// Both host and client send pings to all connected peers ten times a second.
///
/// Uses the standard [`LobbyMessage`] pipeline so pings are routed through
/// whichever transport backend is active.
pub(crate) fn send_pings(
    mut commands: Commands,
    lobbies: Query<Entity, With<Lobby>>,
    time: Res<Time>,
    mut outstanding: ResMut<OutstandingPings>,
    mut cooldown: Local<f32>,
) {
    if lobbies.is_empty() {
        return;
    }
    *cooldown -= time.delta_secs();
    if *cooldown > 0.0 {
        return;
    }
    *cooldown = PING_INTERVAL_SECS;

    let seq = outstanding.issue(time.elapsed_secs_f64());
    for lobby in lobbies.iter() {
        commands
            .entity(lobby)
            .trigger(move |entity| LobbyMessage {
                entity,
                message: EnsemblePing { seq },
                send_mode: SendMode::Unreliable,
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
    time: Res<Time>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    lobby_clients: Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
) {
    for message in messages.read() {
        let Some(sender) = message.sender else {
            continue;
        };
        // t2 = when the ping came off our socket; t3 = now (as we emit the pong).
        let t2 = message.received_at.as_secs_f64();
        let t3 = time.elapsed_secs_f64();
        let dwell = (t3 - t2).max(0.0);
        let pong = EnsemblePong {
            seq: message.message.seq,
            dwell_micros: (dwell * 1_000_000.0).min(f64::from(u32::MAX)) as u32,
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
                        send_mode: SendMode::Unreliable,
                    });
            }
        } else if let Some(lobby) = client_lobby.as_ref() {
            // Client: respond to host via lobby message
            commands
                .entity(**lobby)
                .trigger(move |entity| LobbyMessage {
                    entity,
                    message: pong,
                    send_mode: SendMode::Unreliable,
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
    e2e: f64,
    wire: f64,
}

/// Turn a pong into a sample, or say why it is not one.
///
/// Everything a peer controls is checked here: the sequence number must be one this peer sent
/// and still remembers, the resulting round trip must be finite, non-negative and plausible, and
/// the dwell the peer claims is clamped into the round trip it is subtracted from.
fn sample_from(
    outstanding: &OutstandingPings,
    pong: &EnsemblePong,
    received_at: f64,
) -> Option<Sample> {
    let sent_at = outstanding.sent_at(pong.seq)?;
    let e2e = received_at - sent_at;
    if !e2e.is_finite() || !(0.0..=MAX_ROUND_TRIP_SECS).contains(&e2e) {
        return None;
    }
    let dwell = (f64::from(pong.dwell_micros) / 1_000_000.0).clamp(0.0, e2e);
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
            Option<&PeerLastPongSeq>,
        ),
        With<LobbyClient>,
    >,
    client_lobby_rtt: Query<
        (
            Option<&PeerRtt>,
            Option<&PeerWireRtt>,
            Option<&PeerRttJitter>,
            Option<&PeerLastPongSeq>,
        ),
        (With<Lobby>, Without<Host>),
    >,
) {
    // A peer may answer one sequence number once. Tracked per run as well as per entity, because
    // two pongs in one frame both see the pre-run component.
    let mut accepted_this_run: Vec<(u128, u32)> = Vec::new();

    for message in messages.read() {
        let pong = message.message;
        let Some(sender) = message.sender else {
            continue;
        };
        let Some(sample) = sample_from(&outstanding, &pong, message.received_at.as_secs_f64())
        else {
            debug!("ignoring a pong from {sender:#x} that answers no ping this peer sent");
            continue;
        };
        if accepted_this_run.contains(&(sender, pong.seq)) {
            continue;
        }

        // Host side: find the LobbyClient entity for this sender
        if host_lobby.is_some() {
            if let Some((entity, _, prev_rtt, prev_wire, prev_jitter, prev_seq)) = lobby_clients
                .iter()
                .find(|(_, uuid, ..)| uuid.0 == sender)
            {
                if prev_seq.is_some_and(|last| pong.seq <= last.0) {
                    continue;
                }
                accepted_this_run.push((sender, pong.seq));
                let previous_mean = prev_rtt.map(|p| p.0);
                commands.entity(entity).insert((
                    PeerRtt(smooth(previous_mean, sample.e2e)),
                    PeerWireRtt(smooth(prev_wire.map(|p| p.0), sample.wire)),
                    // Folded from the raw `e2e` against the mean as it stood *before* this sample.
                    PeerRttJitter(smooth_jitter(
                        prev_jitter.map(|p| p.0),
                        previous_mean,
                        sample.e2e,
                    )),
                    PeerLastPong(0.0),
                    PeerLastPongSeq(pong.seq),
                ));
            }
            continue;
        }

        // Client side: store on the lobby entity
        if let Some(lobby_entity) = client_lobby.as_ref() {
            let (prev_rtt, prev_wire, prev_jitter, prev_seq) = client_lobby_rtt
                .get(**lobby_entity)
                .unwrap_or((None, None, None, None));
            if prev_seq.is_some_and(|last| pong.seq <= last.0) {
                continue;
            }
            accepted_this_run.push((sender, pong.seq));
            let previous_mean = prev_rtt.map(|p| p.0);
            commands.entity(**lobby_entity).insert((
                PeerRtt(smooth(previous_mean, sample.e2e)),
                PeerWireRtt(smooth(prev_wire.map(|p| p.0), sample.wire)),
                PeerRttJitter(smooth_jitter(
                    prev_jitter.map(|p| p.0),
                    previous_mean,
                    sample.e2e,
                )),
                PeerLastPong(0.0),
                PeerLastPongSeq(pong.seq),
            ));
        }
    }
}

/// Start the liveness clock the moment a peer is known, not the moment it first answers.
///
/// A client that connects and never pongs used to have no [`PeerLastPong`] at all, and so
/// could never time out; the only peers that could be found dead were ones that had once been
/// alive.
pub(crate) fn arm_peer_liveness(
    mut commands: Commands,
    new_clients: Query<Entity, (Added<LobbyClient>, Without<PeerLastPong>)>,
    new_client_lobbies: Query<Entity, (Added<Lobby>, Without<Host>, Without<PeerLastPong>)>,
) {
    for entity in new_clients.iter().chain(new_client_lobbies.iter()) {
        commands.entity(entity).insert(PeerLastPong(0.0));
    }
}

/// Increments [`PeerLastPong`] every frame so consumers can detect stale connections.
pub(crate) fn tick_last_pong(time: Res<Time>, mut peers: Query<&mut PeerLastPong>) {
    let dt = time.delta_secs_f64();
    for mut last_pong in peers.iter_mut() {
        last_pong.0 += dt;
    }
}

/// Act on [`PeerTimeout`].
///
/// On the host, a client that has not answered in time is despawned exactly as a backend does
/// on disconnect, so `on_lobby_client_removed` tells everyone else and the roster shrinks. On a
/// client, a host that has not answered in time ends the session: the lobby goes, the identity
/// goes with it (as it does when a backend loses the host), and [`LobbyLeft`] says why.
pub(crate) fn detect_dead_peers(
    mut commands: Commands,
    timeout: Res<PeerTimeout>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    clients: Query<(Entity, &LobbyClientPlayerUuid, &PeerLastPong), With<LobbyClient>>,
    client_lobbies: Query<
        (Entity, &PeerLastPong),
        (Or<(With<Lobby>, With<PendingLobby>)>, Without<Host>),
    >,
    mut left: MessageWriter<LobbyLeft>,
) {
    let Some(limit) = timeout.0 else {
        return;
    };
    let limit = limit.as_secs_f64();

    if host_lobby.is_some() {
        for (entity, uuid, last_pong) in clients.iter() {
            if last_pong.0 > limit {
                info!(
                    "dropping client {:#x}: no pong for {:.1}s (limit {limit:.1}s)",
                    uuid.0, last_pong.0
                );
                commands.entity(entity).try_despawn();
            }
        }
        return;
    }

    for (entity, last_pong) in client_lobbies.iter() {
        if last_pong.0 > limit {
            warn!(
                "leaving the session: the host has not answered a ping for {:.1}s (limit \
                 {limit:.1}s)",
                last_pong.0
            );
            commands.entity(entity).try_despawn();
            commands.remove_resource::<LocalMultiplayerPlayerId>();
            left.write(LobbyLeft {
                reason: LobbyLeftReason::PeerTimeout,
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

    fn outstanding_at(times: &[f64]) -> OutstandingPings {
        let mut outstanding = OutstandingPings::default();
        for at in times {
            outstanding.issue(*at);
        }
        outstanding
    }

    #[test]
    fn an_unsolicited_pong_is_not_a_sample() {
        let outstanding = outstanding_at(&[1.0]);
        let pong = EnsemblePong {
            seq: 999,
            dwell_micros: 0,
        };
        assert!(sample_from(&outstanding, &pong, 1.05).is_none());
    }

    #[test]
    fn a_pong_answering_a_real_ping_is_measured_from_our_own_clock() {
        let outstanding = outstanding_at(&[1.0]);
        let pong = EnsemblePong {
            seq: 1,
            dwell_micros: 10_000,
        };
        let sample = sample_from(&outstanding, &pong, 1.05).expect("a real ping");
        assert!((sample.e2e - 0.05).abs() < 1e-9);
        assert!((sample.wire - 0.04).abs() < 1e-9);
    }

    #[test]
    fn a_claimed_dwell_cannot_exceed_the_round_trip() {
        // The peer controls the dwell it reports. A dwell longer than the round trip would make
        // the wire estimate negative, and a huge one used to zero it for the rest of the session.
        let outstanding = outstanding_at(&[1.0]);
        let pong = EnsemblePong {
            seq: 1,
            dwell_micros: u32::MAX,
        };
        let sample = sample_from(&outstanding, &pong, 1.05).expect("still a real ping");
        assert_eq!(sample.wire, 0.0);
        assert!((sample.e2e - 0.05).abs() < 1e-9);
    }

    #[test]
    fn an_absurd_round_trip_is_not_a_sample() {
        let outstanding = outstanding_at(&[1.0]);
        let pong = EnsemblePong {
            seq: 1,
            dwell_micros: 0,
        };
        assert!(sample_from(&outstanding, &pong, 1.0 + MAX_ROUND_TRIP_SECS + 1.0).is_none());
        assert!(
            sample_from(&outstanding, &pong, 0.5).is_none(),
            "answered before it was sent"
        );
        assert!(sample_from(&outstanding, &pong, f64::NAN).is_none());
    }

    #[test]
    fn old_pings_are_forgotten() {
        let mut outstanding = OutstandingPings::default();
        for i in 0..(OUTSTANDING_PINGS as u32 + 5) {
            outstanding.issue(f64::from(i));
        }
        assert!(
            outstanding.sent_at(1).is_none(),
            "the oldest have been forgotten"
        );
        assert!(outstanding.sent_at(OUTSTANDING_PINGS as u32 + 5).is_some());
    }
}
