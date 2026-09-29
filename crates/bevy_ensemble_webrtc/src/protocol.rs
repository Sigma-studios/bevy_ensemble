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
    ///
    /// Kept for clients older than [`CAPABILITY_TYPED_REQUESTS`]; this crate's client sends
    /// [`CreateLobbyRequest`](ClientMessage::CreateLobbyRequest). The same holds for the two joins
    /// below and for [`Signal`](ClientMessage::Signal).
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
    /// Which of the `CAPABILITY_*` bits this client understands, sent right after `Authenticate`.
    ///
    /// The server sends a message added after a client was built only to a client that declared
    /// the capability it belongs to: an older client could not decode it. A server older than
    /// this variant cannot decode the declaration either, logs it, and carries on without it —
    /// which is exactly the behaviour the client gets from a server that has no capabilities.
    DeclareCapabilities { capabilities: u64 },
    /// End the lobby this connection hosts, for everyone in it: nobody takes it over.
    ///
    /// Leaving a lobby that can migrate hands it to another member; this is the host deciding
    /// the session is over. Ignored from anyone but the host.
    CloseLobby,
    /// Which game this client is, sent right after `Authenticate`.
    ///
    /// One signalling server carries several games, and a lobby is only any use to a client of
    /// the game that hosts it: a lobby from another game is listed as joinable and then fails at
    /// the handshake. The server stamps each lobby with its host's game, lists only the lobbies of
    /// the asker's own, and answers a join into another game's lobby as `Lobby not found`.
    ///
    /// A client that never sends this is the empty game, which is every build older than the
    /// variant: they keep seeing each other and nothing else. A server older than the variant
    /// cannot decode it, logs it, and lists everything, as it always did. Longer than
    /// [`MAX_GAME_LEN`] bytes is cut to it.
    DeclareGame { game: String },
    /// Create a lobby, answered by [`ServerMessage::LobbyCreatedFor`] or
    /// [`ServerMessage::Refused`] carrying the same `request`.
    ///
    /// The typed successor of [`CreateLobby`](ClientMessage::CreateLobby). The id is what lets a
    /// client tell which of its attempts an answer belongs to: the old replies named none, so a
    /// client that had given up on one attempt and started another could not tell whose answer
    /// had arrived, and a refusal of anything at all read as "your join failed".
    CreateLobbyRequest {
        request: RequestId,
        max_players: u32,
    },
    /// Join a lobby by id. The typed successor of [`JoinLobby`](ClientMessage::JoinLobby),
    /// answered by [`ServerMessage::LobbyJoinedFor`] or [`ServerMessage::Refused`].
    JoinLobbyRequest { request: RequestId, lobby_id: u64 },
    /// Join a lobby by code. The typed successor of
    /// [`JoinLobbyByCode`](ClientMessage::JoinLobbyByCode), answered by
    /// [`ServerMessage::LobbyJoinedFor`] or [`ServerMessage::Refused`]. The server normalises the
    /// code with [`normalize_lobby_code`].
    JoinLobbyByCodeRequest { request: RequestId, code: String },
    /// Take back `request`: if this connection is in a lobby because of it, leave that lobby.
    ///
    /// Otherwise nothing happens, and that is the point of naming the request rather than sending
    /// [`LeaveLobby`](ClientMessage::LeaveLobby). A player who presses Host and then Cancel before
    /// the answer is back has a lobby on the server that nothing on their side knows about, and
    /// every later attempt was refused "Already in a lobby" until the socket was rebuilt. The
    /// client cancels such a request when it gives up on it, and again when an answer arrives
    /// that nothing is waiting for. Both can be in flight while a *newer* request has already
    /// put the connection in another lobby, and a bare leave would take that one down too.
    CancelRequest { request: RequestId },
    /// Every WebRTC signal for one peer that this client produced in one frame.
    ///
    /// The batched successor of [`Signal`](ClientMessage::Signal). A connection trickles its ICE
    /// candidates one at a time, and a host opening connections to seven members at once — seven
    /// joining together, or the successor after a host migration — sent each as its own frame,
    /// which ran through the server's per-connection message budget and had the rest refused.
    /// `request` is only there so a frame refused for being over the signalling budget can be
    /// named in the refusal and sent again.
    Signals {
        request: RequestId,
        receiver_uuid: u128,
        signals: Vec<String>,
    },
}

