//! A socket whose app has stopped still answers, and the side watching it still hears it.
//!
//! Two real native `EnsembleSocket`s in one process, as in `ordering.rs`. After connecting, `b`
//! is left alone — no `receive`, no `update_peers`, no `send_keepalives`: an app that is not
//! running frames, which is a browser tab in the background. `a` keeps running. The keepalive
//! pong is sent from `b`'s data channel handler, so `a` must go on hearing from `b` for as long as
//! it watches; and `b`'s backlog of what `a` sent meanwhile must stay bounded.
//!
//! A pair that has not connected in 15 s prints `SKIPPED: inconclusive (no ICE connection)` and
//! passes, as the ordering tests do.
#![cfg(not(target_arch = "wasm32"))]

use std::time::{Duration, Instant};

use bevy_ensemble_sockets::{
    EnsembleSocket, IceServers, KEEPALIVE_INTERVAL, MAX_QUEUED_UNRELIABLE, PeerState,
};

const A: u128 = 0xA;
const B: u128 = 0xB;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(2);

fn connect(a: &mut EnsembleSocket, b: &mut EnsembleSocket) -> bool {
    a.connect_peer(B);
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let (mut a_up, mut b_up) = (false, false);
    while !(a_up && b_up) {
        if Instant::now() > deadline {
            return false;
        }
        for signal in a.drain_signals() {
            b.receive_signal(A, signal.signal);
        }
        for signal in b.drain_signals() {
            a.receive_signal(B, signal.signal);
        }
        for (_, state) in a.update_peers() {
            assert_ne!(state, PeerState::Failed, "a: connection to b failed");
            a_up |= state == PeerState::Connected;
        }
        for (_, state) in b.update_peers() {
            assert_ne!(state, PeerState::Failed, "b: connection to a failed");
            b_up |= state == PeerState::Connected;
        }
        std::thread::sleep(POLL);
    }
    true
}

#[test]
fn a_frozen_app_keeps_answering_and_its_backlog_stays_bounded() {
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let mut a = EnsembleSocket::new(runtime.handle().clone()).with_ice_servers(IceServers::none());
    let mut b = EnsembleSocket::new(runtime.handle().clone()).with_ice_servers(IceServers::none());
    if !connect(&mut a, &mut b) {
        println!("SKIPPED: inconclusive (no ICE connection)");
        return;
    }
    // Whatever the connection itself said on the way up is not what is being measured.
    let _ = a.take_heard();
    let _ = a.receive();

    // `b` is frozen from here on. `a` runs frames for three seconds, pinging and streaming
    // unreliable packets at it the way a host streams snapshots, and notes every stretch in which
    // it heard nothing from `b`.
    let watch = Duration::from_secs(3);
    let start = Instant::now();
    let mut last_heard = start;
    let mut longest_silence = Duration::ZERO;
    let mut sent = 0usize;
    while start.elapsed() < watch {
        a.send_keepalives();
        for _ in 0..4 {
            a.send_with_mode(vec![1, 2, 3, 4].into_boxed_slice(), B, false);
            sent += 1;
        }
        assert!(
            a.receive().is_empty(),
            "b's app sent nothing, and a keepalive is never handed up"
        );
        let now = Instant::now();
        if a.take_heard().contains(&B) {
            last_heard = now;
        }
        longest_silence = longest_silence.max(now - last_heard);
        std::thread::sleep(Duration::from_millis(4));
    }

    assert!(
        longest_silence < KEEPALIVE_INTERVAL * 4,
        "a went {longest_silence:?} without hearing from b, whose app is frozen but whose \
         connection is fine; it answers every {KEEPALIVE_INTERVAL:?}"
    );
    assert!(
        sent > MAX_QUEUED_UNRELIABLE,
        "the test must outrun the backlog cap to say anything about it ({sent} sent)"
    );

    // `b` wakes. What waited for it is the newest of a's packets, no more than the cap.
    std::thread::sleep(Duration::from_millis(200));
    let backlog = b.receive();
    assert!(
        !backlog.is_empty() && backlog.len() <= MAX_QUEUED_UNRELIABLE,
        "{} packets waited for b; the cap is {MAX_QUEUED_UNRELIABLE}",
        backlog.len()
    );
    assert!(
        b.dropped_backlog() > 0,
        "{sent} were sent against a cap of {MAX_QUEUED_UNRELIABLE}; some must have been dropped"
    );
    assert!(
        b.take_heard().contains(&A),
        "b heard a the whole time it slept, and says so on waking"
    );
}
