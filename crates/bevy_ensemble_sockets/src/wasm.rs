use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelInit, RtcDataChannelType,
    RtcIceCandidateInit, RtcPeerConnection, RtcPeerConnectionIceEvent, RtcSdpType,
    RtcSessionDescriptionInit,
};

use crate::{OutgoingSignal, PeerSignal, PeerState};

pub(crate) struct WasmPeerConnection {
    pub connection: RtcPeerConnection,
    pub reliable_channel: RtcDataChannel,
    pub unreliable_channel: RtcDataChannel,
    /// ICE candidates received before remote description is set.
    pending_candidates: Arc<Mutex<Vec<String>>>,
    remote_desc_set: Arc<Mutex<bool>>,
}

const STUN_URLS: &[&str] = &[
    "stun:stun.l.google.com:19302",
    "stun:stun1.l.google.com:19302",
];

pub(crate) fn create_peer_connection(
    peer_id: u128,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    peer_state_tx: mpsc::UnboundedSender<(u128, PeerState)>,
    message_tx: mpsc::UnboundedSender<(u128, Box<[u8]>)>,
) -> WasmPeerConnection {
    let config = RtcConfiguration::new();

    #[derive(serde::Serialize)]
    struct IceServer {
        urls: Vec<&'static str>,
    }
    let ice_servers = [IceServer {
        urls: STUN_URLS.to_vec(),
    }];
    config.set_ice_servers(&serde_wasm_bindgen::to_value(&ice_servers).unwrap());

    let conn = RtcPeerConnection::new_with_configuration(&config).unwrap();

    // Wire onicecandidate — trickle ICE.
    let sig_tx = signal_tx.clone();
    let onicecandidate: Closure<dyn FnMut(RtcPeerConnectionIceEvent)> =
        Closure::wrap(Box::new(move |event: RtcPeerConnectionIceEvent| {
            if let Some(candidate) = event.candidate() {
                let json = js_sys::JSON::stringify(&candidate.to_json())
                    .unwrap()
                    .as_string()
                    .unwrap();
                let _ = sig_tx.send(OutgoingSignal {
                    peer: peer_id,
                    signal: PeerSignal::IceCandidate(json),
                });
            }
        }));
    conn.set_onicecandidate(Some(onicecandidate.as_ref().unchecked_ref()));
    onicecandidate.forget();

    // Create negotiated reliable data channel (ordered, reliable).
    let reliable_config = RtcDataChannelInit::new();
    reliable_config.set_ordered(true);
    reliable_config.set_negotiated(true);
    reliable_config.set_id(0);
    let reliable_dc = conn.create_data_channel_with_data_channel_dict("ensemble_reliable", &reliable_config);
    reliable_dc.set_binary_type(RtcDataChannelType::Arraybuffer);

    // Create negotiated unreliable data channel (unordered, no retransmits).
    let unreliable_config = RtcDataChannelInit::new();
    unreliable_config.set_ordered(false);
    unreliable_config.set_max_retransmits(0);
    unreliable_config.set_negotiated(true);
    unreliable_config.set_id(1);
    let unreliable_dc = conn.create_data_channel_with_data_channel_dict("ensemble_unreliable", &unreliable_config);
    unreliable_dc.set_binary_type(RtcDataChannelType::Arraybuffer);

    // Use the reliable channel for connection state signaling.
    let ps_tx = peer_state_tx.clone();
    let onopen: Closure<dyn FnMut(JsValue)> = Closure::wrap(Box::new(move |_: JsValue| {
        let _ = ps_tx.send((peer_id, PeerState::Connected));
    }));
    reliable_dc.set_onopen(Some(onopen.as_ref().unchecked_ref()));
    onopen.forget();

    let ps_tx = peer_state_tx;
    let onclose: Closure<dyn FnMut(JsValue)> = Closure::wrap(Box::new(move |_: JsValue| {
        let _ = ps_tx.send((peer_id, PeerState::Disconnected));
    }));
    reliable_dc.set_onclose(Some(onclose.as_ref().unchecked_ref()));
    onclose.forget();

    // Both channels feed into the same message receiver.
    let msg_tx_reliable = message_tx.clone();
    let onmessage_reliable: Closure<dyn FnMut(MessageEvent)> =
        Closure::wrap(Box::new(move |event: MessageEvent| {
            if let Ok(buf) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                let arr = js_sys::Uint8Array::new(&buf);
                let _ = msg_tx_reliable.send((peer_id, arr.to_vec().into_boxed_slice()));
            }
        }));
    reliable_dc.set_onmessage(Some(onmessage_reliable.as_ref().unchecked_ref()));
    onmessage_reliable.forget();

    let msg_tx_unreliable = message_tx;
    let onmessage_unreliable: Closure<dyn FnMut(MessageEvent)> =
        Closure::wrap(Box::new(move |event: MessageEvent| {
            if let Ok(buf) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                let arr = js_sys::Uint8Array::new(&buf);
                let _ = msg_tx_unreliable.send((peer_id, arr.to_vec().into_boxed_slice()));
            }
        }));
    unreliable_dc.set_onmessage(Some(onmessage_unreliable.as_ref().unchecked_ref()));
    onmessage_unreliable.forget();

    WasmPeerConnection {
        connection: conn,
        reliable_channel: reliable_dc,
        unreliable_channel: unreliable_dc,
        pending_candidates: Arc::new(Mutex::new(Vec::new())),
        remote_desc_set: Arc::new(Mutex::new(false)),
    }
}

