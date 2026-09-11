//! One packet per peer per channel per frame.
//!
//! Every message used to become its own datagram the moment it was encoded. On WebRTC a
//! datagram carries roughly seventy bytes of UDP, DTLS and SCTP headers, so a sixty-byte input
//! packet was more header than payload, and a frame that sent a snapshot, a ping and a roster
//! change to one peer sent three datagrams where one would do.
//!
//! Now an encoded message is appended to [`OutboundBatches`] under its destination entity and
//! channel, and [`flush_outbound`] — in [`Last`], after everything that sends has run — triggers
//! one [`SerializedLobbyPacket`] per batch, framed by [`frame_packets`]. A batch of one message
//! goes unframed, so a lone message costs exactly what it did before.
//!
//! Three rules keep the semantics the sender asked for:
//!
//! - Reliable and unreliable messages never share a frame: a frame is one datagram, and one
//!   datagram has one delivery guarantee.
//! - [`SendMode::ReliableNoDelay`] is flushed as its own frame. It exists so a transport does
//!   not hold the message to pack it with the next one; holding it here to pack it with the
//!   previous ones would be the same mistake from the other side.
//! - A frame never exceeds [`MAX_DATAGRAM_BYTES`]. A batch is cut when the next message would
//!   push it over; a single message over the limit goes alone with a warning, because the
//!   transport is going to refuse or fragment it and somebody should know.
//!
//! An unreliable frame over [`UNRELIABLE_ADVISORY_BYTES`] is logged once per session: an
//! unreliable datagram past the path's fragment size is lost whole if any fragment is lost,
//! and a game sending 4 KB snapshots unreliably deserves one line telling it so.

use bevy::prelude::*;

use crate::{SendMode, SerializedLobbyPacket, registry::frame_packets};

/// The largest frame the core will build. Above this the SCTP transport in a browser refuses
/// the message outright, and native transports fragment it into something a single loss kills.
pub const MAX_DATAGRAM_BYTES: usize = 60_000;

/// Past this size an unreliable frame is likely to be fragmented on the path, and a fragmented
/// unreliable datagram is lost whole if any fragment is. Advisory: logged once.
pub const UNRELIABLE_ADVISORY_BYTES: usize = 1200;

/// Encoded messages waiting for the end of the frame, in the order they were encoded.
#[derive(Resource, Default)]
pub struct OutboundBatches {
    batches: Vec<Batch>,
}

struct Batch {
    entity: Entity,
    send_mode: SendMode,
    packets: Vec<Vec<u8>>,
    bytes: usize,
}

impl OutboundBatches {
    /// Queue one encoded message for `entity` on `send_mode`'s channel.
    pub fn push(&mut self, entity: Entity, send_mode: SendMode, packet: Vec<u8>) {
        let no_delay = matches!(send_mode, SendMode::ReliableNoDelay);
        let fits = |batch: &Batch| {
            batch.entity == entity
                && batch.send_mode == send_mode
                && !no_delay
                && batch.bytes + packet.len() + 8 <= MAX_DATAGRAM_BYTES
        };
        if let Some(batch) = self.batches.iter_mut().rev().find(|batch| fits(batch)) {
            batch.bytes += packet.len();
            batch.packets.push(packet);
            return;
        }
        self.batches.push(Batch {
            entity,
            send_mode,
            bytes: packet.len(),
            packets: vec![packet],
        });
    }

    /// Messages queued and not yet flushed.
    pub fn pending_messages(&self) -> usize {
        self.batches.iter().map(|batch| batch.packets.len()).sum()
    }

    fn take(&mut self) -> Vec<Batch> {
        std::mem::take(&mut self.batches)
    }
}

/// Turn every batch into one [`SerializedLobbyPacket`]. Runs in [`Last`].
pub(crate) fn flush_outbound(
    mut batches: ResMut<OutboundBatches>,
    mut commands: Commands,
    mut advised: Local<bool>,
) {
    for batch in batches.take() {
        let packet = frame_packets(&batch.packets);
        if packet.len() > MAX_DATAGRAM_BYTES {
            warn!(
                "sending a {}-byte {:?} message to {:?}: over the {MAX_DATAGRAM_BYTES}-byte \
                 datagram limit, and the transport may refuse or fragment it",
                packet.len(),
                batch.send_mode,
                batch.entity
            );
        } else if !batch.send_mode.is_reliable()
            && packet.len() > UNRELIABLE_ADVISORY_BYTES
            && !*advised
        {
            *advised = true;
            warn!(
                "an unreliable frame of {} bytes ({} messages) is past the {}-byte advisory \
                 size: past the path's fragment size an unreliable datagram is lost whole if any \
                 fragment is. Said once per session.",
                packet.len(),
                batch.packets.len(),
                UNRELIABLE_ADVISORY_BYTES
            );
        }
        let send_mode = batch.send_mode;
        let entity = batch.entity;
        // Triggered on the world rather than through `commands.entity(..)`, because the entity
        // may already be gone: the notification to a kicked client is addressed to the
        // `LobbyClient` entity whose despawn produced it. A backend that can still reach the
        // peer (the loopback remembers recent clients) delivers it; one that resolves the entity
        // finds nothing and drops it, which is what the transport would have done anyway.
        commands.queue(move |world: &mut World| {
            world.trigger(SerializedLobbyPacket {
                entity,
                packet,
                send_mode,
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(index: u32) -> Entity {
        Entity::from_raw_u32(index).unwrap()
    }

    #[test]
    fn messages_to_one_peer_on_one_channel_share_a_batch() {
        let mut batches = OutboundBatches::default();
        batches.push(entity(1), SendMode::Reliable, vec![1, 0]);
        batches.push(entity(1), SendMode::Reliable, vec![2, 0]);
        batches.push(entity(2), SendMode::Reliable, vec![3, 0]);
        let taken = batches.take();
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].packets.len(), 2);
        assert_eq!(taken[1].packets.len(), 1);
    }

    #[test]
    fn reliable_and_unreliable_never_share() {
        let mut batches = OutboundBatches::default();
        batches.push(entity(1), SendMode::Reliable, vec![1, 0]);
        batches.push(entity(1), SendMode::Unreliable, vec![2, 0]);
        batches.push(entity(1), SendMode::Reliable, vec![3, 0]);
        let taken = batches.take();
        assert_eq!(taken.len(), 2, "reliable messages rejoin their batch; unreliable has its own");
        assert_eq!(taken[0].packets.len(), 2);
    }

    #[test]
    fn no_delay_goes_alone() {
        let mut batches = OutboundBatches::default();
        batches.push(entity(1), SendMode::ReliableNoDelay, vec![1, 0]);
        batches.push(entity(1), SendMode::ReliableNoDelay, vec![2, 0]);
        assert_eq!(batches.take().len(), 2);
    }

    #[test]
    fn a_batch_is_cut_at_the_datagram_limit() {
        let mut batches = OutboundBatches::default();
        let big = vec![0u8; MAX_DATAGRAM_BYTES / 2];
        batches.push(entity(1), SendMode::Reliable, big.clone());
        batches.push(entity(1), SendMode::Reliable, big.clone());
        batches.push(entity(1), SendMode::Reliable, big);
        let taken = batches.take();
        assert_eq!(taken.len(), 3, "two halves plus framing exceed the limit, so each goes alone");
        assert!(taken.iter().all(|b| b.bytes <= MAX_DATAGRAM_BYTES));
    }
}
