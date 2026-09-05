// TODO(sans-io): migrate this native backend off tokio + the async `webrtc`
// crate onto the sans-IO webrtc-rs stack (`rtc` core, or `webrtc` 0.20 once it
// reaches beta). Goal: drive I/O/timers ourselves from a Bevy system (poll each
// frame) and drop the tokio runtime dependency entirely. Wait for the sans-IO
// line to hit beta / stabilize interop before committing; str0m is the
// lower-risk alternative if we want to move sooner.
use std::sync::Arc;

use tokio::sync::mpsc;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::{IceServers, OutgoingSignal, PeerSignal, PeerState};

pub(crate) struct NativePeerConnection {
    pub connection: Arc<RTCPeerConnection>,
    pub reliable_channel: Arc<RTCDataChannel>,
    pub unreliable_channel: Arc<RTCDataChannel>,
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
}

pub(crate) fn create_peer_connection(
    peer_id: u128,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    peer_state_tx: mpsc::UnboundedSender<(u128, PeerState)>,
    message_tx: mpsc::UnboundedSender<(u128, Box<[u8]>)>,
    ice_servers: &IceServers,
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
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };

    let connection = handle.block_on(async {
        Arc::new(api.new_peer_connection(config).await.unwrap())
    });

    let (signal_queue_tx, signal_queue_rx) = mpsc::unbounded_channel::<PeerSignal>();
    handle.spawn(run_signal_queue(
        connection.clone(),
        peer_id,
        signal_tx.clone(),
        signal_queue_rx,
    ));

    // Trickle ICE: always send candidates immediately. The remote peer applies them in order,
    // behind the offer they belong to.
    {
        let sig_tx = signal_tx.clone();
        connection.on_ice_candidate(Box::new(move |candidate| {
            let sig_tx = sig_tx.clone();
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
                log::info!("peer {peer_id:#x}: gathered local candidate {}", init.candidate);
                let _ = sig_tx.send(OutgoingSignal {
                    peer: peer_id,
                    signal: PeerSignal::IceCandidate(json),
                });
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
    // Observation only: `PeerState` still comes from the data channel opening and closing, so a
    // consumer's notion of "connected" is unchanged by this.
    {
        connection.on_ice_connection_state_change(Box::new(move |state| {
            match state {
                RTCIceConnectionState::Failed => log::warn!(
                    "peer {peer_id:#x}: ICE failed -- no pair of candidates could carry traffic. \
                     Neither peer can reach the other directly; a relay (TURN) is the only \
                     remaining route."
                ),
                RTCIceConnectionState::Disconnected => {
                    log::warn!("peer {peer_id:#x}: ICE disconnected, may recover")
                }
                _ => log::info!("peer {peer_id:#x}: ICE state {state}"),
            }
            Box::pin(async {})
        }));
    }

    {
        connection.on_peer_connection_state_change(Box::new(move |state| {
            match state {
                RTCPeerConnectionState::Failed => log::warn!(
                    "peer {peer_id:#x}: connection failed. If ICE reported `connected` before \
                     this, the failure is in DTLS or SCTP rather than in reaching the peer."
                ),
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

    // Use the reliable channel for connection state signaling.
    {
        let ps_tx = peer_state_tx.clone();
        reliable_channel.on_open(Box::new(move || {
            let _ = ps_tx.send((peer_id, PeerState::Connected));
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
            let _ = msg_tx.send((peer_id, msg.data.to_vec().into_boxed_slice()));
            Box::pin(async {})
        }));
    }

    {
        let msg_tx = message_tx;
        unreliable_channel.on_message(Box::new(move |msg| {
            let _ = msg_tx.send((peer_id, msg.data.to_vec().into_boxed_slice()));
            Box::pin(async {})
        }));
    }

    NativePeerConnection {
        connection,
        reliable_channel,
        unreliable_channel,
        runtime_handle: handle,
        signal_queue: signal_queue_tx,
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
    mut signals: mpsc::UnboundedReceiver<PeerSignal>,
) {
    while let Some(signal) = signals.recv().await {
        match signal {
            PeerSignal::Offer(sdp) => {
                let Ok(remote) = RTCSessionDescription::offer(sdp) else {
                    log::warn!("peer {peer_id:#x} sent an offer that is not valid SDP");
                    continue;
                };
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
                        continue;
                    }
                };
                // Reachable only if a peer trickles a candidate before its own offer, which is
                // its bug rather than ours — but it is logged rather than dropped, because the
                // silence is what made the original race expensive to find.
                if let Err(error) = conn.add_ice_candidate(init).await {
                    log::warn!("peer {peer_id:#x}: discarding an ICE candidate: {error}");
                }
            }
        }
    }
}

pub(crate) fn create_offer(
    pc: &NativePeerConnection,
    peer_id: u128,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    handle: &tokio::runtime::Handle,
) {
    let conn = pc.connection.clone();
    handle.spawn(async move {
        let offer = conn.create_offer(None).await.unwrap();
        let sdp = offer.sdp.clone();
        conn.set_local_description(offer).await.unwrap();

        let _ = signal_tx.send(OutgoingSignal {
            peer: peer_id,
            signal: PeerSignal::Offer(sdp),
        });
    });
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

pub(crate) fn send_message(pc: &NativePeerConnection, data: Box<[u8]>, reliable: bool) {
    let dc = if reliable {
        pc.reliable_channel.clone()
    } else {
        pc.unreliable_channel.clone()
    };
    let bytes = bytes::Bytes::from(data.into_vec());
    pc.runtime_handle.spawn(async move {
        let _ = dc.send(&bytes).await;
    });
}
