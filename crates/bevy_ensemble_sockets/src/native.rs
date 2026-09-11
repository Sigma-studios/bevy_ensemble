// TODO(sans-io): migrate this native backend off tokio + the async `webrtc`
// crate onto the sans-IO webrtc-rs stack (`rtc` core, or `webrtc` 0.20 once it
// reaches beta). Goal: drive I/O/timers ourselves from a Bevy system (poll each
// frame) and drop the tokio runtime dependency entirely. Wait for the sans-IO
// line to hit beta / stabilize interop before committing; str0m is the
// lower-risk alternative if we want to move sooner.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use web_time::Instant;

use tokio::sync::mpsc;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::offer_answer_options::RTCOfferOptions;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;

use crate::recovery::{Recover, Recovery, Role};
use crate::{IceServers, MAX_ICE_RESTARTS, OutgoingSignal, PeerRoute, PeerSignal, PeerState};

pub(crate) struct NativePeerConnection {
    pub connection: Arc<RTCPeerConnection>,
    runtime_handle: tokio::runtime::Handle,
    /// Every signal for this peer, applied strictly in the order it arrived.
    ///
    /// Signalling is an ordered conversation — an offer, then the candidates that refine it —
    /// and applying two of its steps concurrently loses information. `set_remote_description`
    /// parses SDP and brings up DTLS; `add_ice_candidate` is a few instructions. Spawn a task
    /// per signal, as this used to, and the cheap one routinely finishes first, which on
    /// loopback is nearly always. `RTCPeerConnection::add_ice_candidate` then returns
    /// `ErrNoRemoteDescription` and the address is gone: no retry, no queue, and — until this
    /// was hunted down — no log line either.
    ///
    /// ICE usually survives that, because one address surviving in either direction lets the
    /// other side be discovered as a peer-reflexive candidate. It fails only when *every*
    /// candidate on *both* sides is lost, at which point neither peer can send the first packet
    /// and nothing bootstraps: `pingAllCandidates called with no candidate pairs`, for ever, on
    /// a connection that looked healthy right up to the data channel that never opened. Measured
    /// at one session in four on loopback, where host candidates are the only candidates.
    ///
    /// One worker per peer, awaiting each signal to completion, is the whole fix. It is also why
    /// there is no separate candidate buffer here: ordering is a property of the queue rather
    /// than something each handler has to defend against.
    signal_queue: mpsc::UnboundedSender<PeerSignal>,
    /// Every outbound packet for this peer, written to its data channel in the order it was sent.
    ///
    /// One task per `send` is what this used to be, and two packets sent in the same frame then
    /// raced each other to the SCTP stream: the "reliable, ordered" channel delivered same-frame
    /// sends reordered between one time in seven and three in four, depending on the machine.
    /// SCTP orders what it is handed; it cannot order what it is handed in the wrong order.
    ///
    /// One writer, awaiting each send to completion, is the whole fix — the same shape as
    /// `signal_queue`, for the same reason. Both channels share the one queue: an unreliable
    /// packet waiting behind a reliable one costs nothing measurable, and a single queue cannot
    /// reorder anything.
    outbound: mpsc::UnboundedSender<(bytes::Bytes, bool)>,
    /// The two workers above, so that a disconnect ends them rather than leaving whatever they
    /// were awaiting to finish on its own.
    workers: [tokio::task::JoinHandle<()>; 2],
    /// Everything an ICE restart needs, shared with the state handlers that trigger one.
    restarter: Restarter,
}

