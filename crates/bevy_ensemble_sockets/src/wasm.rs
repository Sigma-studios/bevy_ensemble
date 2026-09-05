use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelInit, RtcDataChannelType,
    RtcIceCandidateInit, RtcPeerConnection, RtcPeerConnectionIceEvent, RtcSdpType,
    RtcSessionDescriptionInit,
};

use crate::{IceServers, OutgoingSignal, PeerSignal, PeerState};

pub(crate) struct WasmPeerConnection {
    pub connection: RtcPeerConnection,
    pub reliable_channel: RtcDataChannel,
    pub unreliable_channel: RtcDataChannel,
    /// ICE candidates received before remote description is set.
    pending_candidates: Arc<Mutex<Vec<String>>>,
    remote_desc_set: Arc<Mutex<bool>>,
}

pub(crate) fn create_peer_connection(
    peer_id: u128,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    peer_state_tx: mpsc::UnboundedSender<(u128, PeerState)>,
    message_tx: mpsc::UnboundedSender<(u128, Box<[u8]>)>,
    ice_servers: &IceServers,
) -> WasmPeerConnection {
    let config = RtcConfiguration::new();

    /// The shape `RTCPeerConnection` expects, which is not the shape we hold: the browser reads
    /// `credential`, the native stack reads it too, and neither wants the field present when it
    /// is empty.
    #[derive(serde::Serialize)]
    struct JsIceServer {
        urls: Vec<String>,
        #[serde(skip_serializing_if = "String::is_empty")]
        username: String,
        #[serde(skip_serializing_if = "String::is_empty")]
        credential: String,
    }
    let servers: Vec<JsIceServer> = ice_servers
        .0
        .iter()
        .map(|server| JsIceServer {
            urls: server.urls.clone(),
            username: server.username.clone(),
            credential: server.credential.clone(),
        })
        .collect();
    config.set_ice_servers(&serde_wasm_bindgen::to_value(&servers).unwrap());

    let conn = RtcPeerConnection::new_with_configuration(&config).unwrap();

    // Wire onicecandidate — trickle ICE.
    let sig_tx = signal_tx.clone();
    let onicecandidate: Closure<dyn FnMut(RtcPeerConnectionIceEvent)> =
        Closure::wrap(Box::new(move |event: RtcPeerConnectionIceEvent| {
            let Some(candidate) = event.candidate() else {
                // The end-of-candidates marker. Distinguishes "gathering finished and found
                // nothing reachable" from "gathering is still running", which are different
                // problems that read identically in a log that only prints candidates.
                log::info!("peer {peer_id:#x}: finished gathering local candidates");
                return;
            };
            // A browser reports host candidates as randomised `<uuid>.local` mDNS names rather
            // than addresses, and a remote peer that cannot resolve them over multicast is left
            // with nothing to reach this one on. That is visible here and nowhere else, which is
            // the reason this line prints the candidate rather than counting it.
            log::info!(
                "peer {peer_id:#x}: gathered local candidate {}",
                candidate.candidate()
            );
            let json = js_sys::JSON::stringify(&candidate.to_json())
                .unwrap()
                .as_string()
                .unwrap();
            let _ = sig_tx.send(OutgoingSignal {
                peer: peer_id,
                signal: PeerSignal::IceCandidate(json),
            });
        }));
    conn.set_onicecandidate(Some(onicecandidate.as_ref().unchecked_ref()));
    onicecandidate.forget();

    // ICE and connection state.
    //
    // Every other callback here reports success -- a channel that opened, a message that arrived
    // -- so a connection that never completes produced no line at all. In a browser that is worse
    // than on native: there is no `webrtc_ice` log to fall back on, and short of opening
    // `chrome://webrtc-internals` there was no way to tell a lost candidate from a blocked port
    // from a NAT neither peer can traverse. These two handlers are what separate them.
    //
    // Observation only: `PeerState` still comes from the data channel opening and closing, so
    // what a consumer treats as "connected" does not change.
    {
        let conn_for_ice = conn.clone();
        let oniceconnectionstatechange: Closure<dyn FnMut(JsValue)> =
            Closure::wrap(Box::new(move |_: JsValue| {
                let state = conn_for_ice.ice_connection_state();
                if state == web_sys::RtcIceConnectionState::Failed {
                    log::warn!(
                        "peer {peer_id:#x}: ICE failed -- no pair of candidates could carry \
                         traffic. Neither peer can reach the other directly; a relay (TURN) is \
                         the only remaining route."
                    );
                } else {
                    log::info!("peer {peer_id:#x}: ICE state {state:?}");
                }
            }));
        conn.set_oniceconnectionstatechange(Some(
            oniceconnectionstatechange.as_ref().unchecked_ref(),
        ));
        oniceconnectionstatechange.forget();
    }

    {
        let conn_for_state = conn.clone();
        let onconnectionstatechange: Closure<dyn FnMut(JsValue)> =
            Closure::wrap(Box::new(move |_: JsValue| {
                let state = conn_for_state.connection_state();
                if state == web_sys::RtcPeerConnectionState::Failed {
                    log::warn!(
                        "peer {peer_id:#x}: connection failed. If ICE reported `connected` \
                         before this, the failure is in DTLS or SCTP rather than in reaching \
                         the peer."
                    );
                } else {
                    log::info!("peer {peer_id:#x}: connection state {state:?}");
                }
            }));
        conn.set_onconnectionstatechange(Some(onconnectionstatechange.as_ref().unchecked_ref()));
        onconnectionstatechange.forget();
    }

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
        log::warn!("discarding an ICE candidate that is not valid JSON");
        return;
    };
    if parsed.is_null() {
        return;
    }
    let described = js_sys::Reflect::get(&parsed, &JsValue::from_str("candidate"))
        .ok()
        .and_then(|value| value.as_string())
        .unwrap_or_default();
    let candidate = RtcIceCandidateInit::from(parsed);
    // A rejected candidate is an address gone for good -- the peer will not send it again -- and
    // losing every candidate on both sides is a connection that gathers happily and never pairs.
    if let Err(error) =
        JsFuture::from(conn.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&candidate)))
            .await
    {
        log::warn!("discarding a remote ICE candidate the browser rejected: {error:?}");
        return;
    }
    // The counterpart to the `gathered local candidate` line. With only one of the two, a log
    // says what this peer offered the world and nothing about what reached it, and "the remote
    // candidates never arrived" and "they arrived and no pair worked" are different problems that
    // end the same way.
    log::info!("applied remote candidate {described}");
}

pub(crate) fn send_message(pc: &WasmPeerConnection, data: &[u8], reliable: bool) {
    let dc = if reliable {
        &pc.reliable_channel
    } else {
        &pc.unreliable_channel
    };
    let _ = dc.send_with_u8_array(&mut data.to_vec());
}
