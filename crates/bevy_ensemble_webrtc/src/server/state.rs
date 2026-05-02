use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use tokio::sync::mpsc;

use crate::protocol::ServerMessage;

pub struct ServerState {
    pub lobbies: DashMap<u64, LobbyState>,
    pub connections: DashMap<u128, ConnectionHandle>,
    next_lobby_id: AtomicU64,
}

pub struct LobbyState {
    pub lobby_id: u64,
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

impl ServerState {
    pub fn new() -> Self {
        Self {
            lobbies: DashMap::new(),
            connections: DashMap::new(),
            next_lobby_id: AtomicU64::new(1),
        }
    }

    pub fn next_lobby_id(&self) -> u64 {
        self.next_lobby_id.fetch_add(1, Ordering::Relaxed)
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
