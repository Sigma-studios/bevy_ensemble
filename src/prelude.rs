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
    EnsemblePlugin, LobbyBroadcastPlugin,

    // Broadcast
    BroadcastLobbyMessage, LobbyBroadcastAppExt,

    // Player data
    PlayerData, PlayerDataPlugin, SetPlayerData,

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
    PeerLastPong, PeerRtt, PeerRttJitter, PeerWireRtt,

    // Messages & events
    EnsembleAppExt, LobbyMessage, ReceivedEnsembleMessage, SendMode, StartHosting,

    // Transport
    EnsembleTransportAppExt, TransportBackend,

    // Session requests
    JoinLobby, LeaveLobby, RefreshLobbies,
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
