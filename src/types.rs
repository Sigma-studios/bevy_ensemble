/// A unique identifier for a player across the network.
///
/// Each player in a lobby is assigned a `PlayerUUID` that remains stable for the
/// duration of their session. Platform backends are responsible for mapping their
/// native player identifiers (e.g. Steam IDs) to this type.
///
/// The special value [`LOCAL_PLAYER_UUID`] is used as a placeholder before the
/// platform backend assigns a real identity.
pub type PlayerUUID = u128;

/// Placeholder identity used when no platform backend has assigned a real player UUID.
///
/// This value is set on the host's [`LocalMultiplayerPlayerId`](crate::LocalMultiplayerPlayerId)
/// resource when [`StartHosting`](crate::StartHosting) is processed. The platform backend
/// is expected to overwrite it with the real player identity once the lobby is created.
pub const LOCAL_PLAYER_UUID: PlayerUUID = 0;
