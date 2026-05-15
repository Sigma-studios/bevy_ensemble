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
        }
    }

    /// Initiate a WebRTC connection to a peer (we create the offer).
    pub fn connect_peer(&mut self, peer_id: u128) {
        if self.peers.contains_key(&peer_id) {
            return;
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let pc = native::create_peer_connection(
                peer_id,
                self.signal_tx.clone(),
                self.peer_state_tx.clone(),
                self.message_tx.clone(),
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
            );
            wasm::create_offer(&pc, peer_id, self.signal_tx.clone());
            self.peers.insert(peer_id, pc);
        }
    }

    /// Handle an incoming signal from a remote peer.
    pub fn receive_signal(&mut self, sender: u128, signal: PeerSignal) {
        match signal {
            PeerSignal::Offer(sdp) => {
                if self.peers.contains_key(&sender) {
                    return;
                }

                #[cfg(not(target_arch = "wasm32"))]
                {
                    let pc = native::create_peer_connection(
                        sender,
                        self.signal_tx.clone(),
                        self.peer_state_tx.clone(),
                        self.message_tx.clone(),
                        self.runtime_handle.clone(),
                    );
                    native::accept_offer(
                        &pc,
                        sender,
                        &sdp,
                        self.signal_tx.clone(),
                        &self.runtime_handle,
                    );
                    self.peers.insert(sender, pc);
                }

                #[cfg(target_arch = "wasm32")]
                {
                    let pc = wasm::create_peer_connection(
                        sender,
                        self.signal_tx.clone(),
                        self.peer_state_tx.clone(),
                        self.message_tx.clone(),
                    );
                    wasm::accept_offer(&pc, sender, &sdp, self.signal_tx.clone());
                    self.peers.insert(sender, pc);
                }
            }
            PeerSignal::Answer(sdp) => {
                if let Some(pc) = self.peers.get(&sender) {
                    #[cfg(not(target_arch = "wasm32"))]
                    native::set_remote_answer(pc, &sdp, &self.runtime_handle);
                    #[cfg(target_arch = "wasm32")]
                    wasm::set_remote_answer(pc, &sdp);
                }
            }
            PeerSignal::IceCandidate(candidate) => {
                if let Some(pc) = self.peers.get(&sender) {
                    #[cfg(not(target_arch = "wasm32"))]
                    native::add_ice_candidate(pc, &candidate, &self.runtime_handle);
                    #[cfg(target_arch = "wasm32")]
                    wasm::add_ice_candidate(pc, &candidate);
                }
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
