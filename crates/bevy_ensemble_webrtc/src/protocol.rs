use serde::{Deserialize, Serialize};

/// Messages sent from client to the signaling server.
///
/// **Append only.** Postcard encodes an enum by variant index, so inserting a variant renumbers
/// every one after it, and a server built from a different commit then reads `JoinLobby` as
/// `LeaveLobby` with no error of any kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// First message after WebSocket connect. Server responds with `Welcome`.
    Authenticate { display_name: String },
    /// Create a new lobby. Server responds with `LobbyCreated` or `LobbyError`.
    CreateLobby { max_players: u32 },
    /// Join an existing lobby by ID. Server responds with `LobbyJoined` or `LobbyError`.
    JoinLobby { lobby_id: u64 },
    /// Join an existing lobby by its short code. Server responds with `LobbyJoined` or `LobbyError`.
    JoinLobbyByCode { code: String },
    /// Leave the current lobby.
    LeaveLobby,
    /// Request the list of available lobbies. Server responds with `LobbyList`.
    ListLobbies,
    /// WebRTC signaling data relayed to a specific peer.
    Signal { receiver_uuid: u128, data: String },
    /// Keep-alive heartbeat.
    KeepAlive,
    /// Change the name this connection is known by, after `Authenticate`.
    ///
    /// The name given at authentication is fixed when the socket is built, which for most
    /// consumers is process start — so a name typed into a menu could never reach the lobby list,
    /// and a host was advertised under whatever `--name` said at launch. This is what makes it
    /// live.
    ///
    /// Also updates the lobby listing when this connection hosts a lobby: `host_name` is copied
    /// at creation and would otherwise keep the name the host had then.
    SetDisplayName { display_name: String },
}

/// Messages sent from the signaling server to a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Response to `Authenticate`. Assigns a unique player UUID.
    Welcome { player_uuid: u128 },
    /// Lobby was successfully created. The caller is the host.
    LobbyCreated { lobby_id: u64, code: String },
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
    pub code: String,
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
