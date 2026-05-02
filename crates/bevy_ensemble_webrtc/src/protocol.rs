use serde::{Deserialize, Serialize};

/// Messages sent from client to the signaling server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// First message after WebSocket connect. Server responds with `Welcome`.
    Authenticate { display_name: String },
    /// Create a new lobby. Server responds with `LobbyCreated` or `LobbyError`.
    CreateLobby { max_players: u32 },
    /// Join an existing lobby by ID. Server responds with `LobbyJoined` or `LobbyError`.
    JoinLobby { lobby_id: u64 },
    /// Leave the current lobby.
    LeaveLobby,
    /// Request the list of available lobbies. Server responds with `LobbyList`.
    ListLobbies,
    /// WebRTC signaling data relayed to a specific peer.
    Signal { receiver_uuid: u128, data: String },
    /// Keep-alive heartbeat.
    KeepAlive,
}

/// Messages sent from the signaling server to a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Response to `Authenticate`. Assigns a unique player UUID.
    Welcome { player_uuid: u128 },
    /// Lobby was successfully created. The caller is the host.
    LobbyCreated { lobby_id: u64 },
    /// Successfully joined a lobby.
    LobbyJoined {
        lobby_id: u64,
        host_uuid: u128,
        existing_members: Vec<u128>,
    },
    /// A lobby operation failed.
    LobbyError { reason: String },
    /// A new player joined the lobby you are in.
    PlayerJoined { player_uuid: u128 },
    /// A player left the lobby you are in.
    PlayerLeft { player_uuid: u128 },
    /// Response to `ListLobbies`.
    LobbyList { lobbies: Vec<LobbyInfo> },
    /// You were removed from the lobby (host left, lobby destroyed, etc.).
    Disconnected { reason: String },
    /// Relayed WebRTC signaling data from another peer.
    Signal { sender_uuid: u128, data: String },
}

/// Summary information about a lobby, returned in lobby listings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyInfo {
    pub lobby_id: u64,
    pub host_name: String,
    pub player_count: u32,
    pub max_players: u32,
}

/// Serialize a message to postcard bytes.
/// Returns `None` if serialization fails.
pub fn encode<T: Serialize>(msg: &T) -> Option<Vec<u8>> {
    postcard::to_allocvec(msg).ok()
}

/// Deserialize a message from postcard bytes.
pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}
