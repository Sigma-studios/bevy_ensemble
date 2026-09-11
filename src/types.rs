/// A unique identifier for a player across the network.
///
/// Each player in a lobby is assigned a `PlayerUUID` that remains stable for the
/// duration of their session. Platform backends are responsible for mapping their
/// native player identifiers (e.g. Steam IDs) to this type.
///
/// There is no placeholder value. A peer that does not yet know its identity has no
/// [`LocalMultiplayerPlayerId`](crate::LocalMultiplayerPlayerId) resource at all, so code that
/// needs one waits for it instead of adopting a stand-in — which is what a host once did, and
/// took the host role under a uuid of zero.
pub type PlayerUUID = u128;