impl Drop for NativePeerConnection {
    /// Dropping the `Arc<RTCPeerConnection>` is not enough: webrtc-rs keeps its ICE agent, its
    /// sockets and their tasks alive until `close()` is called, and a socket that is rebuilt on
    /// every lobby leaves a set of them behind each time. Closing here rather than in
    /// `disconnect_peer` means every path that lets go of a peer — one disconnect, all of them,
    /// or the socket itself being dropped — closes it.
    ///
    /// Spawned rather than awaited because this runs on the game thread, and a runtime that has
    /// already shut down drops the future instead of running it, which is the right outcome
    /// there too.
    fn drop(&mut self) {
        let pc = Arc::clone(&self.connection);
        self.runtime_handle.spawn(async move {
            let _ = pc.close().await;
        });
        // The senders die with `self`, which ends both loops at their next `recv()`. The abort
        // covers a worker that is mid-await — a write the peer has stopped acknowledging, an SDP
        // that is still being applied — and would otherwise outlive the connection it serves.
        for worker in &self.workers {
            worker.abort();
        }
        // An offer being made — the first one, or a restart's — for a connection that is closing
        // would only ever produce a signal to a peer this side has let go of.
        self.restarter.abort_offer();
    }
}

/// Local candidates held back until the description they belong to has been sent.
///
/// The transport gathers as soon as it can: on the first `set_local_description`, and — for an
/// ICE restart — inside `create_offer` and inside the answerer's `set_remote_description`, before
/// the SDP that carries the new credentials even exists. A candidate can therefore fire, and be
/// sent, *before* the offer or answer it belongs to. On the far side it then arrives ahead of
/// that description, and the remote description it is applied against is the old one (a restart)
/// or none at all (a first answer) — and webrtc-rs's restart wipes the remote candidate list, so
/// either way the address is lost. The far peer's signal queue keeps signals in arrival order;
/// it cannot fix an order that was wrong when it was sent.
///
/// So while an offer or answer is being made, candidates are held here, and released — in the
/// order they were gathered, behind the description — when it has been sent. A guard rather than
/// a flag, so that a description that fails to be made releases them too, instead of holding them
/// for the life of the connection.
#[derive(Clone)]
struct CandidateGate {
    peer_id: u128,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    inner: Arc<Mutex<GateState>>,
}

#[derive(Default)]
struct GateState {
    holds: u32,
    held: Vec<String>,
}

impl CandidateGate {
    fn hold(&self) -> Held {
        self.inner.lock().unwrap().holds += 1;
        Held(self.clone())
    }

    /// Send this candidate now, or hold it if a description is being made.
    fn send(&self, json: String) {
        let mut state = self.inner.lock().unwrap();
        if state.holds > 0 {
            state.held.push(json);
            log::debug!(
                "peer {:#x}: holding a local candidate until the description it belongs to has \
                 been sent ({} held)",
                self.peer_id,
                state.held.len()
            );
            return;
        }
        drop(state);
        let _ = self.signal_tx.send(OutgoingSignal {
            peer: self.peer_id,
            signal: PeerSignal::IceCandidate(json),
        });
    }
}

struct Held(CandidateGate);

impl Drop for Held {
    fn drop(&mut self) {
        // Sent with the lock held, so that a candidate gathered while these go out lands
        // behind them rather than between them.
        let mut state = self.0.inner.lock().unwrap();
        state.holds -= 1;
        if state.holds > 0 {
            return;
        }
        for json in state.held.drain(..) {
            let _ = self.0.signal_tx.send(OutgoingSignal {
                peer: self.0.peer_id,
                signal: PeerSignal::IceCandidate(json),
            });
        }
    }
}