/// A client-chosen number that ties a server's answer to the request it answers.
///
/// Unique per connection, not globally: the server only ever echoes it back to the connection it
/// came from.
pub type RequestId = u32;

/// Why the signalling server refused something.
///
/// The typed successor of the free-text [`ServerMessage::LobbyError`], whose reason a client
/// could only compare against strings — and did not: it treated every one of them, a refused
/// keep-alive included, as its join having failed. Each variant is one of the reasons the server
/// actually gives; [`legacy_reason`](SignallingError::legacy_reason) is the text a client that
/// predates this type is still sent.
///
/// **Append only**, for the same reason as the message enums.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SignallingError {
    /// Over one of this connection's rate limits. The request was dropped unread and can be sent
    /// again once the budget refills; nothing else about the connection changed.
    RateLimited,
    /// Sent before `Authenticate`.
    NotAuthenticated,
    /// `Authenticate` sent twice on one connection.
    AlreadyAuthenticated,
    /// A create or join from a connection that is already in a lobby.
    AlreadyInLobby,
    /// No lobby with that id or code, or one that belongs to another game.
    LobbyNotFound,
    /// The lobby has as many members as it was created for.
    LobbyFull,
}

impl SignallingError {
    /// The text the server sent for this before the type existed, and still sends a client that
    /// did not declare [`CAPABILITY_TYPED_REQUESTS`].
    pub fn legacy_reason(self) -> &'static str {
        match self {
            Self::RateLimited => "rate limited",
            Self::NotAuthenticated => "Not authenticated",
            Self::AlreadyAuthenticated => "Already authenticated",
            Self::AlreadyInLobby => "Already in a lobby",
            Self::LobbyNotFound => "Lobby not found",
            Self::LobbyFull => "Lobby is full",
        }
    }
}

/// Words a game can put in front of a player.
impl std::fmt::Display for SignallingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RateLimited => "Too many requests at once. Wait a moment and try again.",
            Self::NotAuthenticated => "Not connected to the lobby server yet.",
            Self::AlreadyAuthenticated => "Already connected to the lobby server.",
            Self::AlreadyInLobby => "Already in a lobby. Leave it first.",
            Self::LobbyNotFound => "No lobby with that code.",
            Self::LobbyFull => "That lobby is full.",
        })
    }
}

impl std::error::Error for SignallingError {}

/// A lobby code as the server stores it: whitespace removed, upper case.
///
/// Codes are four letters from `A` to `Z`, read aloud, typed on phones and pasted from links, so
/// what arrives is `abcd`, ` ABCD\n` or `ab cd` as often as `ABCD`. The client applies this to
/// every `JoinWebrtcLobbyByCode` whatever wrote it, and the server applies it again to
/// whatever reaches it, so no entry path can miss it. Anything else — a digit, a dash — is left
/// in: it cannot match a code, and dropping it could turn a typo into somebody else's lobby.
pub fn normalize_lobby_code(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect()
}

/// The longest game name the server keeps, in bytes. A name is an identifier, not prose.
pub const MAX_GAME_LEN: usize = 64;

/// A client that understands [`ServerMessage::LobbyMigratable`] and [`ServerMessage::HostChanged`]:
/// a lobby it hosts outlives it, and a lobby it is in can hand it the host role.
pub const CAPABILITY_HOST_MIGRATION: u64 = 1 << 0;

