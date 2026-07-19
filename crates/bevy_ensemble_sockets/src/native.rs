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
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::{OutgoingSignal, PeerSignal, PeerState};

const STUN_URLS: &[&str] = &[
    "stun:stun.l.google.com:19302",
    "stun:stun1.l.google.com:19302",
];

pub(crate) struct NativePeerConnection {
    pub connection: Arc<RTCPeerConnection>,
    pub reliable_channel: Arc<RTCDataChannel>,
    pub unreliable_channel: Arc<RTCDataChannel>,
    runtime_handle: tokio::runtime::Handle,
}

pub(crate) fn create_peer_connection(
    peer_id: u128,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    peer_state_tx: mpsc::UnboundedSender<(u128, PeerState)>,
    message_tx: mpsc::UnboundedSender<(u128, Box<[u8]>)>,
    handle: tokio::runtime::Handle,
) -> NativePeerConnection {
    let api = APIBuilder::new().build();

    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: STUN_URLS.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }],
        ..Default::default()
    };

    let connection = handle.block_on(async {
        Arc::new(api.new_peer_connection(config).await.unwrap())
    });

    // Trickle ICE: always send candidates immediately.
    // The remote peer buffers incoming candidates until its remote description is set.
    {
        let sig_tx = signal_tx.clone();
        connection.on_ice_candidate(Box::new(move |candidate| {
            let sig_tx = sig_tx.clone();
            Box::pin(async move {
                let Some(candidate) = candidate else { return };
                let init = match candidate.to_json() {
                    Ok(init) => init,
                    Err(_) => return,
                };
                let json = serde_json::to_string(&init).unwrap();
                let _ = sig_tx.send(OutgoingSignal {
                    peer: peer_id,
                    signal: PeerSignal::IceCandidate(json),
                });
            })
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

pub(crate) fn accept_offer(
    pc: &NativePeerConnection,
    peer_id: u128,
    offer_sdp: &str,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    handle: &tokio::runtime::Handle,
) {
    let conn = pc.connection.clone();
    let sdp = offer_sdp.to_string();
    handle.spawn(async move {
        let remote = RTCSessionDescription::offer(sdp).unwrap();
        conn.set_remote_description(remote).await.unwrap();

        let answer = conn.create_answer(None).await.unwrap();
        let sdp = answer.sdp.clone();
        conn.set_local_description(answer).await.unwrap();

        let _ = signal_tx.send(OutgoingSignal {
            peer: peer_id,
            signal: PeerSignal::Answer(sdp),
        });
    });
}

pub(crate) fn set_remote_answer(
    pc: &NativePeerConnection,
    sdp: &str,
    handle: &tokio::runtime::Handle,
) {
    let conn = pc.connection.clone();
    let sdp = sdp.to_string();
    handle.spawn(async move {
        let remote = RTCSessionDescription::answer(sdp).unwrap();
        conn.set_remote_description(remote).await.unwrap();
    });
}

pub(crate) fn add_ice_candidate(
    pc: &NativePeerConnection,
    candidate_json: &str,
    handle: &tokio::runtime::Handle,
) {
    let conn = pc.connection.clone();
    let init: RTCIceCandidateInit = match serde_json::from_str(candidate_json) {
        Ok(v) => v,
        Err(_) => return,
    };
    handle.spawn(async move {
        let _ = conn.add_ice_candidate(init).await;
    });
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
