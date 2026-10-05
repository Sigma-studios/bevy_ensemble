//! What arrives from peers, and the one thing the socket answers by itself.
//!
//! # Liveness belongs to the transport
//!
//! A peer used to be alive for as long as its *game* answered pings, and a game answers from its
//! frame loop. A browser stops that loop entirely for a tab in the background, and a native build
//! stalls it for a long shader compile or a load, so a player who looked away for five seconds
//! lost their seat over a connection that was perfectly healthy. Whether a connection is alive is
//! a question for the connection.
//!
//! So the socket keeps its own: a keepalive ping every [`KEEPALIVE_INTERVAL`], sent from the frame
//! loop by [`EnsembleSocket::send_keepalives`](crate::EnsembleSocket::send_keepalives), and the
//! pong for it sent straight from the data channel's message handler. That handler runs whether
//! or not the app is drawing frames — a browser still delivers network events to a hidden tab, and
//! natively it runs on the network runtime — so a peer whose game is frozen still answers, and
//! the side watching it still hears from it. Anything heard counts, not only pongs: the peers it
//! was heard from since the last look are what [`Inbox::take_heard`] returns.
//!
//! # On the wire
//!
//! A keepalive is six bytes: a reserved two-byte type index, then a sequence number. The indices
//! are the two below the ones `bevy_ensemble` reserves for frames and its handshake, and it
//! reserves these too, so no message ever travels under them and a game's packets go out exactly
//! as they did, with nothing added. An older build that receives one fails to decode it — and has
//! by then refused the session at the handshake, since the core's protocol version moved with
//! this.
//!
//! # The backlog
//!
//! A frozen app still has everything sent to it delivered into this queue, and it drains it only
//! when it wakes: at 64 snapshots a second, minutes away is a great deal of memory, all decoded on
//! the first frame back for the sake of the newest. Unreliable traffic may be dropped by contract,
//! so at most [`MAX_QUEUED_UNRELIABLE`] of a peer's unreliable packets wait here, and the oldest
//! goes when another arrives. Reliable packets are kept, in order, whatever their number.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use web_time::Instant;

/// The type index a keepalive ping travels under. Reserved by `bevy_ensemble`'s registry.
pub const KEEPALIVE_PING_INDEX: u16 = u16::MAX - 2;
/// The type index a keepalive pong travels under. Reserved by `bevy_ensemble`'s registry.
pub const KEEPALIVE_PONG_INDEX: u16 = u16::MAX - 3;

/// How often each peer is sent a keepalive ping.
///
/// Four a second: a liveness timeout is many seconds, so this is dozens of chances inside one,
/// and a ping is six bytes against a game stream already sending sixty-four packets a second.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(250);

/// How many of one peer's unreliable packets wait to be read before the oldest is dropped.
///
/// Eight seconds of a 64 Hz stream: far more than any frame hitch, and far less than a tab left in
/// the background for a few minutes.
pub const MAX_QUEUED_UNRELIABLE: usize = 512;

const KEEPALIVE_LEN: usize = 6;

/// A keepalive, if `bytes` is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keepalive {
    Ping(u32),
    Pong(u32),
}

impl Keepalive {
    pub(crate) fn encode(self) -> [u8; KEEPALIVE_LEN] {
        let (index, seq) = match self {
            Keepalive::Ping(seq) => (KEEPALIVE_PING_INDEX, seq),
            Keepalive::Pong(seq) => (KEEPALIVE_PONG_INDEX, seq),
        };
        let mut bytes = [0; KEEPALIVE_LEN];
        bytes[..2].copy_from_slice(&index.to_le_bytes());
        bytes[2..].copy_from_slice(&seq.to_le_bytes());
        bytes
    }

    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != KEEPALIVE_LEN {
            return None;
        }
        let index = u16::from_le_bytes([bytes[0], bytes[1]]);
        let seq = u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
        match index {
            KEEPALIVE_PING_INDEX => Some(Keepalive::Ping(seq)),
            KEEPALIVE_PONG_INDEX => Some(Keepalive::Pong(seq)),
            _ => None,
        }
    }
}

/// One packet waiting to be read.
struct Delivery {
    peer: u128,
    bytes: Box<[u8]>,
    at: Instant,
    reliable: bool,
}

#[derive(Default)]
struct InboxState {
    queue: VecDeque<Delivery>,
    /// How many of `queue` are each peer's unreliable packets.
    unreliable: HashMap<u128, usize>,
    /// Peers anything has been heard from since [`Inbox::take_heard`] last looked.
    heard: HashSet<u128>,
    /// Unreliable packets dropped to keep the backlog bounded, over the socket's lifetime.
    dropped: u64,
}

/// Everything every connection of one socket has received, shared with the handlers that receive
/// it. Cloning shares it.
#[derive(Clone, Default)]
pub(crate) struct Inbox(Arc<Mutex<InboxState>>);

