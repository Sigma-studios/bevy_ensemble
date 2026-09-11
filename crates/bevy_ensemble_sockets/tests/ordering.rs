//! Two `EnsembleSocket`s in one process, wired to each other with no signalling server and no ICE
//! servers, driven the way a game drives them: from a plain thread, polled, with the tokio runtime
//! only ever touched through the handle the socket was given.
//!
//! The ordering tests exist because the native "reliable, ordered" channel was not ordered: one
//! task per `send` raced same-frame packets to the SCTP stream. A burst is what a frame sends.
//!
//! Loopback UDP is not a given on every CI machine. A pair that has not connected in 15 s prints
//! `SKIPPED: inconclusive (no ICE connection)` and passes, so that a missing network proves
//! nothing either way rather than reading as a regression.
#![cfg(not(target_arch = "wasm32"))]

use std::time::{Duration, Instant};

use bevy_ensemble_sockets::{EnsembleSocket, IceServers, PeerState};

const A: u128 = 0xA;
const B: u128 = 0xB;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(2);

struct Pair {
    a: EnsembleSocket,
    b: EnsembleSocket,
}

impl Pair {
    fn new(handle: &tokio::runtime::Handle) -> Self {
        Self {
            a: EnsembleSocket::new(handle.clone()).with_ice_servers(IceServers::none()),
            b: EnsembleSocket::new(handle.clone()).with_ice_servers(IceServers::none()),
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
            self.pump_signals();
            for (peer, state) in self.a.update_peers() {
                assert_eq!(peer, B);
                assert_ne!(state, PeerState::Failed, "a: connection to b failed");
                a_up = state == PeerState::Connected;
            }
            for (peer, state) in self.b.update_peers() {
                assert_eq!(peer, A);
                assert_ne!(state, PeerState::Failed, "b: connection to a failed");
                b_up = state == PeerState::Connected;
            }
            std::thread::sleep(POLL);
        }
        true
    }

    fn disconnect(&mut self) {
        self.a.disconnect_peer(B);
        self.b.disconnect_peer(A);
    }
}

fn skip_inconclusive() {
    println!("SKIPPED: inconclusive (no ICE connection)");
}

/// `bursts` bursts of `burst_len` reliable packets from `a` to `b`, each packet carrying its burst
/// and its index within it. Every burst must arrive complete, in order, and before the next.
fn reliable_bursts_arrive_in_order(burst_len: u32, bursts: u32) {
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let mut pair = Pair::new(runtime.handle());
    if !pair.connect() {
        skip_inconclusive();
        return;
    }

    let mut received_total = 0u64;
    for burst in 0..bursts {
        // One frame's worth of sends: no yielding between them.
        for index in 0..burst_len {
            let mut payload = Vec::with_capacity(8);
            payload.extend_from_slice(&burst.to_le_bytes());
            payload.extend_from_slice(&index.to_le_bytes());
            pair.a.send(payload.into_boxed_slice(), B);
        }

        let mut arrived: Vec<(u32, u32)> = Vec::with_capacity(burst_len as usize);
        let deadline = Instant::now() + Duration::from_secs(5);
        while arrived.len() < burst_len as usize {
            assert!(
                Instant::now() < deadline,
                "burst {burst}: only {} of {burst_len} packets arrived within 5 s: {arrived:?}",
                arrived.len()
            );
            for (peer, bytes, _received_at) in pair.b.receive() {
                assert_eq!(peer, A);
                assert_eq!(bytes.len(), 8, "burst {burst}: a packet of the wrong size arrived");
                let got_burst = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
                let got_index = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
                arrived.push((got_burst, got_index));
            }
            std::thread::sleep(POLL);
        }

        let expected: Vec<(u32, u32)> = (0..burst_len).map(|index| (burst, index)).collect();
        assert_eq!(
            arrived, expected,
            "burst {burst} of {burst_len} arrived out of order (or mixed with another burst)"
        );
        received_total += burst_len as u64;
    }
    assert_eq!(received_total, u64::from(burst_len) * u64::from(bursts));

    pair.disconnect();
}

#[test]
fn a_burst_of_nine_reliable_sends_arrives_in_order() {
    reliable_bursts_arrive_in_order(9, 200);
}

#[test]
fn a_burst_of_thirty_two_reliable_sends_arrives_in_order() {
    reliable_bursts_arrive_in_order(32, 200);
}

/// A connection that is closed has to take its tasks with it. webrtc-rs spawns a good few per
/// peer connection (ICE agent, DTLS, SCTP, and the crate's own two workers), and leaves them
/// running unless the connection is explicitly closed — twenty lobbies joined and left used to
/// mean twenty sets of them still alive.
#[test]
fn twenty_connect_cycles_never_leak_a_task() {
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let alive = || runtime.handle().metrics().num_alive_tasks();

    let mut pair = Pair::new(runtime.handle());
    let idle = alive();

    // Wait for the task count to settle back near `idle`. Closing is asynchronous, and a few
    // internal tasks end a little after `close()` returns.
    let settled = |bound: usize| -> usize {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let now = alive();
            if now <= bound || Instant::now() > deadline {
                return now;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    for cycle in 0..20 {
        if !pair.connect() {
            skip_inconclusive();
            return;
        }
        let while_connected = alive();
        assert!(
            while_connected > idle,
            "cycle {cycle}: a connected pair runs no tasks at all?"
        );
        pair.disconnect();
        // Whatever the transport keeps around while a closed connection winds down, it must not
        // grow with the number of connections that have come and gone. Two spare tasks is the
        // slack for a close that is still in flight.
        let after = settled(idle + 2);
        assert!(
            after <= idle + 2,
            "cycle {cycle}: {after} tasks alive after disconnect, {idle} before any connection \
             ({while_connected} while connected) -- the connection leaked"
        );
    }
}
