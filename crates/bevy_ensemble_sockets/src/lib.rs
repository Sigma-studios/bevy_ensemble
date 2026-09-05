#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod wasm;

use std::collections::HashMap;
use tokio::sync::mpsc;

/// Signal data exchanged between peers via the signalling server.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum PeerSignal {
    Offer(String),
    Answer(String),
    IceCandidate(String),
}

/// Peer connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    Connected,
    Disconnected,
}

/// Outbound signal destined for a remote peer (must be relayed via signalling server).
#[derive(Debug)]
pub struct OutgoingSignal {
    pub peer: u128,
    pub signal: PeerSignal,
}

/// One ICE server a peer connection may gather candidates from.
///
/// `username` and `credential` are only read for TURN; leave them empty for STUN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

impl IceServer {
    /// A credential-less server, which is every STUN server.
    pub fn stun(url: impl Into<String>) -> Self {
        Self {
            urls: vec![url.into()],
            username: String::new(),
            credential: String::new(),
        }
    }
}

/// The ICE servers peer connections gather candidates from.
///
/// The default is the pair of public Google STUN servers this crate used to hardcode with no way
/// to override them. That default is right for two peers on different networks and pure cost for
/// two on the same machine: gathering waits on servers whose answer is not needed, which makes
/// every local run slower — and, while trickled candidates were still being dropped, made the
/// resulting failure slow as well as total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceServers(pub Vec<IceServer>);

impl Default for IceServers {
    fn default() -> Self {
        Self(vec![IceServer {
            urls: vec![
                "stun:stun.l.google.com:19302".into(),
                "stun:stun1.l.google.com:19302".into(),
            ],
            username: String::new(),
            credential: String::new(),
        }])
    }
}

impl IceServers {
    /// Gather host candidates only. Correct for loopback and a LAN, and for tests.
    pub fn none() -> Self {
        Self(Vec::new())
    }
}

/// A cross-platform WebRTC socket that manages peer connections and data channels.
///
/// Call [`EnsembleSocket::new`] to create one, then use:
/// - [`connect_peer`](EnsembleSocket::connect_peer) to initiate a connection (you are the offerer)
/// - [`receive_signal`](EnsembleSocket::receive_signal) to handle incoming signals (offers/answers/ICE)
/// - [`update_peers`](EnsembleSocket::update_peers) each frame to drain connect/disconnect events
/// - [`send`](EnsembleSocket::send) / [`receive`](EnsembleSocket::receive) for data channel I/O
/// - [`drain_signals`](EnsembleSocket::drain_signals) each frame to get outbound signals for the signalling server
pub struct EnsembleSocket {
    signal_tx: mpsc::UnboundedSender<OutgoingSignal>,
    signal_rx: mpsc::UnboundedReceiver<OutgoingSignal>,
    peer_state_tx: mpsc::UnboundedSender<(u128, PeerState)>,
    peer_state_rx: mpsc::UnboundedReceiver<(u128, PeerState)>,
    message_tx: mpsc::UnboundedSender<(u128, Box<[u8]>)>,
    message_rx: mpsc::UnboundedReceiver<(u128, Box<[u8]>)>,
    #[cfg(not(target_arch = "wasm32"))]
    peers: HashMap<u128, native::NativePeerConnection>,
    #[cfg(target_arch = "wasm32")]
    peers: HashMap<u128, wasm::WasmPeerConnection>,
    connected: HashMap<u128, bool>,
    ice_servers: IceServers,
    #[cfg(not(target_arch = "wasm32"))]
    runtime_handle: tokio::runtime::Handle,
}

