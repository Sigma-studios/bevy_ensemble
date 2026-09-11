use std::collections::HashSet;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::mpsc;

use crate::protocol::ServerMessage;

/// What one connection is allowed to do, and how long it may stay silent.
///
/// The defaults are what a deployment gets from [`ServerState::new`]. They are generous for a
/// game client — one that sends twenty signalling messages a second sustained is misbehaving —
/// and tight enough that a flood costs the flooder its connection rather than everyone their
/// server.
#[derive(Debug, Clone)]
pub struct Limits {
    /// A connection that sends nothing for this long is closed and its player leaves its lobby.
    /// `KeepAlive` counts as sending something; that is what it is for.
    pub idle_timeout: Duration,
    /// Sustained messages per second, over every kind of frame.
    pub messages_per_second: f64,
    /// How many messages may arrive at once before the sustained rate applies.
    pub message_burst: f64,
    /// Sustained rate for `CreateLobby` and `JoinLobbyByCode`, on top of the general one.
    ///
    /// A join by code is a guess at a four-letter code, and this is what makes guessing
    /// impractical; creating a lobby allocates server state, and this is what caps it.
    pub lobby_ops_per_second: f64,
    /// Burst for the lobby-operation limit. Two, so that a mistyped code retried straight away
    /// still gets a real answer.
    pub lobby_ops_burst: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(60),
            messages_per_second: 20.0,
            message_burst: 40.0,
            lobby_ops_per_second: 1.0,
            lobby_ops_burst: 2.0,
        }
    }
}

pub struct ServerState {
    pub lobbies: DashMap<u64, LobbyState>,
    pub lobby_codes: DashMap<String, u64>,
    pub connections: DashMap<u128, ConnectionHandle>,
    pub limits: Limits,
}

pub struct LobbyState {
    pub lobby_id: u64,
    pub code: String,
    pub host_uuid: u128,
    pub host_name: String,
    pub members: HashSet<u128>,
    pub max_players: u32,
}

pub struct ConnectionHandle {
    pub display_name: String,
    pub lobby_id: Option<u64>,
    pub sender: mpsc::UnboundedSender<ServerMessage>,
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerState {
    pub fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    pub fn with_limits(limits: Limits) -> Self {
        Self {
            lobbies: DashMap::new(),
            lobby_codes: DashMap::new(),
            connections: DashMap::new(),
            limits,
        }
    }

    /// A lobby id nothing currently uses.
    ///
    /// Random rather than sequential on purpose. Ids are handed to clients in `LobbyCreated` and
    /// accepted back in `JoinLobby`, so a counter starting at 1 made every lobby joinable by
    /// counting and the four-letter code decorative. A random `u64` cannot be enumerated.
    ///
    /// Only unused at the moment it is picked; the caller inserts through the map's entry API so
    /// a concurrent creation that lands on the same id is noticed rather than overwritten.
    pub fn next_lobby_id(&self) -> u64 {
        loop {
            let id: u64 = rand::random();
            // Zero is kept out so it can never be mistaken for "no lobby" by anything that
            // stores ids as plain integers.
            if id != 0 && !self.lobbies.contains_key(&id) {
                return id;
            }
        }
    }

    pub fn send_to(&self, player_uuid: u128, msg: ServerMessage) {
        if let Some(conn) = self.connections.get(&player_uuid) {
            let _ = conn.sender.send(msg);
        }
    }

    pub fn send_to_all_except(&self, members: &HashSet<u128>, except: u128, msg: ServerMessage) {
        for &uuid in members {
            if uuid != except {
                self.send_to(uuid, msg.clone());
            }
        }
    }
}
