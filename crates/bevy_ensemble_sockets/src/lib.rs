#[cfg(not(target_arch = "wasm32"))]
mod native;
mod recovery;
#[cfg(target_arch = "wasm32")]
mod wasm;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

/// Monotonic clock used to stamp received messages (works natively and on wasm).
pub use web_time::Instant;

/// Signal data exchanged between peers via the signalling server.
///
/// A second `Offer` on a connection that exists is a renegotiation — in this crate, always an
/// ICE restart — and is answered like the first. Nothing new on the wire for that: an offer is
/// an offer, and the SDP inside it is what says the ICE credentials changed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum PeerSignal {
    Offer(String),
    Answer(String),
    IceCandidate(String),
}

/// How long a peer may stay [`Reconnecting`](PeerState::Reconnecting) before it is
/// [`Failed`](PeerState::Failed).
///
/// Measured from the moment the loss was noticed, across however many restart offers fit in it.
/// A network switch that is going to work is done in a second or two — gather, one round of
/// checks, a nominated pair — so fifteen is not a budget a restart spends; it is how long a game
/// keeps a player's seat warm before deciding they are gone.
pub const ICE_RESTART_TIMEOUT: Duration = Duration::from_secs(15);

/// How many ICE restarts one connection may make, [`restart_ice`](EnsembleSocket::restart_ice)
/// included, before a lost path is reported [`Failed`](PeerState::Failed) instead.
///
/// Bounded because a restart that keeps being needed is a path that keeps going away, and a
/// connection that spends the session reconnecting is worse to play against than one that ends.
pub const MAX_ICE_RESTARTS: u32 = 3;

/// Peer connection state, as reported by [`EnsembleSocket::update_peers`].
///
/// The order a connection goes through them: `Connecting`, then `Connected`; from there
/// `Reconnecting` and back to `Connected` any number of times (bounded by [`MAX_ICE_RESTARTS`]),
/// and finally `Disconnected` or `Failed`. Nothing is reported after `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    /// A connection has been created and nothing has happened on it yet: an offer is being made
    /// or answered, candidates are being gathered, no pair has been checked.
    ///
    /// Reported once, first, for every peer. It is what lets a listener tell "this peer is on
    /// its way" from "this peer was never heard of", which before this were the same silence.
    Connecting,
    /// The reliable data channel is open. Traffic flows.
    Connected,
    /// ICE lost the path to this peer and is restarting: new candidates are being gathered and
    /// a fresh offer/answer is going through the signalling server. The data channels are still
    /// open, and whatever is sent meanwhile is queued by SCTP and delivered once a pair is found
    /// again — so a game keeps sending, and keeps the player's seat.
    ///
    /// The usual causes are a phone switching from Wi-Fi to cellular and a NAT rebinding a
    /// mapping mid-session. Ends in `Connected` when ICE finds a pair, or in `Failed` after
    /// [`ICE_RESTART_TIMEOUT`] or [`MAX_ICE_RESTARTS`], whichever comes first. Only the side
    /// that made the original offer restarts; the other side reports this state while it waits
    /// for the restart offer.
    Reconnecting,
    /// A connection that was open has ended: the data channel closed, or the peer was
    /// disconnected on purpose.
    Disconnected,
    /// The connection is over and will not be coming back: ICE found no working pair, the
    /// transport gave up, or a restart did not find a path in time.
    ///
    /// Distinct from [`Disconnected`](PeerState::Disconnected), which means a connection that
    /// existed has ended cleanly. This one either never opened or was lost and could not be
    /// restored, and the difference matters to whoever is listening: a peer that drops
    /// mid-session was playing a moment ago, where a peer that fails to connect leaves somebody
    /// waiting on a screen that will never change.
    Failed,
}

/// How a peer's traffic actually reaches it, once ICE has nominated a pair.
///
/// Worth surfacing rather than inferring from the ICE servers offered: a build configured with a
/// relay still connects directly whenever it can, so "a relay was available" and "a relay is being
/// used" are different facts, and only the second one costs latency. A player reporting that the
/// game feels worse than usual is answering a different question depending on which is true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRoute {
    /// A direct pair — host or server-reflexive at both ends. No third party in the path.
    Direct,
    /// Through a TURN relay, because no direct pair worked. Costs the relay's round trip, and is
    /// the difference between a slower session and no session at all.
    Relayed,
}