impl EnsembleSocket {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(runtime_handle: tokio::runtime::Handle) -> Self {
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        let (peer_state_tx, peer_state_rx) = mpsc::unbounded_channel();
        let (message_tx, message_rx) = mpsc::unbounded_channel();
        Self {
            signal_tx,
            signal_rx,
            peer_state_tx,
            peer_state_rx,
            message_tx,
            message_rx,
            peers: HashMap::new(),
            connected: HashMap::new(),
            ice_servers: IceServers::default(),
            runtime_handle,
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn new() -> Self {
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        let (peer_state_tx, peer_state_rx) = mpsc::unbounded_channel();
        let (message_tx, message_rx) = mpsc::unbounded_channel();
        Self {
            signal_tx,
            signal_rx,
            peer_state_tx,
            peer_state_rx,
            message_tx,
            message_rx,
            peers: HashMap::new(),
            connected: HashMap::new(),
            ice_servers: IceServers::default(),
        }
    }

    /// Gather candidates from these ICE servers rather than the default public STUN pair.
    ///
    /// Takes effect for connections opened after it, which in practice is all of them: the socket
    /// is rebuilt each time a lobby is left.
    pub fn with_ice_servers(mut self, ice_servers: IceServers) -> Self {
        self.ice_servers = ice_servers;
        self
    }

    /// Initiate a WebRTC connection to a peer (we create the offer).
    pub fn connect_peer(&mut self, peer_id: u128) {
        if self.peers.contains_key(&peer_id) {
            log::debug!("peer {peer_id:#x}: already connecting or connected, not offering again");
            return;
        }
        log::info!("peer {peer_id:#x}: opening a connection, offering as the caller");

        #[cfg(not(target_arch = "wasm32"))]
        {
            let pc = native::create_peer_connection(
                peer_id,
                self.signal_tx.clone(),
                self.peer_state_tx.clone(),
                self.message_tx.clone(),
                &self.ice_servers,
                self.runtime_handle.clone(),
            );
            native::create_offer(&pc, peer_id, self.signal_tx.clone(), &self.runtime_handle);
            self.peers.insert(peer_id, pc);
        }

        #[cfg(target_arch = "wasm32")]
        {
            let pc = wasm::create_peer_connection(
                peer_id,
                self.signal_tx.clone(),
                self.peer_state_tx.clone(),
                self.message_tx.clone(),
                &self.ice_servers,
            );
            wasm::create_offer(&pc, peer_id, self.signal_tx.clone());
            self.peers.insert(peer_id, pc);
        }
    }

    /// Handle an incoming signal from a remote peer.
    ///
    /// # Every path that discards a signal says so
    ///
    /// Signalling is an ordered conversation that arrives over a channel this type does not
    /// control, and the ways it can go wrong all look the same from the outside: a data channel
    /// that never opens, on a connection that reported no error of any kind. Answering "was the
    /// offer applied, and how many of its candidates survived?" used to mean adding print
    /// statements to this function, because none of the discards below were audible.
    ///
    /// They are `warn!` rather than `debug!` deliberately. Reaching one of them means a peer sent
    /// something this side had no state for, which is either a bug here or a peer that is not
    /// speaking the protocol — never routine traffic.
    pub fn receive_signal(&mut self, sender: u128, signal: PeerSignal) {
        match signal {
            PeerSignal::Offer(sdp) => {
                if self.peers.contains_key(&sender) {
                    log::warn!(
                        "peer {sender:#x}: ignoring a second offer; a connection to it already \
                         exists. Both sides may believe they are the offerer."
                    );
                    return;
                }
                log::info!("peer {sender:#x}: applying its offer, answering as the callee");

                #[cfg(not(target_arch = "wasm32"))]
                {
                    let pc = native::create_peer_connection(
                        sender,
                        self.signal_tx.clone(),
                        self.peer_state_tx.clone(),
                        self.message_tx.clone(),
                        &self.ice_servers,
                        self.runtime_handle.clone(),
                    );
                    native::accept_offer(&pc, &sdp);
                    self.peers.insert(sender, pc);
                }

                #[cfg(target_arch = "wasm32")]
                {
                    let pc = wasm::create_peer_connection(
                        sender,
                        self.signal_tx.clone(),
                        self.peer_state_tx.clone(),
                        self.message_tx.clone(),
                        &self.ice_servers,
                    );
                    wasm::accept_offer(&pc, sender, &sdp, self.signal_tx.clone());
                    self.peers.insert(sender, pc);
                }
            }
            PeerSignal::Answer(sdp) => {
                let Some(pc) = self.peers.get(&sender) else {
                    log::warn!(
                        "peer {sender:#x}: discarding its answer -- no connection to it exists. \
                         An answer to an offer this side never made."
                    );
                    return;
                };
                log::info!("peer {sender:#x}: applying its answer");
                #[cfg(not(target_arch = "wasm32"))]
                native::set_remote_answer(pc, &sdp);
                #[cfg(target_arch = "wasm32")]
                wasm::set_remote_answer(pc, &sdp);
            }
            PeerSignal::IceCandidate(candidate) => {
                let Some(pc) = self.peers.get(&sender) else {
                    // The address is gone and there is no retry: the peer will not resend it.
                    // Losing every candidate on both sides is a connection that gathers happily
                    // and never pairs, so this is worth a line even though ICE often survives it.
                    log::warn!(
                        "peer {sender:#x}: discarding an ICE candidate that arrived before its \
                         offer -- no connection to it exists yet"
                    );
                    return;
                };
                #[cfg(not(target_arch = "wasm32"))]
                native::add_ice_candidate(pc, &candidate);
                #[cfg(target_arch = "wasm32")]
                wasm::add_ice_candidate(pc, &candidate);
            }
        }
    }

    /// Drain peer state changes since last call.
    pub fn update_peers(&mut self) -> Vec<(u128, PeerState)> {
        let mut changes = Vec::new();
        while let Ok((peer, state)) = self.peer_state_rx.try_recv() {
            let was_connected = self.connected.get(&peer).copied().unwrap_or(false);
            let is_connected = state == PeerState::Connected;
            if was_connected != is_connected {
                self.connected.insert(peer, is_connected);
                changes.push((peer, state));
            }
        }
        changes
    }

    /// Currently connected peer IDs.
    pub fn connected_peers(&self) -> impl Iterator<Item = u128> + '_ {
        self.connected
            .iter()
            .filter(|&(_, &c)| c)
            .map(|(&id, _)| id)
    }

