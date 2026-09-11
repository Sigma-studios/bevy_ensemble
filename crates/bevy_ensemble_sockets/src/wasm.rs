use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use web_time::Instant;

use tokio::sync::mpsc;
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelInit, RtcDataChannelType,
    RtcIceCandidateInit, RtcIceConnectionState, RtcOfferOptions, RtcPeerConnection,
    RtcPeerConnectionIceEvent, RtcSdpType, RtcSessionDescriptionInit,
};

use crate::recovery::{Recover, Recovery, Role};
use crate::{IceServers, MAX_ICE_RESTARTS, OutgoingSignal, PeerRoute, PeerSignal, PeerState};

pub(crate) struct WasmPeerConnection {
    pub connection: RtcPeerConnection,
    pub reliable_channel: RtcDataChannel,
    pub unreliable_channel: RtcDataChannel,
    /// ICE candidates received before remote description is set.
    pending_candidates: Arc<Mutex<Vec<String>>>,
    remote_desc_set: Arc<Mutex<bool>>,
    /// Everything an ICE restart needs, shared with the state handlers that trigger one.
    restarter: Restarter,
}

impl Drop for WasmPeerConnection {
    /// The browser keeps a peer connection — its sockets, its candidates, its DTLS session —
    /// until `close()` is called or the tab goes. Closing here means every path that lets go of
    /// a peer closes it, as on native; and an offer being made for it, a restart's included,
    /// rejects instead of producing a signal to a peer this side has let go of.
    fn drop(&mut self) {
        self.connection.close();
    }
}

/// What an ICE restart needs, in a form the state handlers can hold.
#[derive(Clone)]
struct Restarter {
    peer_id: u128,
    connection: RtcPeerConnection,
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    peer_state_tx: mpsc::UnboundedSender<(u128, PeerState)>,
    recovery: Arc<Mutex<Recovery>>,
    /// Remote candidates the browser refused; the socket's counter, shared by all its peers.
    discarded_candidates: Arc<AtomicU64>,
}

impl Restarter {
    fn report(&self, state: PeerState) {
        let _ = self.peer_state_tx.send((self.peer_id, state));
    }