impl PeerRoute {
    /// For a readout that has one column to spend on this.
    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relayed => "relayed",
        }
    }
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
    route_tx: mpsc::UnboundedSender<(u128, PeerRoute)>,
    route_rx: mpsc::UnboundedReceiver<(u128, PeerRoute)>,
    message_tx: mpsc::UnboundedSender<(u128, Box<[u8]>, Instant)>,
    message_rx: mpsc::UnboundedReceiver<(u128, Box<[u8]>, Instant)>,
    #[cfg(not(target_arch = "wasm32"))]
    peers: HashMap<u128, native::NativePeerConnection>,
    #[cfg(target_arch = "wasm32")]
    peers: HashMap<u128, wasm::WasmPeerConnection>,
    states: HashMap<u128, PeerState>,
    routes: HashMap<u128, PeerRoute>,
    /// When each peer currently `Reconnecting` was first reported so, for [`ICE_RESTART_TIMEOUT`].
    ///
    /// Checked from [`update_peers`](EnsembleSocket::update_peers) rather than by a timer task,
    /// because the game already calls that every frame on both platforms, and a deadline that
    /// is polled needs no runtime, no `setTimeout`, and nothing to cancel when the peer goes.
    reconnecting_since: HashMap<u128, Instant>,
    /// Remote candidates the transport refused, over the socket's lifetime. See
    /// [`discarded_candidates`](EnsembleSocket::discarded_candidates).
    discarded_candidates: Arc<AtomicU64>,
    ice_servers: IceServers,
    #[cfg(not(target_arch = "wasm32"))]
    runtime_handle: tokio::runtime::Handle,
}

impl EnsembleSocket {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(runtime_handle: tokio::runtime::Handle) -> Self {
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        let (peer_state_tx, peer_state_rx) = mpsc::unbounded_channel();
        let (route_tx, route_rx) = mpsc::unbounded_channel();
        let (message_tx, message_rx) = mpsc::unbounded_channel();
        Self {
            signal_tx,
            signal_rx,
            peer_state_tx,
            peer_state_rx,
            route_tx,
            route_rx,
            message_tx,
            message_rx,
            peers: HashMap::new(),
            states: HashMap::new(),
            routes: HashMap::new(),
            reconnecting_since: HashMap::new(),
            discarded_candidates: Arc::new(AtomicU64::new(0)),
            ice_servers: IceServers::default(),
            runtime_handle,
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn new() -> Self {
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        let (peer_state_tx, peer_state_rx) = mpsc::unbounded_channel();
        let (route_tx, route_rx) = mpsc::unbounded_channel();
        let (message_tx, message_rx) = mpsc::unbounded_channel();
        Self {
            signal_tx,
            signal_rx,
            peer_state_tx,
            peer_state_rx,
            route_tx,
            route_rx,
            message_tx,
            message_rx,
            peers: HashMap::new(),
            states: HashMap::new(),
            routes: HashMap::new(),
            reconnecting_since: HashMap::new(),
            discarded_candidates: Arc::new(AtomicU64::new(0)),
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
                recovery::Role::Offerer,
                self.signal_tx.clone(),
                self.peer_state_tx.clone(),
                self.route_tx.clone(),
                self.message_tx.clone(),
                &self.ice_servers,
                Arc::clone(&self.discarded_candidates),
                self.runtime_handle.clone(),
            );
            native::create_offer(&pc, false);
            self.peers.insert(peer_id, pc);
        }

        #[cfg(target_arch = "wasm32")]
        {
            let pc = wasm::create_peer_connection(
                peer_id,
                recovery::Role::Offerer,
                self.signal_tx.clone(),
                self.peer_state_tx.clone(),
                self.route_tx.clone(),
                self.message_tx.clone(),
                &self.ice_servers,
                Arc::clone(&self.discarded_candidates),
            );
            wasm::create_offer(&pc, false);
            self.peers.insert(peer_id, pc);
        }
    }

    /// Restart ICE on the connection to `peer`: gather candidates afresh and renegotiate through
    /// the signalling server, keeping the data channels and whatever is queued on them.
    ///
    /// What the socket does by itself when ICE reports the path lost; public so that a game's
    /// "reconnect" button can do it on demand — a player who knows they just changed networks
    /// need not wait for ICE to notice. `Reconnecting` is reported, then `Connected` when the new
    /// pair is nominated, or `Failed` after [`ICE_RESTART_TIMEOUT`].
    ///
    /// Only the side that made the original offer can restart, because a restart *is* an offer
    /// and this protocol refuses offers from the other side. On the answerer this logs and does
    /// nothing: the offerer's ICE agent notices the same loss and restarts on its own.
    ///
    /// Counts against [`MAX_ICE_RESTARTS`]; past it, the peer is reported `Failed`.
    pub fn restart_ice(&mut self, peer: u128) {
        let Some(pc) = self.peers.get(&peer) else {
            log::warn!("peer {peer:#x}: cannot restart ICE, no connection to it exists");
            return;
        };
        #[cfg(not(target_arch = "wasm32"))]
        native::restart_ice(pc);
        #[cfg(target_arch = "wasm32")]
        wasm::restart_ice(pc);
    }

