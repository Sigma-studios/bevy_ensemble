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
    // Broadcast
    BroadcastLobbyMessage,
    // Messages & events
    EnsembleAppExt,
    // Plugin
    EnsemblePlugin,
    // Transport
    EnsembleTransportAppExt,
    // Lobby components
    Host,
    // Session requests
    JoinLobby,
    LeaveLobby,
    Lobby,
    LobbyBroadcastAppExt,

    LobbyBroadcastPlugin,

    LobbyClient,
    LobbyJoinFailed,
    LobbyMessage,
    // Participant components
    LobbyParticipant,
    LobbyParticipantOf,
    LobbyParticipants,

    // Identity
    LocalMultiplayerPlayerId,
    // Ping
    PeerLastPong,
    PeerRoute,
    PeerRtt,
    PeerRttJitter,
    PeerWireRtt,

    PendingLobby,

    // Player data
    PlayerData,
    PlayerDataPlugin,
    // Ownership
    PlayerOwned,
    PlayerOwnedEntities,

    PlayerUUID,

    // Lobby discovery
    PublicLobbies,
    PublicLobbyInfo,

    ReceivedEnsembleMessage,
    RefreshLobbies,
    SendMode,
    SetPlayerData,

    StartHosting,

    TransportBackend,
};

// Network metrics (feature `netmetrics`).
#[cfg(feature = "netmetrics")]
pub use crate::{NetMetrics, NetMetricsPlugin};

// Interactive network debug overlay + condition simulator (feature `netdebug`).
#[cfg(feature = "netdebug")]
pub use crate::{
    ChannelModel, NetDebugConfig, NetDebugExtras, NetDebugPlugin, NetPreset, NetSim, NetSimClock,
    NetSimConfig, NetSimPlugin,
};