    /// ICE changed state. `Connected`/`Completed` ends a recovery if one is under way;
    /// `Disconnected`/`Failed` starts one, or gives up, as the policy says.
    fn on_ice_state(&self, state: RtcIceConnectionState) {
        let peer_id = self.peer_id;
        match state {
            RtcIceConnectionState::Connected | RtcIceConnectionState::Completed => {
                let restored = self.recovery.lock().unwrap().ice_connected();
                if restored {
                    log::info!("peer {peer_id:#x}: ICE found a path again ({state:?})");
                    // The data channel never closed, so its `onopen` will not say so.
                    self.report(PeerState::Connected);
                } else {
                    log::info!("peer {peer_id:#x}: ICE state {state:?}");
                }
            }
            RtcIceConnectionState::Disconnected | RtcIceConnectionState::Failed => {
                let failed = state == RtcIceConnectionState::Failed;
                let decision = self.recovery.lock().unwrap().ice_lost(failed);
                if decision == Recover::GiveUp && failed {
                    log::warn!(
                        "peer {peer_id:#x}: ICE failed -- no pair of candidates could carry \
                         traffic. Neither peer can reach the other directly; a relay (TURN) is \
                         the only remaining route."
                    );
                } else {
                    log::warn!("peer {peer_id:#x}: ICE {state:?}, the path to it is lost");
                }
                self.recover(decision);
            }
            _ => log::info!("peer {peer_id:#x}: ICE state {state:?}"),
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

    /// Make an offer — the first, or one with `iceRestart` — and send it.
    ///
    /// No holding of candidates here, unlike native: a browser starts gathering when
    /// `setLocalDescription` resolves, and fires `icecandidate` as events after it, so the offer
    /// sent from the same continuation is always ahead of them.
    fn spawn_offer(&self, restart: bool) {
        let conn = self.connection.clone();
        let signal_tx = self.signal_tx.clone();
        let peer_id = self.peer_id;
        wasm_bindgen_futures::spawn_local(async move {
            let promise = if restart {
                let options = RtcOfferOptions::new();
                options.set_ice_restart(true);
                conn.create_offer_with_rtc_offer_options(&options)
            } else {
                conn.create_offer()
            };
            let offer = match JsFuture::from(promise).await {
                Ok(offer) => offer,
                Err(error) => {
                    log::warn!("peer {peer_id:#x}: could not create an offer: {error:?}");
                    return;
                }
            };
            let Some(sdp) = sdp_of(peer_id, "offer", &offer) else {
                return;
            };

            // Per spec: setLocalDescription must be called before sending the offer.
            let desc = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
            desc.set_sdp(&sdp);
            if let Err(error) = JsFuture::from(conn.set_local_description(&desc)).await {
                log::warn!("peer {peer_id:#x}: could not apply our offer: {error:?}");
                return;
            }

            let _ = signal_tx.send(OutgoingSignal {
                peer: peer_id,
                signal: PeerSignal::Offer(sdp),
            });
        });
    }
}

/// Which kind of pair the browser settled on, via `getStats`.
///
/// There is no property for this: `RTCPeerConnection` exposes the selected pair only through the
/// stats report, which is a maplike of records keyed by id. The pair record names its two
/// candidates by id, and the candidate records carry the `candidateType` that actually answers the
/// question — so it takes two passes over the same report.
///
/// A cast to [`js_sys::Map`] rather than a typed `RtcStatsReport`: the report is maplike and
/// carries `forEach`, which is all this needs, and the typed wrapper would mean another `web-sys`
/// feature for no gain.
async fn selected_route(conn: &RtcPeerConnection) -> Option<PeerRoute> {
    let report = wasm_bindgen_futures::JsFuture::from(conn.get_stats())
        .await
        .ok()?;
    let report: js_sys::Map = report.unchecked_into();

    let string_field = |value: &JsValue, key: &str| -> Option<String> {
        js_sys::Reflect::get(value, &JsValue::from_str(key))
            .ok()?
            .as_string()
    };

    // Pass one: the pair that won. Browsers differ on whether they mark it `succeeded`, or
    // `nominated`, or both, so either will do.
    let mut candidates: Option<(String, String)> = None;
    report.for_each(&mut |value, _key| {
        if candidates.is_some() || string_field(&value, "type").as_deref() != Some("candidate-pair")
        {
            return;
        }
        let nominated = js_sys::Reflect::get(&value, &JsValue::from_str("nominated"))
            .ok()
            .and_then(|flag| flag.as_bool())
            .unwrap_or(false);
        let succeeded = string_field(&value, "state").as_deref() == Some("succeeded");
        if !nominated && !succeeded {
            return;
        }
        if let (Some(local), Some(remote)) = (
            string_field(&value, "localCandidateId"),
            string_field(&value, "remoteCandidateId"),
        ) {
            candidates = Some((local, remote));
        }
    });
    let (local_id, remote_id) = candidates?;

    // Pass two: either end being a relay candidate means the traffic goes through the relay.
    let mut relayed = false;
    report.for_each(&mut |value, _key| {
        let Some(id) = string_field(&value, "id") else {
            return;
        };
        if id != local_id && id != remote_id {
            return;
        }
        if string_field(&value, "candidateType").as_deref() == Some("relay") {
            relayed = true;
        }
    });

    Some(if relayed {
        PeerRoute::Relayed
    } else {
        PeerRoute::Direct
    })
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

    // First, so that whoever listens hears of this peer before anything happens to it.
    let _ = peer_state_tx.send((peer_id, PeerState::Connecting));

    let restarter = Restarter {
        peer_id,
        connection: conn.clone(),
        signal_tx: signal_tx.clone(),
        peer_state_tx: peer_state_tx.clone(),
        recovery: Arc::new(Mutex::new(Recovery::new(role))),
        discarded_candidates,
    };

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
            let json = match js_sys::JSON::stringify(&candidate.to_json()) {
                Ok(json) => String::from(json),
                Err(error) => {
                    log::warn!(
                        "peer {peer_id:#x}: dropping a local candidate that would not \
                         serialise: {error:?}"
                    );
                    return;
                }
            };
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
    // ICE is also where a lost path is noticed and, on the offerer, restarted: `disconnected`
    // and `failed` go through the recovery policy, which decides between a restart offer,
    // waiting for one, and `Failed`. `Connected` from the data channel opening is still the
    // proof that traffic flows; the ICE `connected` that ends a restart is reported too, because
    // the channel never closed and will not open again.
    {
        let conn_for_ice = conn.clone();
        let restarter = restarter.clone();
        let oniceconnectionstatechange: Closure<dyn FnMut(JsValue)> =
            Closure::wrap(Box::new(move |_: JsValue| {
                restarter.on_ice_state(conn_for_ice.ice_connection_state());
            }));
        conn.set_oniceconnectionstatechange(Some(
            oniceconnectionstatechange.as_ref().unchecked_ref(),
        ));
        oniceconnectionstatechange.forget();
    }

    {
        // Both handlers report, and the duplicate costs nothing: `update_peers` reports a state
        // once. Which of the two reaches `Failed` first -- or at all -- differs between browsers,
        // and this is not a signal to be clever about missing. The one exception is a restart
        // under way: `failed` here follows the ICE `failed` that started it, and reporting it
        // would end a recovery that has restarts left.
        let conn_for_state = conn.clone();
        let restarter = restarter.clone();
        let onconnectionstatechange: Closure<dyn FnMut(JsValue)> =
            Closure::wrap(Box::new(move |_: JsValue| {
                let state = conn_for_state.connection_state();
                if state != web_sys::RtcPeerConnectionState::Failed {
                    log::info!("peer {peer_id:#x}: connection state {state:?}");
                } else if restarter.recovery.lock().unwrap().in_progress() {
                    log::info!(
                        "peer {peer_id:#x}: connection failed while ICE is restarting; not \
                         giving up yet"
                    );
                } else {
                    log::warn!(
                        "peer {peer_id:#x}: connection failed. If ICE reported `connected` \
                         before this, the failure is in DTLS or SCTP rather than in reaching \
                         the peer."
                    );
                    restarter.report(PeerState::Failed);
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
    let reliable_dc =
        conn.create_data_channel_with_data_channel_dict("ensemble_reliable", &reliable_config);
    reliable_dc.set_binary_type(RtcDataChannelType::Arraybuffer);

    // Create negotiated unreliable data channel (unordered, no retransmits).
    let unreliable_config = RtcDataChannelInit::new();
    unreliable_config.set_ordered(false);
    unreliable_config.set_max_retransmits(0);
    unreliable_config.set_negotiated(true);
    unreliable_config.set_id(1);
    let unreliable_dc =
        conn.create_data_channel_with_data_channel_dict("ensemble_unreliable", &unreliable_config);
    unreliable_dc.set_binary_type(RtcDataChannelType::Arraybuffer);

    // Use the reliable channel for connection state signaling.
    let ps_tx = peer_state_tx.clone();
    let conn_for_route = conn.clone();
    let onopen: Closure<dyn FnMut(JsValue)> = Closure::wrap(Box::new(move |_: JsValue| {
        let _ = ps_tx.send((peer_id, PeerState::Connected));

        // Same moment, same reasoning as the native backend: a negotiated channel cannot open
        // before ICE has nominated a pair, so the answer exists now and traffic is about to use
        // it. `getStats` is the only way to ask a browser — there is no property for it.
        let route_tx = route_tx.clone();
        let conn = conn_for_route.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match selected_route(&conn).await {
                Some(route) => {
                    log::info!("peer {peer_id:#x}: {} pair", route.label());
                    let _ = route_tx.send((peer_id, route));
                }
                None => log::debug!("peer {peer_id:#x}: no candidate pair to report"),
            }
        });
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
            let received_at = Instant::now();
            if let Ok(buf) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                let arr = js_sys::Uint8Array::new(&buf);
                let _ =
                    msg_tx_reliable.send((peer_id, arr.to_vec().into_boxed_slice(), received_at));
            }
        }));
    reliable_dc.set_onmessage(Some(onmessage_reliable.as_ref().unchecked_ref()));
    onmessage_reliable.forget();

    let msg_tx_unreliable = message_tx;
    let onmessage_unreliable: Closure<dyn FnMut(MessageEvent)> =
        Closure::wrap(Box::new(move |event: MessageEvent| {
            let received_at = Instant::now();
            if let Ok(buf) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                let arr = js_sys::Uint8Array::new(&buf);
                let _ =
                    msg_tx_unreliable.send((peer_id, arr.to_vec().into_boxed_slice(), received_at));
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
        restarter,
    }
}

/// The `sdp` string of a description the browser produced, or `None` — with a line saying so —
/// if it did not produce one.
///
/// Every step of negotiation used to `unwrap()` its result, and a promise the browser rejects is
/// a panic on wasm: `unreachable` executed, the tab's whole game gone. Any lobby member can cause
/// that by sending an offer that is not SDP, and the native backend has always answered the same
/// input with a warning (see `run_signal_queue` there). This and the `match`es below make the
/// browser do the same.
fn sdp_of(peer_id: u128, what: &str, description: &JsValue) -> Option<String> {
    let sdp = js_sys::Reflect::get(description, &JsValue::from_str("sdp"))
        .ok()
        .and_then(|value| value.as_string());
    if sdp.is_none() {
        log::warn!("peer {peer_id:#x}: the browser's {what} carries no SDP");
    }
    sdp
}

/// Make and send this side's offer. `restart` asks for new ICE credentials and a fresh gather.
pub(crate) fn create_offer(pc: &WasmPeerConnection, restart: bool) {
    pc.restarter.spawn_offer(restart);
}

/// See [`EnsembleSocket::restart_ice`](crate::EnsembleSocket::restart_ice).
pub(crate) fn restart_ice(pc: &WasmPeerConnection) {
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

pub(crate) fn role(pc: &WasmPeerConnection) -> Role {
    pc.restarter.recovery.lock().unwrap().role()
}

/// Apply an offer and answer it. A second offer on a connection that exists is an ICE restart:
/// the browser reads the new credentials out of the SDP and restarts its own agent to match.
pub(crate) fn accept_offer(pc: &WasmPeerConnection, offer_sdp: &str) {
    let conn = pc.connection.clone();
    let peer_id = pc.restarter.peer_id;
    let signal_tx = pc.restarter.signal_tx.clone();
    let offer_sdp = offer_sdp.to_string();
    let rds = pc.remote_desc_set.clone();
    let pending = pc.pending_candidates.clone();
    let discarded = Arc::clone(&pc.restarter.discarded_candidates);
    wasm_bindgen_futures::spawn_local(async move {
        let remote_desc = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        remote_desc.set_sdp(&offer_sdp);
        // The one line here that another peer controls. Garbage in its offer is its bug, and
        // it ends here as a warning rather than as the tab.
        if let Err(error) = JsFuture::from(conn.set_remote_description(&remote_desc)).await {
            log::warn!("peer {peer_id:#x}: could not apply its offer: {error:?}");
            return;
        }

        // Flush buffered candidates now that remote description is set.
        *rds.lock().unwrap() = true;
        let buffered: Vec<String> = pending.lock().unwrap().drain(..).collect();
        for c in buffered {
            apply_ice_candidate(&conn, &c, &discarded).await;
        }

        let answer = match JsFuture::from(conn.create_answer()).await {
            Ok(answer) => answer,
            Err(error) => {
                log::warn!("peer {peer_id:#x}: could not answer: {error:?}");
                return;
            }
        };
        let Some(sdp) = sdp_of(peer_id, "answer", &answer) else {
            return;
        };

        // Per spec: setLocalDescription must be called before sending the answer.
        let desc = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        desc.set_sdp(&sdp);
        if let Err(error) = JsFuture::from(conn.set_local_description(&desc)).await {
            log::warn!("peer {peer_id:#x}: could not apply our answer: {error:?}");
            return;
        }

        let _ = signal_tx.send(OutgoingSignal {
            peer: peer_id,
            signal: PeerSignal::Answer(sdp),
        });
    });
}

pub(crate) fn set_remote_answer(pc: &WasmPeerConnection, sdp: &str) {
    let conn = pc.connection.clone();
    let peer_id = pc.restarter.peer_id;
    let sdp = sdp.to_string();
    let rds = pc.remote_desc_set.clone();
    let pending = pc.pending_candidates.clone();
    let discarded = Arc::clone(&pc.restarter.discarded_candidates);
    wasm_bindgen_futures::spawn_local(async move {
        let desc = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        desc.set_sdp(&sdp);
        if let Err(error) = JsFuture::from(conn.set_remote_description(&desc)).await {
            log::warn!("peer {peer_id:#x}: could not apply its answer: {error:?}");
            return;
        }

        // Flush buffered candidates now that remote description is set.
        *rds.lock().unwrap() = true;
        let buffered: Vec<String> = pending.lock().unwrap().drain(..).collect();
        for c in buffered {
            apply_ice_candidate(&conn, &c, &discarded).await;
        }
    });
}

pub(crate) fn add_ice_candidate(pc: &WasmPeerConnection, candidate_json: &str) {
    // Buffer candidates until remote description is set.
    if !*pc.remote_desc_set.lock().unwrap() {
        let mut pending = pc.pending_candidates.lock().unwrap();
        pending.push(candidate_json.to_string());
        // Held, not applied. Whatever sets the remote description has to flush these, and if it
        // never runs they are never applied and never reported -- a candidate that arrived and
        // did nothing, which reads from the far end exactly like one that never arrived.
        log::info!(
            "buffering a remote candidate until the remote description is set ({} held)",
            pending.len()
        );
        return;
    }

    let conn = pc.connection.clone();
    let json = candidate_json.to_string();
    let discarded = Arc::clone(&pc.restarter.discarded_candidates);
    wasm_bindgen_futures::spawn_local(async move {
        apply_ice_candidate(&conn, &json, &discarded).await;
    });
}

async fn apply_ice_candidate(conn: &RtcPeerConnection, json: &str, discarded: &AtomicU64) {
    let Ok(parsed) = js_sys::JSON::parse(json) else {
        log::warn!("discarding an ICE candidate that is not valid JSON");
        discarded.fetch_add(1, Ordering::Relaxed);
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
        discarded.fetch_add(1, Ordering::Relaxed);
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