/// Request ids and typed refusals: [`ClientMessage::CreateLobbyRequest`] and its siblings,
/// [`ClientMessage::CancelRequest`], batched [`Signals`](ClientMessage::Signals), and on the
/// server's side [`ServerMessage::Refused`], [`ServerMessage::LobbyCreatedFor`],
/// [`ServerMessage::LobbyJoinedFor`] and batched [`Signals`](ServerMessage::Signals).
///
/// The second revision of this protocol, and the first a client cannot do without: it sends its
/// lobby requests only in the typed form, which a server older than this cannot decode. So the
/// negotiation goes both ways. A client declares the bit, and a server that has it answers with
/// [`ServerMessage::ServerHello`] before anything else it says on that connection. A client that
/// hears the server's reply to its first `ListLobbies` without having heard a hello first is
/// talking to an older server, and says so — loudly, and to the player, rather than waiting out a
/// join that the server dropped as undecodable. Deploy the server before the clients.
///
/// Older clients are not affected: a server only sends any of the new messages to a connection
/// that declared the bit.
pub const CAPABILITY_TYPED_REQUESTS: u64 = 1 << 1;

/// Every capability this build of the protocol has, as a client declares it and a server
/// announces it.
pub const CAPABILITIES: u64 = CAPABILITY_HOST_MIGRATION | CAPABILITY_TYPED_REQUESTS;

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
    /// A lobby operation failed. Sent only to a client that did not declare
    /// [`CAPABILITY_TYPED_REQUESTS`]; any other is sent [`Refused`](ServerMessage::Refused).
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
    /// The lobby you created or joined survives its host: when the host leaves, another member
    /// takes it over and you are told so with [`HostChanged`](ServerMessage::HostChanged), rather
    /// than being removed with `Disconnected`.
    ///
    /// Sent only to a connection that declared [`CAPABILITY_HOST_MIGRATION`], right after
    /// `LobbyCreated` or `LobbyJoined`, and only for a lobby whose host declared it too.
    /// `idle_timeout_secs` is how long this server waits on a silent connection before it counts
    /// it gone, which bounds how long a member can wait to hear who the new host is.
    LobbyMigratable {
        lobby_id: u64,
        idle_timeout_secs: u32,
    },
    /// The host left and `new_host` hosts the lobby now, under the same id and code.
    ///
    /// `members` is everyone still in the lobby, the new host and the recipient included, in the
    /// order they joined. Anyone missing from it is no longer in the lobby, whether or not a
    /// `PlayerLeft` about them has arrived. Sent only in a lobby that was
    /// [`LobbyMigratable`](ServerMessage::LobbyMigratable).
    HostChanged {
        lobby_id: u64,
        previous_host: u128,
        new_host: u128,
        code: String,
        members: Vec<u128>,
    },
    /// What this server understands, sent in answer to a
    /// [`DeclareCapabilities`](ClientMessage::DeclareCapabilities) that includes
    /// [`CAPABILITY_TYPED_REQUESTS`]. See there for what its absence means.
    ServerHello { capabilities: u64 },
    /// A request, or a frame, was refused.
    ///
    /// `request` names the request it answers; `None` is a refusal of something that carried no
    /// id — a `ListLobbies` over the message budget, say. Neither kind ends anything but the
    /// request it names: a refusal is never a statement about a lobby this connection is already
    /// in.
    Refused {
        request: Option<RequestId>,
        error: SignallingError,
    },
    /// [`LobbyCreated`](ServerMessage::LobbyCreated), answering the request that asked for it.
    LobbyCreatedFor {
        request: RequestId,
        lobby_id: u64,
        code: String,
    },
    /// [`LobbyJoined`](ServerMessage::LobbyJoined), answering the request that asked for it, and
    /// with the lobby's code: a joiner that came in by id, or through a link, has as much use for
    /// the code to pass on as the host does.
    LobbyJoinedFor {
        request: RequestId,
        lobby_id: u64,
        host_uuid: u128,
        existing_members: Vec<u128>,
        code: String,
    },
    /// Relayed [`ClientMessage::Signals`]: every signal `sender_uuid` sent this client in one
    /// frame, in order. Sent only to a client that declared [`CAPABILITY_TYPED_REQUESTS`]; any
    /// other gets each as its own [`Signal`](ServerMessage::Signal).
    Signals {
        sender_uuid: u128,
        signals: Vec<String>,
    },
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
