//! The signalling protocol, against the build that shipped before host migration.
//!
//! The server is shared by games pinned to different commits, so a message has to mean the same
//! bytes to every one of them. Postcard encodes an enum as a variant index and the fields in order:
//! appending a variant changes nothing for the ones before it, and inserting one renumbers them all
//! with no error of any kind. The enums below are frozen copies of `protocol.rs` at `9f7f245`,
//! the last commit before any variant was appended. They are never edited.

use bevy_ensemble_webrtc::protocol::{
    CAPABILITY_HOST_MIGRATION, ClientMessage, LobbyInfo, ServerMessage, decode, encode,
};
use serde::{Deserialize, Serialize};

mod pre_e5 {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize)]
    pub enum ClientMessage {
        Authenticate { display_name: String },
        CreateLobby { max_players: u32 },
        JoinLobby { lobby_id: u64 },
        JoinLobbyByCode { code: String },
        LeaveLobby,
        ListLobbies,
        Signal { receiver_uuid: u128, data: String },
        KeepAlive,
        SetDisplayName { display_name: String },
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub enum ServerMessage {
        Welcome {
            player_uuid: u128,
        },
        LobbyCreated {
            lobby_id: u64,
            code: String,
        },
        LobbyJoined {
            lobby_id: u64,
            host_uuid: u128,
            existing_members: Vec<u128>,
        },
        LobbyError {
            reason: String,
        },
        PlayerJoined {
            player_uuid: u128,
        },
        PlayerLeft {
            player_uuid: u128,
        },
        LobbyList {
            lobbies: Vec<LobbyInfo>,
        },
        Disconnected {
            reason: String,
        },
        Signal {
            sender_uuid: u128,
            data: String,
        },
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct LobbyInfo {
        pub lobby_id: u64,
        pub code: String,
        pub host_name: String,
        pub player_count: u32,
        pub max_players: u32,
    }
}

fn bytes(message: &impl Serialize) -> Vec<u8> {
    encode(message).expect("encode")
}

fn decodes_as<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> bool {
    decode::<T>(bytes).is_ok()
}

const UUID: u128 = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef;

#[test]
fn every_pre_e5_client_message_encodes_to_the_same_bytes() {
    let name = || "name".to_owned();
    let pairs: [(Vec<u8>, Vec<u8>); 9] = [
        (
            bytes(&ClientMessage::Authenticate {
                display_name: name(),
            }),
            bytes(&pre_e5::ClientMessage::Authenticate {
                display_name: name(),
            }),
        ),
        (
            bytes(&ClientMessage::CreateLobby { max_players: 7 }),
            bytes(&pre_e5::ClientMessage::CreateLobby { max_players: 7 }),
        ),
        (
            bytes(&ClientMessage::JoinLobby { lobby_id: 99 }),
            bytes(&pre_e5::ClientMessage::JoinLobby { lobby_id: 99 }),
        ),
        (
            bytes(&ClientMessage::JoinLobbyByCode { code: name() }),
            bytes(&pre_e5::ClientMessage::JoinLobbyByCode { code: name() }),
        ),
        (
            bytes(&ClientMessage::LeaveLobby),
            bytes(&pre_e5::ClientMessage::LeaveLobby),
        ),
        (
            bytes(&ClientMessage::ListLobbies),
            bytes(&pre_e5::ClientMessage::ListLobbies),
        ),
        (
            bytes(&ClientMessage::Signal {
                receiver_uuid: UUID,
                data: name(),
            }),
            bytes(&pre_e5::ClientMessage::Signal {
                receiver_uuid: UUID,
                data: name(),
            }),
        ),
        (
            bytes(&ClientMessage::KeepAlive),
            bytes(&pre_e5::ClientMessage::KeepAlive),
        ),
        (
            bytes(&ClientMessage::SetDisplayName {
                display_name: name(),
            }),
            bytes(&pre_e5::ClientMessage::SetDisplayName {
                display_name: name(),
            }),
        ),
    ];
    for (index, (now, then)) in pairs.iter().enumerate() {
        assert_eq!(now, then, "client variant {index} changed its encoding");
    }
}

#[test]
fn every_pre_e5_server_message_encodes_to_the_same_bytes() {
    let text = || "text".to_owned();
    let info = LobbyInfo {
        lobby_id: 5,
        code: text(),
        host_name: text(),
        player_count: 2,
        max_players: 8,
    };
    let old_info = pre_e5::LobbyInfo {
        lobby_id: 5,
        code: text(),
        host_name: text(),
        player_count: 2,
        max_players: 8,
    };
    let pairs: [(Vec<u8>, Vec<u8>); 9] = [
        (
            bytes(&ServerMessage::Welcome { player_uuid: UUID }),
            bytes(&pre_e5::ServerMessage::Welcome { player_uuid: UUID }),
        ),
        (
            bytes(&ServerMessage::LobbyCreated {
                lobby_id: 5,
                code: text(),
            }),
            bytes(&pre_e5::ServerMessage::LobbyCreated {
                lobby_id: 5,
                code: text(),
            }),
        ),
        (
            bytes(&ServerMessage::LobbyJoined {
                lobby_id: 5,
                host_uuid: UUID,
                existing_members: vec![UUID, 1],
            }),
            bytes(&pre_e5::ServerMessage::LobbyJoined {
                lobby_id: 5,
                host_uuid: UUID,
                existing_members: vec![UUID, 1],
            }),
        ),
        (
            bytes(&ServerMessage::LobbyError { reason: text() }),
            bytes(&pre_e5::ServerMessage::LobbyError { reason: text() }),
        ),
        (
            bytes(&ServerMessage::PlayerJoined { player_uuid: UUID }),
            bytes(&pre_e5::ServerMessage::PlayerJoined { player_uuid: UUID }),
        ),
        (
            bytes(&ServerMessage::PlayerLeft { player_uuid: UUID }),
            bytes(&pre_e5::ServerMessage::PlayerLeft { player_uuid: UUID }),
        ),
        (
            bytes(&ServerMessage::LobbyList {
                lobbies: vec![info],
            }),
            bytes(&pre_e5::ServerMessage::LobbyList {
                lobbies: vec![old_info],
            }),
        ),
        (
            bytes(&ServerMessage::Disconnected { reason: text() }),
            bytes(&pre_e5::ServerMessage::Disconnected { reason: text() }),
        ),
        (
            bytes(&ServerMessage::Signal {
                sender_uuid: UUID,
                data: text(),
            }),
            bytes(&pre_e5::ServerMessage::Signal {
                sender_uuid: UUID,
                data: text(),
            }),
        ),
    ];
    for (index, (now, then)) in pairs.iter().enumerate() {
        assert_eq!(now, then, "server variant {index} changed its encoding");
    }
}

/// What a server from before this change does with a newer client's additions: fails to decode
/// them — which it logs and skips — rather than reading them as something it knows.
#[test]
fn a_new_client_message_fails_to_decode_on_a_pre_e5_server_rather_than_misreading() {
    for message in [
        ClientMessage::DeclareCapabilities {
            capabilities: CAPABILITY_HOST_MIGRATION,
        },
        ClientMessage::CloseLobby,
    ] {
        assert!(
            !decodes_as::<pre_e5::ClientMessage>(&bytes(&message)),
            "{message:?} reads as a pre-E5 message"
        );
    }
}

/// The same from the other side, for a client that was never going to be sent these anyway.
#[test]
fn a_new_server_message_fails_to_decode_on_a_pre_e5_client_rather_than_misreading() {
    for message in [
        ServerMessage::LobbyMigratable {
            lobby_id: 5,
            idle_timeout_secs: 60,
        },
        ServerMessage::HostChanged {
            lobby_id: 5,
            previous_host: UUID,
            new_host: 1,
            code: "ABCD".into(),
            members: vec![1],
        },
    ] {
        assert!(
            !decodes_as::<pre_e5::ServerMessage>(&bytes(&message)),
            "{message:?} reads as a pre-E5 message"
        );
    }
}
