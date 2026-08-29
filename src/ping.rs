use bevy::prelude::*;

use crate::{
    Host, Lobby, LobbyClient, LobbyClientPlayerUuid, ReceivedEnsembleMessage, SendMode,
    messages::{LobbyClientMessage, LobbyMessage},
};

/// How often each peer pings every other.
///
/// Once a second was too slow to be a signal. Two consumers need this to be a *series* and not an
/// occasional reading: the smoothed mean takes about a dozen samples to follow a genuine latency
/// shift, which at 1 Hz is fifteen seconds of a session running on a stale number, and
/// [`PeerRttJitter`] cannot exist at all without enough samples to have a spread.
///
/// Ten a second, and it costs nothing worth counting: the payload is two `f64`s and it goes
/// unreliably, against a lockstep stream already sending 64 messages a second in each direction.
const PING_INTERVAL_SECS: f32 = 0.1;

/// Internal ping message sent over data channels to measure RTT.
#[derive(Message, Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct EnsemblePing {
    /// `t1`: sender's [`Time`] elapsed (seconds) when the ping was created.
    pub t1: f64,
}

/// Internal pong response, echoing the ping timestamp plus how long the responder held
/// the packet. Together with the local send/receive times this gives the four
/// timestamps (t1..t4) needed to separate wire time from in-app dwell.
#[derive(Message, Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct EnsemblePong {
    /// The original `t1` from the ping, echoed back unchanged (on the *sender's* clock).
    pub t1: f64,
    /// `t3 - t2`: seconds the responder spent between receiving the ping at its socket
    /// seam and emitting this pong, measured on the responder's clock. Subtracting this
    /// from the round trip cancels the responder's clock offset and its in-app time.
    pub peer_dwell: f64,
}

/// Round-trip time to a connected peer, in seconds.
///
/// Added to `LobbyClient` entities on the host (one per peer) and to the
/// lobby entity on clients (single connection to the host).
#[derive(Component, Debug, Clone, Copy)]
pub struct PeerRtt(pub f64);

/// Estimated round-trip **wire time** to a peer, in seconds: the full RTT minus the time
/// the peer spent holding the ping (its [`EnsemblePong::peer_dwell`]).
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
/// Added alongside [`PeerRtt`] on `LobbyClient` entities (host) and the
/// lobby entity (client). Starts at `0.0` when a pong arrives and ticks up
/// every frame. If this value exceeds several seconds the connection is
/// likely degraded or dead.
#[derive(Component, Debug, Clone, Copy)]
pub struct PeerLastPong(pub f64);

/// Both host and client send pings to all connected peers every 1 second.
///
/// Uses the standard [`LobbyMessage`] pipeline so pings are routed through
/// whichever transport backend is active.
pub(crate) fn send_pings(
    mut commands: Commands,
    lobbies: Query<Entity, With<Lobby>>,
    time: Res<Time>,
    mut cooldown: Local<f32>,
) {
    *cooldown -= time.delta_secs();
    if *cooldown > 0.0 {
        return;
    }
    *cooldown = PING_INTERVAL_SECS;

    let t1 = time.elapsed_secs_f64();
    for lobby in lobbies.iter() {
        commands
            .entity(lobby)
            .trigger(move |entity| LobbyMessage {
                entity,
                message: EnsemblePing { t1 },
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
        let pong = EnsemblePong {
            t1: message.message.t1,
            peer_dwell: (t3 - t2).max(0.0),
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

/// When we receive a pong, compute the round trip and store both the full end-to-end RTT
/// ([`PeerRtt`]) and the wire estimate ([`PeerWireRtt`], the RTT minus the peer's dwell).
///
/// On the host: updates the components on the `LobbyClient` entity for that peer.
/// On clients: updates them on the lobby entity itself.
pub(crate) fn receive_pongs(
    mut commands: Commands,
    mut messages: MessageReader<ReceivedEnsembleMessage<EnsemblePong>>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    lobby_clients: Query<
        (
            Entity,
            &LobbyClientPlayerUuid,
            Option<&PeerRtt>,
            Option<&PeerWireRtt>,
            Option<&PeerRttJitter>,
        ),
        With<LobbyClient>,
    >,
    client_lobby_rtt: Query<
        (Option<&PeerRtt>, Option<&PeerWireRtt>, Option<&PeerRttJitter>),
        (With<Lobby>, Without<Host>),
    >,
) {
    for message in messages.read() {
        let pong = message.message;
        // Four-timestamp method: t1 = our send, t4 = seam receive of the pong (both on our
        // clock); peer_dwell = t3 - t2 on the peer's clock (its offset cancels out).
        let t4 = message.received_at.as_secs_f64();
        let e2e = t4 - pong.t1;
        if e2e < 0.0 {
            continue;
        }
        let wire = (e2e - pong.peer_dwell).max(0.0);

        let Some(sender) = message.sender else {
            continue;
        };

        // Host side: find the LobbyClient entity for this sender
        if host_lobby.is_some() {
            if let Some((entity, _, prev_rtt, prev_wire, prev_jitter)) = lobby_clients
                .iter()
                .find(|(_, uuid, _, _, _)| uuid.0 == sender)
            {
                let previous_mean = prev_rtt.map(|p| p.0);
                commands.entity(entity).insert((
                    PeerRtt(smooth(previous_mean, e2e)),
                    PeerWireRtt(smooth(prev_wire.map(|p| p.0), wire)),
                    // Folded from the raw `e2e` against the mean as it stood *before* this sample.
                    PeerRttJitter(smooth_jitter(
                        prev_jitter.map(|p| p.0),
                        previous_mean,
                        e2e,
                    )),
                    PeerLastPong(0.0),
                ));
            }
        }

        // Client side: store on the lobby entity
        if let Some(lobby_entity) = client_lobby.as_ref() {
            let (prev_rtt, prev_wire, prev_jitter) = client_lobby_rtt
                .get(**lobby_entity)
                .unwrap_or((None, None, None));
            let previous_mean = prev_rtt.map(|p| p.0);
            commands.entity(**lobby_entity).insert((
                PeerRtt(smooth(previous_mean, e2e)),
                PeerWireRtt(smooth(prev_wire.map(|p| p.0), wire)),
                PeerRttJitter(smooth_jitter(prev_jitter.map(|p| p.0), previous_mean, e2e)),
                PeerLastPong(0.0),
            ));
        }
    }
}

/// Increments [`PeerLastPong`] every frame so consumers can detect stale connections.
pub(crate) fn tick_last_pong(time: Res<Time>, mut peers: Query<&mut PeerLastPong>) {
    let dt = time.delta_secs_f64();
    for mut last_pong in peers.iter_mut() {
        last_pong.0 += dt;
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
}