impl Inbox {
    /// Bytes from `peer` came off a data channel at `at`. Returns the reply to send back on the
    /// same channel, if the bytes asked for one — which only a keepalive ping does.
    ///
    /// Called from the channel's message handler, which is the point: it runs when the app is
    /// not.
    pub(crate) fn arrive(
        &self,
        peer: u128,
        bytes: Box<[u8]>,
        at: Instant,
        reliable: bool,
    ) -> Option<[u8; KEEPALIVE_LEN]> {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        state.heard.insert(peer);
        match Keepalive::decode(&bytes) {
            Some(Keepalive::Ping(seq)) => return Some(Keepalive::Pong(seq).encode()),
            Some(Keepalive::Pong(_)) => return None,
            None => {}
        }
        if !reliable {
            let queued = state.unreliable.entry(peer).or_default();
            if *queued >= MAX_QUEUED_UNRELIABLE {
                // The oldest of this peer's unreliable packets: the one a newer one has replaced.
                if let Some(at) = state
                    .queue
                    .iter()
                    .position(|delivery| delivery.peer == peer && !delivery.reliable)
                {
                    state.queue.remove(at);
                    state.dropped += 1;
                }
            } else {
                *queued += 1;
            }
        }
        state.queue.push_back(Delivery {
            peer,
            bytes,
            at,
            reliable,
        });
        None
    }

    /// Everything waiting, oldest first.
    pub(crate) fn drain(&self) -> Vec<(u128, Box<[u8]>, Instant)> {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        state.unreliable.clear();
        state
            .queue
            .drain(..)
            .map(|delivery| (delivery.peer, delivery.bytes, delivery.at))
            .collect()
    }

    /// The peers heard from since the last call: anything at all, a keepalive included.
    pub(crate) fn take_heard(&self) -> Vec<u128> {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        state.heard.drain().collect()
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: u128 = 7;
    const OTHER: u128 = 8;

    fn data(byte: u8) -> Box<[u8]> {
        vec![byte, 0, byte].into_boxed_slice()
    }

    #[test]
    fn a_ping_is_answered_and_never_queued() {
        let inbox = Inbox::default();
        let reply = inbox.arrive(
            PEER,
            Keepalive::Ping(41).encode().into(),
            Instant::now(),
            false,
        );
        assert_eq!(reply, Some(Keepalive::Pong(41).encode()));
        assert!(inbox.drain().is_empty(), "a keepalive is not the app's");
        assert_eq!(inbox.take_heard(), vec![PEER], "but it is proof of life");
        assert!(inbox.take_heard().is_empty(), "once per look");
    }

    #[test]
    fn a_pong_counts_as_hearing_from_the_peer() {
        let inbox = Inbox::default();
        let reply = inbox.arrive(
            PEER,
            Keepalive::Pong(3).encode().into(),
            Instant::now(),
            false,
        );
        assert_eq!(reply, None);
        assert!(inbox.drain().is_empty());
        assert_eq!(inbox.take_heard(), vec![PEER]);
    }

    #[test]
    fn game_packets_go_through_untouched_and_in_order() {
        let inbox = Inbox::default();
        for byte in 0..5 {
            assert_eq!(
                inbox.arrive(PEER, data(byte), Instant::now(), byte % 2 == 0),
                None
            );
        }
        let bytes: Vec<Box<[u8]>> = inbox.drain().into_iter().map(|(_, b, _)| b).collect();
        assert_eq!(bytes, (0..5).map(data).collect::<Vec<_>>());
    }

    #[test]
    fn a_frozen_apps_unreliable_backlog_keeps_only_the_newest() {
        let inbox = Inbox::default();
        let now = Instant::now();
        inbox.arrive(PEER, data(255), now, true);
        for n in 0..(MAX_QUEUED_UNRELIABLE + 100) {
            inbox.arrive(PEER, vec![(n % 256) as u8; 4].into(), now, false);
        }
        inbox.arrive(OTHER, data(1), now, false);

        let drained = inbox.drain();
        assert_eq!(
            drained.len(),
            1 + MAX_QUEUED_UNRELIABLE + 1,
            "the reliable packet, the newest unreliable ones, and the other peer's"
        );
        assert_eq!(
            drained[0].1,
            data(255),
            "a reliable packet is never dropped"
        );
        assert_eq!(
            drained[1].1,
            vec![100u8; 4].into(),
            "the oldest hundred unreliable packets went"
        );
        assert_eq!(
            drained.last().unwrap().0,
            OTHER,
            "one peer's backlog is its own"
        );
        assert_eq!(inbox.dropped(), 100);

        // Drained, the count starts again.
        for _ in 0..MAX_QUEUED_UNRELIABLE {
            inbox.arrive(PEER, data(2), now, false);
        }
        assert_eq!(inbox.drain().len(), MAX_QUEUED_UNRELIABLE);
        assert_eq!(inbox.dropped(), 100);
    }

    #[test]
    fn only_six_bytes_under_a_reserved_index_are_a_keepalive() {
        let mut long = Keepalive::Ping(1).encode().to_vec();
        long.push(0);
        assert_eq!(Keepalive::decode(&long), None);
        assert_eq!(Keepalive::decode(&[0, 0, 1, 0, 0, 0]), None);
        assert_eq!(
            Keepalive::decode(&Keepalive::Pong(9).encode()),
            Some(Keepalive::Pong(9))
        );
    }
}