pub(crate) fn create_offer(
    pc: &WasmPeerConnection,
    peer_id: u128,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
) {
    let conn = pc.connection.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let offer = JsFuture::from(conn.create_offer()).await.unwrap();
        let sdp = js_sys::Reflect::get(&offer, &JsValue::from_str("sdp"))
            .unwrap()
            .as_string()
            .unwrap();

        // Per spec: setLocalDescription must be called before sending the offer.
        let desc = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        desc.set_sdp(&sdp);
        JsFuture::from(conn.set_local_description(&desc))
            .await
            .unwrap();

        let _ = signal_tx.send(OutgoingSignal {
            peer: peer_id,
            signal: PeerSignal::Offer(sdp),
        });
    });
}

pub(crate) fn accept_offer(
    pc: &WasmPeerConnection,
    peer_id: u128,
    offer_sdp: &str,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
) {
    let conn = pc.connection.clone();
    let offer_sdp = offer_sdp.to_string();
    let rds = pc.remote_desc_set.clone();
    let pending = pc.pending_candidates.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let remote_desc = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        remote_desc.set_sdp(&offer_sdp);
        JsFuture::from(conn.set_remote_description(&remote_desc))
            .await
            .unwrap();

        // Flush buffered candidates now that remote description is set.
        *rds.lock().unwrap() = true;
        let buffered: Vec<String> = pending.lock().unwrap().drain(..).collect();
        for c in buffered {
            apply_ice_candidate(&conn, &c).await;
        }

        let answer = JsFuture::from(conn.create_answer()).await.unwrap();
        let sdp = js_sys::Reflect::get(&answer, &JsValue::from_str("sdp"))
            .unwrap()
            .as_string()
            .unwrap();

        // Per spec: setLocalDescription must be called before sending the answer.
        let desc = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        desc.set_sdp(&sdp);
        JsFuture::from(conn.set_local_description(&desc))
            .await
            .unwrap();

        let _ = signal_tx.send(OutgoingSignal {
            peer: peer_id,
            signal: PeerSignal::Answer(sdp),
        });
    });
}

pub(crate) fn set_remote_answer(pc: &WasmPeerConnection, sdp: &str) {
    let conn = pc.connection.clone();
    let sdp = sdp.to_string();
    let rds = pc.remote_desc_set.clone();
    let pending = pc.pending_candidates.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let desc = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        desc.set_sdp(&sdp);
        JsFuture::from(conn.set_remote_description(&desc))
            .await
            .unwrap();

        // Flush buffered candidates now that remote description is set.
        *rds.lock().unwrap() = true;
        let buffered: Vec<String> = pending.lock().unwrap().drain(..).collect();
        for c in buffered {
            apply_ice_candidate(&conn, &c).await;
        }
    });
}

pub(crate) fn add_ice_candidate(pc: &WasmPeerConnection, candidate_json: &str) {
    // Buffer candidates until remote description is set.
    if !*pc.remote_desc_set.lock().unwrap() {
        pc.pending_candidates
            .lock()
            .unwrap()
            .push(candidate_json.to_string());
        return;
    }

    let conn = pc.connection.clone();
    let json = candidate_json.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        apply_ice_candidate(&conn, &json).await;
    });
}

async fn apply_ice_candidate(conn: &RtcPeerConnection, json: &str) {
    let Ok(parsed) = js_sys::JSON::parse(json) else {
        return;
    };
    if parsed.is_null() {
        return;
    }
    let candidate = RtcIceCandidateInit::from(parsed);
    let _ = JsFuture::from(
        conn.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&candidate)),
    )
    .await;
}

pub(crate) fn send_message(pc: &WasmPeerConnection, data: &[u8], reliable: bool) {
    let dc = if reliable {
        &pc.reliable_channel
    } else {
        &pc.unreliable_channel
    };
    let _ = dc.send_with_u8_array(&mut data.to_vec());
}
