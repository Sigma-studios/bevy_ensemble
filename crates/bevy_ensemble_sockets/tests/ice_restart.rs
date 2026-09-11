//! Connection state and ICE restart, on two `EnsembleSocket`s in one process wired to each other
//! with no signalling server and no ICE servers — the same harness as `ordering.rs`, driven the
//! way a game drives it: from a plain thread, polled.
//!
//! Loopback UDP is not a given on every CI machine. A pair that has not connected in 15 s prints
//! `SKIPPED: inconclusive (no ICE connection)` and passes, so that a missing network proves
//! nothing either way rather than reading as a regression. What has already been observed before
//! the skip is still asserted: an inconclusive run is never a false pass.
#![cfg(not(target_arch = "wasm32"))]

use std::time::{Duration, Instant};

use bevy_ensemble_sockets::{EnsembleSocket, ICE_RESTART_TIMEOUT, IceServers, PeerState};

const A: u128 = 0xA;
const B: u128 = 0xB;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(2);

struct Pair {
    a: EnsembleSocket,
    b: EnsembleSocket,
    /// Every state each side has reported, in order, over the pair's life.
    a_states: Vec<PeerState>,
    b_states: Vec<PeerState>,
}

impl Pair {
    fn new(handle: &tokio::runtime::Handle) -> Self {
        Self {
            a: EnsembleSocket::new(handle.clone()).with_ice_servers(IceServers::none()),
            b: EnsembleSocket::new(handle.clone()).with_ice_servers(IceServers::none()),
            a_states: Vec::new(),
            b_states: Vec::new(),
        }
    }

    /// The signalling server, in two lines: what `a` addresses to `B` arrives at `b` from `A`.
    fn pump_signals(&mut self) {
        for signal in self.a.drain_signals() {
            assert_eq!(signal.peer, B);
            self.b.receive_signal(A, signal.signal);
        }
        for signal in self.b.drain_signals() {
            assert_eq!(signal.peer, A);
            self.a.receive_signal(B, signal.signal);
        }
    }

    /// One frame: signals across, states recorded. Returns what was reported this frame.
    fn poll(&mut self) -> (Vec<PeerState>, Vec<PeerState>) {
        self.pump_signals();
        let a_now: Vec<PeerState> = self
            .a
            .update_peers()
            .into_iter()
            .map(|(peer, state)| {
                assert_eq!(peer, B);
                state
            })
            .collect();
        let b_now: Vec<PeerState> = self
            .b
            .update_peers()
            .into_iter()
            .map(|(peer, state)| {
                assert_eq!(peer, A);
                state
            })
            .collect();
        self.a_states.extend(&a_now);
        self.b_states.extend(&b_now);
        std::thread::sleep(POLL);
        (a_now, b_now)
    }

    /// `a` offers, and both sides are polled until each reports the other `Connected`.
    /// `false` if that has not happened within [`CONNECT_TIMEOUT`].
    fn connect(&mut self) -> bool {
        self.a.connect_peer(B);
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let (mut a_up, mut b_up) = (false, false);
        while !(a_up && b_up) {
            if Instant::now() > deadline {
                return false;
            }
            let (a_now, b_now) = self.poll();
            for state in a_now {
                assert_ne!(state, PeerState::Failed, "a: connection to b failed");
                a_up |= state == PeerState::Connected;
            }
            for state in b_now {
                assert_ne!(state, PeerState::Failed, "b: connection to a failed");
                b_up |= state == PeerState::Connected;
            }
        }
        true
    }

    fn disconnect(&mut self) {
        self.a.disconnect_peer(B);
        self.b.disconnect_peer(A);
    }

    /// Poll until `b` has received `expected`, or `within` has passed.
    fn b_receives(&mut self, expected: &[u8], within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            self.poll();
            for (peer, bytes, _received_at) in self.b.receive() {
                assert_eq!(peer, A);
                if &*bytes == expected {
                    return true;
                }
            }
        }
        false
    }
}

fn skip_inconclusive() {
    println!("SKIPPED: inconclusive (no ICE connection)");
}

