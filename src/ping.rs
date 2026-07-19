use bevy::prelude::*;

use crate::{
    Host, Lobby, LobbyClient, LobbyClientPlayerUuid, ReceivedEnsembleMessage, SendMode,
    messages::{LobbyClientMessage, LobbyMessage},
};

const PING_INTERVAL_SECS: f32 = 1.0;

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
        (Entity, &LobbyClientPlayerUuid, Option<&PeerRtt>, Option<&PeerWireRtt>),
        With<LobbyClient>,
    >,
    client_lobby_rtt: Query<(Option<&PeerRtt>, Option<&PeerWireRtt>), (With<Lobby>, Without<Host>)>,
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
            if let Some((entity, _, prev_rtt, prev_wire)) =
                lobby_clients.iter().find(|(_, uuid, _, _)| uuid.0 == sender)
            {
                commands.entity(entity).insert((
                    PeerRtt(smooth(prev_rtt.map(|p| p.0), e2e)),
                    PeerWireRtt(smooth(prev_wire.map(|p| p.0), wire)),
                    PeerLastPong(0.0),
                ));
            }
        }

        // Client side: store on the lobby entity
        if let Some(lobby_entity) = client_lobby.as_ref() {
            let (prev_rtt, prev_wire) = client_lobby_rtt
                .get(**lobby_entity)
                .unwrap_or((None, None));
            commands.entity(**lobby_entity).insert((
                PeerRtt(smooth(prev_rtt.map(|p| p.0), e2e)),
                PeerWireRtt(smooth(prev_wire.map(|p| p.0), wire)),
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