    /// Send binary data to a peer over the reliable (ordered, guaranteed) channel.
    pub fn send(&self, data: Box<[u8]>, peer: u128) {
        self.send_with_mode(data, peer, true);
    }

    /// Send binary data to a peer, choosing reliable or unreliable delivery.
    ///
    /// - `reliable = true`: ordered, guaranteed delivery.
    /// - `reliable = false`: unordered, fire-and-forget (no retransmits).
    pub fn send_with_mode(&self, data: Box<[u8]>, peer: u128, reliable: bool) {
        if let Some(pc) = self.peers.get(&peer) {
            #[cfg(not(target_arch = "wasm32"))]
            native::send_message(pc, data, reliable);
            #[cfg(target_arch = "wasm32")]
            wasm::send_message(pc, &data, reliable);
        }
    }

    /// Receive all pending messages from all peers.
    pub fn receive(&mut self) -> Vec<(u128, Box<[u8]>)> {
        let mut messages = Vec::new();
        while let Ok(msg) = self.message_rx.try_recv() {
            messages.push(msg);
        }
        messages
    }

    /// Drain outbound signals that need to be sent to the signalling server.
    pub fn drain_signals(&mut self) -> Vec<OutgoingSignal> {
        let mut signals = Vec::new();
        while let Ok(s) = self.signal_rx.try_recv() {
            signals.push(s);
        }
        signals
    }

    /// Disconnect a specific peer.
    pub fn disconnect_peer(&mut self, peer: u128) {
        if let Some(_pc) = self.peers.remove(&peer) {
            self.connected.remove(&peer);
            // Dropping the peer connection closes it.
        }
    }

    /// Disconnect all peers.
    pub fn disconnect_all(&mut self) {
        self.peers.clear();
        self.connected.clear();
    }
}