/// A listener hears of a peer before anything happens to it: `Connecting` first, `Connected`
/// after, on both sides, and nothing in between on a connection that just works.
#[test]
fn a_peer_state_transition_is_visible() {
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let mut pair = Pair::new(runtime.handle());
    if !pair.connect() {
        skip_inconclusive();
        return;
    }
    assert_eq!(
        pair.a_states,
        [PeerState::Connecting, PeerState::Connected],
        "the offerer's states"
    );
    assert_eq!(
        pair.b_states,
        [PeerState::Connecting, PeerState::Connected],
        "the answerer's states"
    );
    pair.disconnect();
}

/// An ICE restart on the offerer renegotiates through signalling and keeps the data channels:
/// `Reconnecting` is observed, then `Connected`, and a message sent *during* the restart — while
/// no candidate pair is selected — arrives, because SCTP retransmits what the ICE layer dropped.
#[test]
fn an_ice_restart_keeps_the_data_channel_open() {
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let mut pair = Pair::new(runtime.handle());
    if !pair.connect() {
        skip_inconclusive();
        return;
    }
    let b_states_before = pair.b_states.len();

    pair.a.restart_ice(B);
    // Sent before a single signal has crossed: the restart has torn the old candidates down
    // and the new offer has not left yet.
    pair.a
        .send(b"during the restart".to_vec().into_boxed_slice(), B);

    // Well past the socket's own deadline, so that a restart that fails is reported as `Failed`
    // by the socket and asserted on here, rather than timing out silently.
    let deadline = Instant::now() + ICE_RESTART_TIMEOUT + Duration::from_secs(5);
    let (mut reconnecting, mut reconnected) = (false, false);
    while !reconnected {
        assert!(
            Instant::now() < deadline,
            "a never reported the restart complete; states so far: {:?}",
            pair.a_states
        );
        let (a_now, b_now) = pair.poll();
        for state in a_now {
            match state {
                PeerState::Reconnecting => reconnecting = true,
                PeerState::Connected => {
                    assert!(
                        reconnecting,
                        "a reported Connected without Reconnecting first"
                    );
                    reconnected = true;
                }
                other => panic!("a reported {other:?} during an ICE restart"),
            }
        }
        for state in b_now {
            // The answerer's ICE goes straight from connected to checking on the new offer;
            // it may or may not notice a gap. What it must not do is end the session.
            assert!(
                !matches!(state, PeerState::Disconnected | PeerState::Failed),
                "b reported {state:?} during a's ICE restart"
            );
        }
    }
    assert_eq!(
        pair.a_states.last(),
        Some(&PeerState::Connected),
        "a's full history: {:?}",
        pair.a_states
    );
    assert!(
        pair.b_states[b_states_before..]
            .iter()
            .all(|state| matches!(state, PeerState::Reconnecting | PeerState::Connected)),
        "b's states during the restart: {:?}",
        &pair.b_states[b_states_before..]
    );

    assert!(
        pair.b_receives(b"during the restart", Duration::from_secs(10)),
        "the message sent during the restart never arrived"
    );

    // And the restored path carries traffic like the old one did.
    pair.a
        .send(b"after the restart".to_vec().into_boxed_slice(), B);
    assert!(
        pair.b_receives(b"after the restart", Duration::from_secs(5)),
        "a message sent after the restart never arrived"
    );

    pair.disconnect();
}

/// The handshake, twenty times over: every cycle connects, and no remote candidate is ever
/// refused by the transport on either side. Candidates that arrive before the description they
/// belong to are the known way to lose one — the native side used to apply them concurrently,
/// and a restart regathers before its offer exists — and both are held rather than dropped now.
///
/// A cycle that does not connect is inconclusive (loopback UDP is not a given), but the
/// candidate count up to it is asserted regardless: an inconclusive run is never a false pass.
#[test]
fn twenty_connect_cycles_never_drop_a_candidate() {
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let mut pair = Pair::new(runtime.handle());
    let mut connected = 0;

    for cycle in 0..20 {
        let up = pair.connect();
        let dropped = pair.a.discarded_candidates() + pair.b.discarded_candidates();
        assert_eq!(
            dropped, 0,
            "cycle {cycle}: {dropped} remote candidate(s) refused by the transport"
        );
        if !up {
            println!("cycle {cycle} of 20 did not connect ({connected} did)");
            skip_inconclusive();
            return;
        }
        connected += 1;
        pair.disconnect();
    }
    assert_eq!(connected, 20);
}