    /// How many remote ICE candidates the transport has refused since this socket was made.
    ///
    /// A candidate is an address the peer will not send again, so each one refused is a route
    /// that can never be tried, and losing every candidate on both sides is a connection that
    /// gathers happily and never pairs. The count is zero on a healthy socket; the candidates
    /// that arrive before the description they belong to are held, not refused. Exposed so
    /// that a soak test can assert that, rather than grep a log for it.
    pub fn discarded_candidates(&self) -> u64 {
        self.discarded_candidates.load(Ordering::Relaxed)
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
                if let Some(pc) = self.peers.get(&sender) {
                    // A second offer from the peer this side answered is a renegotiation, which
                    // here means its ICE restarted (see `restart_ice`). Answered like the first;
                    // the transport reads the new credentials out of the SDP and restarts its
                    // own agent to match. A second offer from a peer this side *offered to* is
                    // glare, and stays refused.
                    #[cfg(not(target_arch = "wasm32"))]
                    let answerer = native::role(pc) == recovery::Role::Answerer;
                    #[cfg(target_arch = "wasm32")]
                    let answerer = wasm::role(pc) == recovery::Role::Answerer;
                    if !answerer {
                        log::warn!(
                            "peer {sender:#x}: ignoring a second offer; a connection to it \
                             already exists. Both sides may believe they are the offerer."
                        );
                        return;
                    }
                    log::info!("peer {sender:#x}: applying a new offer from it, restarting ICE");
                    #[cfg(not(target_arch = "wasm32"))]
                    native::accept_offer(pc, &sdp);
                    #[cfg(target_arch = "wasm32")]
                    wasm::accept_offer(pc, &sdp);
                    return;
                }
                log::info!("peer {sender:#x}: applying its offer, answering as the callee");

                #[cfg(not(target_arch = "wasm32"))]
                {
                    let pc = native::create_peer_connection(
                        sender,
                        recovery::Role::Answerer,
                        self.signal_tx.clone(),
                        self.peer_state_tx.clone(),
                        self.route_tx.clone(),
                        self.message_tx.clone(),
                        &self.ice_servers,
                        Arc::clone(&self.discarded_candidates),
                        self.runtime_handle.clone(),
                    );
                    native::accept_offer(&pc, &sdp);
                    self.peers.insert(sender, pc);
                }

                #[cfg(target_arch = "wasm32")]
                {
                    let pc = wasm::create_peer_connection(
                        sender,
                        recovery::Role::Answerer,
                        self.signal_tx.clone(),
                        self.peer_state_tx.clone(),
                        self.route_tx.clone(),
                        self.message_tx.clone(),
                        &self.ice_servers,
                        Arc::clone(&self.discarded_candidates),
                    );
                    wasm::accept_offer(&pc, &sdp);
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
    ///
    /// # Why this is not a `connected: bool`
    ///
    /// It was, and the comparison it made was `was_connected != is_connected`. For a connection
    /// that never opened, both sides of that are `false`, so a failure was reported as no change
    /// at all -- and a peer that never connects is the single case a consumer most needs to hear
    /// about, because nothing else in this crate will ever mention it again. Tracking the last
    /// state instead of a flag is what lets [`PeerState::Failed`] through.
    ///
    /// `Disconnected` keeps its old meaning deliberately: a close on a channel that never opened
    /// says nothing a listener can act on, and reporting it would turn one failure into two
    /// events describing it differently.
    ///
    /// # Reconnecting, and the deadline on it
    ///
    /// `Reconnecting` is reported only from `Connected`: a restart is something that happens to
    /// a connection that existed. It is also where [`ICE_RESTART_TIMEOUT`] starts — armed here
    /// whether or not the report is passed on, so that a restart nobody was told about is still
    /// bounded — and a peer still reconnecting when it runs out is reported `Failed` from this
    /// method, on whichever call first finds it late.
    ///
    /// Nothing is reported after `Failed`. The transport may well find a pair a moment after the
    /// deadline, and a `Connected` that follows a `Failed` would put a peer back into a session
    /// that has already been torn down around it.
    pub fn update_peers(&mut self) -> Vec<(u128, PeerState)> {
        let mut changes = Vec::new();
        while let Ok((peer, state)) = self.peer_state_rx.try_recv() {
            let previous = self.states.get(&peer).copied();
            match state {
                PeerState::Reconnecting => {
                    self.reconnecting_since
                        .entry(peer)
                        .or_insert_with(Instant::now);
                }
                PeerState::Connecting => {}
                PeerState::Connected | PeerState::Disconnected | PeerState::Failed => {
                    self.reconnecting_since.remove(&peer);
                }
            }
            let report = match (previous, state) {
                (Some(previous), current) if previous == current => false,
                (Some(PeerState::Failed), _) => false,
                (None, PeerState::Connecting) => true,
                (Some(_), PeerState::Connecting) => false,
                (_, PeerState::Connected) => true,
                (_, PeerState::Failed) => true,
                (Some(PeerState::Connected), PeerState::Reconnecting) => true,
                (_, PeerState::Reconnecting) => false,
                (Some(PeerState::Connected | PeerState::Reconnecting), PeerState::Disconnected) => {
                    true
                }
                (_, PeerState::Disconnected) => false,
            };
            if report {
                self.states.insert(peer, state);
                changes.push((peer, state));
            }
        }

        let now = Instant::now();
        let late: Vec<u128> = self
            .reconnecting_since
            .iter()
            .filter(|(_, since)| now.duration_since(**since) >= ICE_RESTART_TIMEOUT)
            .map(|(peer, _)| *peer)
            .collect();
        for peer in late {
            self.reconnecting_since.remove(&peer);
            if self.states.get(&peer) == Some(&PeerState::Failed) {
                continue;
            }
            log::warn!(
                "peer {peer:#x}: ICE has not found a path again in {}s; giving up on it",
                ICE_RESTART_TIMEOUT.as_secs()
            );
            self.states.insert(peer, PeerState::Failed);
            changes.push((peer, PeerState::Failed));
        }
        changes
    }

    /// Drain the routes ICE has settled on since the last call.
    ///
    /// Reported once per change rather than once per query, matching [`update_peers`]: the route
    /// is decided when a pair is nominated and only changes if ICE renominates, so a consumer that
    /// polls every frame would otherwise re-apply the same fact for the length of the session.
    ///
    /// [`update_peers`]: EnsembleSocket::update_peers
    pub fn update_routes(&mut self) -> Vec<(u128, PeerRoute)> {
        let mut changes = Vec::new();
        while let Ok((peer, route)) = self.route_rx.try_recv() {
            if self.routes.get(&peer).copied() != Some(route) {
                self.routes.insert(peer, route);
                changes.push((peer, route));
            }
        }
        changes
    }

    /// How this peer is reached, if ICE has settled on a pair yet.
    pub fn route(&self, peer: u128) -> Option<PeerRoute> {
        self.routes.get(&peer).copied()
    }

    /// The peers whose data channels are open: `Connected`, and `Reconnecting` too.
    ///
    /// A peer whose ICE is restarting still has its channels, and a send to it is queued by
    /// SCTP and delivered when the new pair is up — which is what a game wants, since the
    /// pings and inputs sent during those seconds are what make the reconnect seamless rather
    /// than a freeze followed by a catch-up. Leaving such a peer out here would make every
    /// consumer that gates its sends on this list go silent for exactly the window that
    /// decides whether the session survives.
    pub fn connected_peers(&self) -> impl Iterator<Item = u128> + '_ {
        self.states
            .iter()
            .filter(|&(_, &state)| matches!(state, PeerState::Connected | PeerState::Reconnecting))
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
    ///
    /// Each entry carries the [`Instant`] at which the bytes came off the data channel
    /// (stamped in the `on_message` callback, not when this method is called).
    pub fn receive(&mut self) -> Vec<(u128, Box<[u8]>, Instant)> {
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
    ///
    /// Dropping the connection is what closes it. On native that is an explicit `close()` on the
    /// `RTCPeerConnection` and the end of the peer's worker tasks, both from the connection's
    /// `Drop` — webrtc-rs leaks its ICE agent and sockets otherwise — so [`disconnect_all`] and
    /// dropping the socket close their peers the same way.
    ///
    /// [`disconnect_all`]: EnsembleSocket::disconnect_all
    pub fn disconnect_peer(&mut self, peer: u128) {
        if self.peers.remove(&peer).is_some() {
            self.states.remove(&peer);
            self.routes.remove(&peer);
            self.reconnecting_since.remove(&peer);
        }
    }

    /// Disconnect all peers.
    pub fn disconnect_all(&mut self) {
        self.peers.clear();
        self.states.clear();
        self.routes.clear();
        self.reconnecting_since.clear();
    }
}
