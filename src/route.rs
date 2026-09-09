//! How a peer is actually reached, once ICE has chosen.

use bevy::prelude::*;

/// The kind of path a peer's traffic takes.
///
/// Set by whichever transport knows — `bevy_ensemble_webrtc` reads it off the nominated ICE
/// candidate pair — and added to the same entities as [`PeerRtt`](crate::PeerRtt): `LobbyClient`
/// entities on a host, the lobby entity on a client.
///
/// # Why this is worth carrying
///
/// A build configured with a relay still connects directly whenever it can, so "a relay was
/// offered" and "a relay is being used" are different facts and only the second costs anything.
/// Without this, the two are indistinguishable from inside the game: a relayed session looks
/// exactly like a direct one that happens to have a worse ping, and the first question about a
/// player reporting that the game feels sluggish has no answer.
///
/// Absent means ICE has not settled yet, or the transport does not report it.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRoute {
    /// A direct pair — host or server-reflexive at both ends. Nothing in the middle.
    Direct,
    /// Through a TURN relay, because no direct pair worked. Costs the relay's round trip, and is
    /// the difference between a slower session and no session at all.
    Relayed,
}

impl PeerRoute {
    /// For a readout with one column to spend on this.
    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relayed => "relayed",
        }
    }
}