/// What an ICE restart needs, in a form the state handlers can hold.
///
/// Holds the connection weakly: this lives inside handlers the connection itself owns, and an
/// `Arc` here would be a cycle that `close()` does not break.
#[derive(Clone)]
struct Restarter {
    peer_id: u128,
    connection: Weak<RTCPeerConnection>,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    peer_state_tx: mpsc::UnboundedSender<(u128, PeerState)>,
    gate: CandidateGate,
    recovery: Arc<Mutex<Recovery>>,
    handle: tokio::runtime::Handle,
    /// The offer being made, if one is, so that a drop can abort it.
    offer: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl Restarter {
    fn report(&self, state: PeerState) {
        let _ = self.peer_state_tx.send((self.peer_id, state));
    }

    /// ICE changed state. `Connected`/`Completed` ends a recovery if one is under way;
    /// `Disconnected`/`Failed` starts one, or gives up, as the policy says.
    fn on_ice_state(&self, state: RTCIceConnectionState) {
        let peer_id = self.peer_id;
        match state {
            RTCIceConnectionState::Connected | RTCIceConnectionState::Completed => {
                let restored = self.recovery.lock().unwrap().ice_connected();
                if restored {
                    log::info!("peer {peer_id:#x}: ICE found a path again ({state})");
                    // The data channel never closed, so its `on_open` will not say so.
                    self.report(PeerState::Connected);
                } else {
                    log::info!("peer {peer_id:#x}: ICE state {state}");
                }
            }
            RTCIceConnectionState::Disconnected | RTCIceConnectionState::Failed => {
                let failed = state == RTCIceConnectionState::Failed;
                let decision = self.recovery.lock().unwrap().ice_lost(failed);
                if decision == Recover::GiveUp && failed {
                    log::warn!(
                        "peer {peer_id:#x}: ICE failed -- no pair of candidates could carry \
                         traffic. Neither peer can reach the other directly; a relay (TURN) is \
                         the only remaining route."
                    );
                } else {
                    log::warn!("peer {peer_id:#x}: ICE {state}, the path to it is lost");
                }
                self.recover(decision);
            }
            _ => log::info!("peer {peer_id:#x}: ICE state {state}"),
        }
    }

    /// Do what the policy decided: report it, and make the restart offer if it is this side's
    /// to make.
    fn recover(&self, decision: Recover) {
        let peer_id = self.peer_id;
        match decision {
            Recover::GiveUp => {
                log::warn!("peer {peer_id:#x}: giving up on it");
                self.report(PeerState::Failed);
            }
            Recover::Wait => {
                log::warn!(
                    "peer {peer_id:#x}: waiting for it to restart ICE (it made the offer, so \
                     the restart is its to make)"
                );
                self.report(PeerState::Reconnecting);
            }
            Recover::Restart { attempt } => {
                log::warn!(
                    "peer {peer_id:#x}: restarting ICE, attempt {attempt} of {MAX_ICE_RESTARTS}"
                );
                self.report(PeerState::Reconnecting);
                self.spawn_offer(true);
            }
        }
    }

    /// Make an offer — the first, or one with `ice_restart` — and send it, then whatever
    /// candidates gathering produced meanwhile.
    fn spawn_offer(&self, restart: bool) {
        let Some(conn) = self.connection.upgrade() else {
            log::debug!("peer {:#x}: no offer, the connection is gone", self.peer_id);
            return;
        };
        let task = self.handle.spawn(make_offer(
            conn,
            self.peer_id,
            self.signal_tx.clone(),
            self.gate.clone(),
            restart,
        ));
        if let Some(previous) = self.offer.lock().unwrap().replace(task) {
            previous.abort();
        }
    }

    fn abort_offer(&self) {
        if let Some(task) = self.offer.lock().unwrap().take() {
            task.abort();
        }
    }
}

async fn make_offer(
    conn: Arc<RTCPeerConnection>,
    peer_id: u128,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    gate: CandidateGate,
    restart: bool,
) {
    let _held = gate.hold();
    let options = restart.then_some(RTCOfferOptions {
        ice_restart: true,
        voice_activity_detection: false,
    });
    // With `ice_restart`, webrtc-rs restarts the ICE agent -- new credentials, candidates
    // regathered -- inside this call; the DTLS and SCTP transports over it are untouched.
    let offer = match conn.create_offer(options).await {
        Ok(offer) => offer,
        Err(error) => {
            log::warn!("peer {peer_id:#x}: could not create an offer: {error}");
            return;
        }
    };
    let sdp = offer.sdp.clone();
    if let Err(error) = conn.set_local_description(offer).await {
        log::warn!("peer {peer_id:#x}: could not apply our offer: {error}");
        return;
    }
    let _ = signal_tx.send(OutgoingSignal {
        peer: peer_id,
        signal: PeerSignal::Offer(sdp),
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_peer_connection(
    peer_id: u128,
    role: Role,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    peer_state_tx: mpsc::UnboundedSender<(u128, PeerState)>,
    route_tx: mpsc::UnboundedSender<(u128, PeerRoute)>,
    message_tx: mpsc::UnboundedSender<(u128, Box<[u8]>, Instant)>,
    ice_servers: &IceServers,
    discarded_candidates: Arc<AtomicU64>,
    handle: tokio::runtime::Handle,
) -> NativePeerConnection {
    let api = APIBuilder::new().build();

    let config = RTCConfiguration {
        ice_servers: ice_servers
            .0
            .iter()
            .map(|server| RTCIceServer {
                urls: server.urls.clone(),
                username: server.username.clone(),
                credential: server.credential.clone(),
            })
            .collect(),
        ..Default::default()
    };

    let connection =
        handle.block_on(async { Arc::new(api.new_peer_connection(config).await.unwrap()) });

    // First, so that whoever listens hears of this peer before anything happens to it.
    let _ = peer_state_tx.send((peer_id, PeerState::Connecting));

    let gate = CandidateGate {
        peer_id,
        signal_tx: signal_tx.clone(),
        inner: Arc::new(Mutex::new(GateState::default())),
    };
    let restarter = Restarter {
        peer_id,
        connection: Arc::downgrade(&connection),
        signal_tx: signal_tx.clone(),
        peer_state_tx: peer_state_tx.clone(),
        gate: gate.clone(),
        recovery: Arc::new(Mutex::new(Recovery::new(role))),
        handle: handle.clone(),
        offer: Arc::new(Mutex::new(None)),
    };

    let (signal_queue_tx, signal_queue_rx) = mpsc::unbounded_channel::<PeerSignal>();
    let signal_worker = handle.spawn(run_signal_queue(
        connection.clone(),
        peer_id,
        signal_tx.clone(),
        gate.clone(),
        discarded_candidates,
        signal_queue_rx,
    ));

    // Trickle ICE: send candidates as they come, behind the description they belong to (see
    // `CandidateGate`). The remote peer applies them in order.
    {
        let gate = gate.clone();
        connection.on_ice_candidate(Box::new(move |candidate| {
            let gate = gate.clone();
            Box::pin(async move {
                // `None` is the end-of-candidates marker. Worth a line of its own: "gathering
                // finished having found nothing usable" and "gathering is still running" are
                // different problems that otherwise look identical from the log.
                let Some(candidate) = candidate else {
                    log::info!("peer {peer_id:#x}: finished gathering local candidates");
                    return;
                };
                let init = match candidate.to_json() {
                    Ok(init) => init,
                    Err(error) => {
                        log::warn!(
                            "peer {peer_id:#x}: dropping a local candidate that would not \
                             serialise: {error}"
                        );
                        return;
                    }
                };
                let json = serde_json::to_string(&init).unwrap();
                // The candidate line carries its type (host / srflx / relay) and address, which
                // is what says whether this peer has anything a remote peer could reach it on.
                log::info!(
                    "peer {peer_id:#x}: gathered local candidate {}",
                    init.candidate
                );
                gate.send(json);
            })
        }));
    }

    // ICE and peer connection state.
    //
    // Every other callback in this file reports success -- a channel that opened, a message that
    // arrived -- so a connection that simply never completes produced no line at all. That is the
    // one symptom shared by every distinct failure in this stack: a lost candidate, a blocked
    // port, a peer that went away, a NAT neither side can traverse. These two handlers are what
    // separate them, and they cost nothing on a connection that works.
    //
    // ICE is also where a lost path is noticed and, on the offerer, restarted: `Disconnected`
    // and `Failed` go through the recovery policy, which decides between a restart offer,
    // waiting for one, and `Failed`. `Connected` from the data channel opening is still the
    // proof that traffic flows; the ICE `Connected` that ends a restart is reported too, because
    // the channel never closed and will not open again.
    {
        let restarter = restarter.clone();
        connection.on_ice_connection_state_change(Box::new(move |state| {
            restarter.on_ice_state(state);
            Box::pin(async {})
        }));
    }

    {
        // Both handlers report, and the duplicate costs nothing: `update_peers` reports a state
        // once. Which of the two reaches `Failed` first -- or at all -- differs between stacks,
        // and this is not a signal to be clever about missing. The one exception is a restart
        // under way: `Failed` here follows the ICE `Failed` that started it (the ICE handler
        // runs first), and reporting it would end a recovery that has restarts left.
        let restarter = restarter.clone();
        connection.on_peer_connection_state_change(Box::new(move |state| {
            match state {
                RTCPeerConnectionState::Failed => {
                    if restarter.recovery.lock().unwrap().in_progress() {
                        log::info!(
                            "peer {peer_id:#x}: connection failed while ICE is restarting; \
                             not giving up yet"
                        );
                    } else {
                        log::warn!(
                            "peer {peer_id:#x}: connection failed. If ICE reported `connected` \
                             before this, the failure is in DTLS or SCTP rather than in reaching \
                             the peer."
                        );
                        restarter.report(PeerState::Failed);
                    }
                }
                _ => log::info!("peer {peer_id:#x}: connection state {state}"),
            }
            Box::pin(async {})
        }));
    }

    // Create negotiated reliable data channel (ordered, reliable).
    let reliable_config = RTCDataChannelInit {
        ordered: Some(true),
        negotiated: Some(0),
        ..Default::default()
    };
    let reliable_channel = handle.block_on(async {
        connection
            .create_data_channel("ensemble_reliable", Some(reliable_config))
            .await
            .unwrap()
    });

    // Create negotiated unreliable data channel (unordered, no retransmits).
    let unreliable_config = RTCDataChannelInit {
        ordered: Some(false),
        max_retransmits: Some(0),
        negotiated: Some(1),
        ..Default::default()
    };
    let unreliable_channel = handle.block_on(async {
        connection
            .create_data_channel("ensemble_unreliable", Some(unreliable_config))
            .await
            .unwrap()
    });

    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<(bytes::Bytes, bool)>();
    // The writer owns the only handles to the channels this side keeps; the peer connection
    // holds its own, so this is not what keeps them alive.
    let outbound_worker = handle.spawn(run_outbound_queue(
        peer_id,
        Arc::clone(&reliable_channel),
        Arc::clone(&unreliable_channel),
        outbound_rx,
    ));

    // Use the reliable channel for connection state signaling.
    {
        let ps_tx = peer_state_tx.clone();
        let route_tx = route_tx.clone();
        let pc = Arc::clone(&connection);
        let route_handle = handle.clone();
        reliable_channel.on_open(Box::new(move || {
            let _ = ps_tx.send((peer_id, PeerState::Connected));

            // Asked here, and only here, because this is the one moment the answer is both
            // available and relevant: a negotiated data channel opens over SCTP, which needs
            // DTLS, which needs a nominated pair — so by now ICE has chosen, and traffic is
            // about to start flowing over whatever it chose.
            //
            // Reported rather than logged, because "is this session relayed" is the first thing
            // to establish when somebody says the game feels worse than usual. A renomination
            // later in the session would not be picked up; ICE rarely does one, and the cost of
            // being wrong is a stale word in a debug overlay.
            let route_tx = route_tx.clone();
            let pc = Arc::clone(&pc);
            route_handle.spawn(async move {
                let dtls = pc.sctp().transport();
                let Some(pair) = dtls.ice_transport().get_selected_candidate_pair().await else {
                    log::debug!("peer {peer_id:#x}: no candidate pair to report");
                    return;
                };
                let relayed = pair.local.typ == RTCIceCandidateType::Relay
                    || pair.remote.typ == RTCIceCandidateType::Relay;
                let route = if relayed {
                    PeerRoute::Relayed
                } else {
                    PeerRoute::Direct
                };
                log::info!("peer {peer_id:#x}: {} pair, {pair}", route.label());
                let _ = route_tx.send((peer_id, route));
            });
            Box::pin(async {})
        }));
    }

    {
        let ps_tx = peer_state_tx;
        reliable_channel.on_close(Box::new(move || {
            let _ = ps_tx.send((peer_id, PeerState::Disconnected));
            Box::pin(async {})
        }));
    }

    // Both channels feed into the same message receiver.
    {
        let msg_tx = message_tx.clone();
        reliable_channel.on_message(Box::new(move |msg| {
            let received_at = Instant::now();
            let _ = msg_tx.send((peer_id, msg.data.to_vec().into_boxed_slice(), received_at));
            Box::pin(async {})
        }));
    }

    {
        let msg_tx = message_tx;
        unreliable_channel.on_message(Box::new(move |msg| {
            let received_at = Instant::now();
            let _ = msg_tx.send((peer_id, msg.data.to_vec().into_boxed_slice(), received_at));
            Box::pin(async {})
        }));
    }

    NativePeerConnection {
        connection,
        runtime_handle: handle,
        signal_queue: signal_queue_tx,
        outbound: outbound_tx,
        workers: [signal_worker, outbound_worker],
        restarter,
    }
}

/// Write one peer's outbound packets, one at a time, in the order they were sent.
///
/// Ends when the sender is dropped, i.e. with the peer connection; see `NativePeerConnection::outbound`.
async fn run_outbound_queue(
    peer_id: u128,
    reliable: Arc<RTCDataChannel>,
    unreliable: Arc<RTCDataChannel>,
    mut outbound: mpsc::UnboundedReceiver<(bytes::Bytes, bool)>,
) {
    while let Some((bytes, reliably)) = outbound.recv().await {
        let (channel, label) = if reliably {
            (&reliable, "reliable")
        } else {
            (&unreliable, "unreliable")
        };
        // Sending before the channel is open, or after it has closed, is the usual reason. Either
        // way a packet the caller believes was sent was not, and that used to be silent.
        if let Err(error) = channel.send(&bytes).await {
            log::warn!(
                "peer {peer_id:#x}: could not send {} bytes on the {label} channel: {error}",
                bytes.len()
            );
        }
    }
}

/// Apply one peer's signals, one at a time, in arrival order.
///
/// Lives for as long as the channel does: dropping the peer connection drops the sender, the
/// `recv()` returns `None`, and the worker ends.
async fn run_signal_queue(
    conn: Arc<RTCPeerConnection>,
    peer_id: u128,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    gate: CandidateGate,
    discarded_candidates: Arc<AtomicU64>,
    mut signals: mpsc::UnboundedReceiver<PeerSignal>,
) {
    while let Some(signal) = signals.recv().await {
        match signal {
            PeerSignal::Offer(sdp) => {
                // Held from before the remote description goes in: for a restart offer, that
                // is the call that regathers, and the answer does not exist yet.
                let held = gate.hold();
                let Ok(remote) = RTCSessionDescription::offer(sdp) else {
                    log::warn!("peer {peer_id:#x} sent an offer that is not valid SDP");
                    continue;
                };
                // A second offer with new ICE credentials makes webrtc-rs restart its own
                // agent here, to match the offerer's. Nothing to do for it beyond answering.
                if let Err(error) = conn.set_remote_description(remote).await {
                    log::warn!("peer {peer_id:#x}: could not apply its offer: {error}");
                    continue;
                }
                let answer = match conn.create_answer(None).await {
                    Ok(answer) => answer,
                    Err(error) => {
                        log::warn!("peer {peer_id:#x}: could not answer: {error}");
                        continue;
                    }
                };
                let sdp = answer.sdp.clone();
                if let Err(error) = conn.set_local_description(answer).await {
                    log::warn!("peer {peer_id:#x}: could not apply our answer: {error}");
                    continue;
                }
                let _ = signal_tx.send(OutgoingSignal {
                    peer: peer_id,
                    signal: PeerSignal::Answer(sdp),
                });
                drop(held);
            }
            PeerSignal::Answer(sdp) => {
                let Ok(remote) = RTCSessionDescription::answer(sdp) else {
                    log::warn!("peer {peer_id:#x} sent an answer that is not valid SDP");
                    continue;
                };
                if let Err(error) = conn.set_remote_description(remote).await {
                    log::warn!("peer {peer_id:#x}: could not apply its answer: {error}");
                }
            }
            PeerSignal::IceCandidate(json) => {
                let init: RTCIceCandidateInit = match serde_json::from_str(&json) {
                    Ok(init) => init,
                    Err(error) => {
                        log::warn!("peer {peer_id:#x} sent an unreadable ICE candidate: {error}");
                        discarded_candidates.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };
                // Reachable only if a peer trickles a candidate before its own offer, which is
                // its bug rather than ours — but it is logged rather than dropped, because the
                // silence is what made the original race expensive to find. Counted too, so
                // that a soak test can assert it never happens.
                let candidate = init.candidate.clone();
                if let Err(error) = conn.add_ice_candidate(init).await {
                    log::warn!("peer {peer_id:#x}: discarding an ICE candidate: {error}");
                    discarded_candidates.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                // The counterpart to the `gathered local candidate` line. With only one of the
                // two, a log says what this peer offered the world and nothing about what reached
                // it, and "the remote candidates never arrived" and "they arrived and no pair
                // worked" are different problems that end the same way.
                log::info!("peer {peer_id:#x}: applied remote candidate {candidate}");
            }
        }
    }
}

/// Make and send this side's offer. `restart` asks for new ICE credentials and a fresh gather.
pub(crate) fn create_offer(pc: &NativePeerConnection, restart: bool) {
    pc.restarter.spawn_offer(restart);
}

/// See [`EnsembleSocket::restart_ice`](crate::EnsembleSocket::restart_ice).
pub(crate) fn restart_ice(pc: &NativePeerConnection) {
    let peer_id = pc.restarter.peer_id;
    let decision = {
        let mut recovery = pc.restarter.recovery.lock().unwrap();
        if recovery.role() == Role::Answerer {
            log::warn!(
                "peer {peer_id:#x}: not restarting ICE -- this side answered its offer, and only \
                 the offerer can restart. It will, when its ICE notices the loss."
            );
            return;
        }
        recovery.request_restart()
    };
    log::info!("peer {peer_id:#x}: ICE restart requested");
    pc.restarter.recover(decision);
}

pub(crate) fn role(pc: &NativePeerConnection) -> Role {
    pc.restarter.recovery.lock().unwrap().role()
}

/// Hand the offer to this peer's signal queue. The answer is sent from there, once the offer has
/// actually been applied.
pub(crate) fn accept_offer(pc: &NativePeerConnection, offer_sdp: &str) {
    let _ = pc
        .signal_queue
        .send(PeerSignal::Offer(offer_sdp.to_string()));
}

pub(crate) fn set_remote_answer(pc: &NativePeerConnection, sdp: &str) {
    let _ = pc.signal_queue.send(PeerSignal::Answer(sdp.to_string()));
}

pub(crate) fn add_ice_candidate(pc: &NativePeerConnection, candidate_json: &str) {
    let _ = pc
        .signal_queue
        .send(PeerSignal::IceCandidate(candidate_json.to_string()));
}

/// Queue a packet for this peer's writer. Order between calls is the order on the wire.
pub(crate) fn send_message(pc: &NativePeerConnection, data: Box<[u8]>, reliable: bool) {
    let bytes = bytes::Bytes::from(data.into_vec());
    // The only way this fails is a writer that has already ended, which only happens when the
    // connection is being dropped — and a packet to a peer being dropped has nowhere to go.
    let _ = pc.outbound.send((bytes, reliable));
}
