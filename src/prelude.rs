//! Convenience re-exports for common `bevy_ensemble` usage.
//!
//! ```rust,ignore
//! use bevy_ensemble::prelude::*;
//! ```
//!
//! This includes everything most game code needs: the plugin, components for
//! querying lobbies and participants, message types for sending and receiving,
//! the registration trait, and core identity types.

pub use crate::{
    // Plugin
    EnsemblePlugin,

    // Identity
    LocalMultiplayerPlayerId, PlayerUUID,

    // Lobby components
    Host, Lobby, LobbyClient, PendingLobby,

    // Participant components
    LobbyParticipant, LobbyParticipantOf, LobbyParticipants,

    // Lobby discovery
    PublicLobbies, PublicLobbyInfo,

    // Ownership
    PlayerOwned, PlayerOwnedEntities,

    // Ping
    PeerRtt,

    // Messages & events
    EnsembleAppExt, LobbyMessage, ReceivedEnsembleMessage, StartHosting,
};
