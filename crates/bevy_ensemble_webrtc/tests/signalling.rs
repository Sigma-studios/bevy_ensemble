//! The signalling server, driven over a real websocket from an in-process client.
//!
//! What is on the wire is postcard in binary frames — `protocol::encode` / `decode` — which is
//! what the plugin's own connection sends, so that is what these send too.

use std::time::Duration;

use bevy_ensemble_webrtc::protocol::{ClientMessage, ServerMessage, decode, encode};
use bevy_ensemble_webrtc::server::test_support::SignallingServer;
use bevy_ensemble_webrtc::server::{Limits, MAX_PLAYERS};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Long enough that a slow CI machine does not fail a test that passes, short enough that a
/// message that never comes fails the test rather than hanging it.
const REPLY: Duration = Duration::from_secs(5);
/// How long to wait for a message that must *not* arrive.
const SILENCE: Duration = Duration::from_millis(500);

async fn connect(server: &SignallingServer) -> Ws {
    let (ws, _) = tokio_tungstenite::connect_async(server.ws_url())
        .await
        .expect("connect to the in-process server");
    ws
}

async fn send(ws: &mut Ws, msg: &ClientMessage) {
    let bytes = encode(msg).expect("encode a client message");
    ws.send(Message::Binary(bytes.into()))
        .await
        .expect("send to the server");
}

/// The next server message, or `None` once the connection is closed.
///
/// Anything that is not a data frame — a ping, a pong — is skipped; a close frame, a stream that
/// ends, and a transport error all count as closed, since a server that drops the socket without
/// a close handshake shows up as the last of those.
async fn next(ws: &mut Ws, within: Duration) -> Option<ServerMessage> {
    tokio::time::timeout(within, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Binary(bytes))) => {
                    return Some(decode::<ServerMessage>(&bytes).expect("decode a server message"));
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no message from the server within {within:?}"))
}

async fn recv(ws: &mut Ws) -> ServerMessage {
    next(ws, REPLY)
        .await
        .expect("the server closed the connection")
}

/// Authenticate and return the uuid the server assigned.
async fn authenticate(ws: &mut Ws, name: &str) -> u128 {
    send(
        ws,
        &ClientMessage::Authenticate {
            display_name: name.into(),
        },
    )
    .await;
    match recv(ws).await {
        ServerMessage::Welcome { player_uuid } => player_uuid,
        other => panic!("expected Welcome, got {other:?}"),
    }
}

/// A fresh, authenticated connection.
async fn player(server: &SignallingServer, name: &str) -> (Ws, u128) {
    let mut ws = connect(server).await;
    let uuid = authenticate(&mut ws, name).await;
    (ws, uuid)
}

/// Create a lobby and return `(lobby_id, code)`.
async fn create_lobby(ws: &mut Ws, max_players: u32) -> (u64, String) {
    send(ws, &ClientMessage::CreateLobby { max_players }).await;
    match recv(ws).await {
        ServerMessage::LobbyCreated { lobby_id, code } => (lobby_id, code),
        other => panic!("expected LobbyCreated, got {other:?}"),
    }
}

async fn join_by_code(ws: &mut Ws, code: &str) -> ServerMessage {
    send(
        ws,
        &ClientMessage::JoinLobbyByCode {
            code: code.to_owned(),
        },
    )
    .await;
    recv(ws).await
}

async fn list_lobbies(ws: &mut Ws) -> ServerMessage {
    send(ws, &ClientMessage::ListLobbies).await;
    recv(ws).await
}

#[tokio::test]
async fn lobby_ids_are_not_sequential() {
    let server = SignallingServer::start();

    let (mut first, _) = player(&server, "first").await;
    let (mut second, _) = player(&server, "second").await;
    let (a, _) = create_lobby(&mut first, 4).await;
    let (b, _) = create_lobby(&mut second, 4).await;

    assert_ne!(a, b);
    assert!(a.abs_diff(b) > 1, "ids {a} and {b} are adjacent");
    assert!(a != 1 && a != 2, "first id {a} counts from one");
    assert!(b != 1 && b != 2, "second id {b} counts from one");
}

#[tokio::test]
async fn list_lobbies_before_authenticating_is_refused() {
    let server = SignallingServer::start();

    let mut ws = connect(&server).await;
    match list_lobbies(&mut ws).await {
        ServerMessage::LobbyError { reason } => assert_eq!(reason, "Not authenticated"),
        other => panic!("an unauthenticated ListLobbies was answered with {other:?}"),
    }

    // The same socket still works once it does authenticate.
    authenticate(&mut ws, "late").await;
    match list_lobbies(&mut ws).await {
        ServerMessage::LobbyList { lobbies } => assert!(lobbies.is_empty()),
        other => panic!("expected LobbyList, got {other:?}"),
    }
}

#[tokio::test]
async fn an_idle_connection_is_closed() {
    let server = SignallingServer::start_with(Limits {
        idle_timeout: Duration::from_secs(1),
        ..Limits::default()
    });

    let (mut host, _) = player(&server, "host").await;
    create_lobby(&mut host, 4).await;

    // Say nothing. The server should close the socket on its own, comfortably inside three
    // seconds for a one-second timeout.
    let closed = next(&mut host, Duration::from_secs(3)).await;
    assert!(
        closed.is_none(),
        "expected the connection closed, got {closed:?}"
    );

    // ...and the lobby the idle player hosted is gone with them.
    let (mut other, _) = player(&server, "other").await;
    match list_lobbies(&mut other).await {
        ServerMessage::LobbyList { lobbies } => {
            assert!(
                lobbies.is_empty(),
                "idle host's lobby survived: {lobbies:?}"
            );
        }
        other => panic!("expected LobbyList, got {other:?}"),
    }
}

#[tokio::test]
async fn a_keepalive_keeps_an_idle_connection_open() {
    let server = SignallingServer::start_with(Limits {
        idle_timeout: Duration::from_secs(1),
        ..Limits::default()
    });

    let (mut ws, _) = player(&server, "chatty").await;
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        send(&mut ws, &ClientMessage::KeepAlive).await;
    }

    // Two and a half seconds of nothing but keep-alives, and it is still answering.
    match list_lobbies(&mut ws).await {
        ServerMessage::LobbyList { .. } => {}
        other => panic!("expected LobbyList, got {other:?}"),
    }
}

#[tokio::test]
async fn a_flood_of_messages_is_rate_limited() {
    let server = SignallingServer::start();
    let limits = Limits::default();

    let (mut ws, _) = player(&server, "flooder").await;

    // Past the burst, but nowhere near the point of being closed: every excess message is
    // refused, and the connection survives.
    let sent = limits.message_burst as usize * 2;
    for _ in 0..sent {
        send(&mut ws, &ClientMessage::ListLobbies).await;
    }
    let mut served = 0;
    let mut refused = 0;
    for _ in 0..sent {
        match recv(&mut ws).await {
            ServerMessage::LobbyList { .. } => served += 1,
            ServerMessage::LobbyError { reason } if reason == "rate limited" => refused += 1,
            other => panic!("unexpected reply to a flood: {other:?}"),
        }
    }
    assert!(refused > 0, "{sent} messages at once and none were refused");
    assert!(served > 0, "{sent} messages at once and none were served");
    assert!(
        served <= limits.message_burst as usize + 5,
        "served {served}, more than the burst of {}",
        limits.message_burst
    );

    // Sustained far past the limit, the connection is closed. Sends can start failing before
    // every frame is out, which is the point; the assertion is on what the server does.
    let bytes = encode(&ClientMessage::KeepAlive).unwrap();
    for _ in 0..(limits.message_burst as usize * 20) {
        if ws
            .send(Message::Binary(bytes.clone().into()))
            .await
            .is_err()
        {
            break;
        }
    }
    let closed = tokio::time::timeout(REPLY, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return true,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed, "the flooding connection was not closed");
}

#[tokio::test]
async fn lobby_operations_have_their_own_limit() {
    let server = SignallingServer::start();

    let (mut ws, _) = player(&server, "guesser").await;

    // Guessing codes: after the small burst, every further guess is refused rather than answered
    // "Lobby not found", which is what would make guessing informative.
    let mut refused = 0;
    for _ in 0..10 {
        match join_by_code(&mut ws, "ZZZZ").await {
            ServerMessage::LobbyError { reason } if reason == "rate limited" => refused += 1,
            ServerMessage::LobbyError { reason } => assert_eq!(reason, "Lobby not found"),
            other => panic!("unexpected reply to a guess: {other:?}"),
        }
    }
    assert!(
        refused >= 7,
        "only {refused} of 10 rapid guesses were refused"
    );
}

#[tokio::test]
async fn a_signal_between_two_members_is_not_relayed() {
    let server = SignallingServer::start();

    let (mut host, host_uuid) = player(&server, "host").await;
    let (_, code) = create_lobby(&mut host, 4).await;

    let (mut a, a_uuid) = player(&server, "a").await;
    let (mut b, b_uuid) = player(&server, "b").await;
    assert!(matches!(
        join_by_code(&mut a, &code).await,
        ServerMessage::LobbyJoined { .. }
    ));
    assert!(matches!(
        join_by_code(&mut b, &code).await,
        ServerMessage::LobbyJoined { .. }
    ));
    // Drain the joins everybody else was told about, so the only thing left on any stream is a
    // signal or nothing.
    for _ in 0..2 {
        assert!(matches!(
            recv(&mut host).await,
            ServerMessage::PlayerJoined { .. }
        ));
    }
    assert!(
        matches!(recv(&mut a).await, ServerMessage::PlayerJoined { player_uuid } if player_uuid == b_uuid)
    );

    // Member to member: dropped.
    send(
        &mut a,
        &ClientMessage::Signal {
            receiver_uuid: b_uuid,
            data: "a->b".into(),
        },
    )
    .await;
    // Member to host: carried. Sent after the one above, so if that one had been relayed it would
    // already be on `b`'s stream by the time this one lands on the host's.
    send(
        &mut a,
        &ClientMessage::Signal {
            receiver_uuid: host_uuid,
            data: "a->host".into(),
        },
    )
    .await;
    match recv(&mut host).await {
        ServerMessage::Signal { sender_uuid, data } => {
            assert_eq!(sender_uuid, a_uuid);
            assert_eq!(data, "a->host");
        }
        other => panic!("expected the member's signal at the host, got {other:?}"),
    }

    // Host to member: carried too.
    send(
        &mut host,
        &ClientMessage::Signal {
            receiver_uuid: a_uuid,
            data: "host->a".into(),
        },
    )
    .await;
    match recv(&mut a).await {
        ServerMessage::Signal { sender_uuid, data } => {
            assert_eq!(sender_uuid, host_uuid);
            assert_eq!(data, "host->a");
        }
        other => panic!("expected the host's signal at the member, got {other:?}"),
    }

    let leaked = tokio::time::timeout(SILENCE, b.next()).await;
    assert!(
        leaked.is_err(),
        "a member-to-member signal reached the other member: {leaked:?}"
    );
}

#[tokio::test]
async fn max_players_is_clamped() {
    let server = SignallingServer::start();

    let (mut huge, _) = player(&server, "huge").await;
    let (mut zero, _) = player(&server, "zero").await;
    let (mut small, _) = player(&server, "small").await;
    let (huge_id, _) = create_lobby(&mut huge, u32::MAX).await;
    let (zero_id, _) = create_lobby(&mut zero, 0).await;
    let (small_id, _) = create_lobby(&mut small, 3).await;

    let (mut viewer, _) = player(&server, "viewer").await;
    let ServerMessage::LobbyList { lobbies } = list_lobbies(&mut viewer).await else {
        panic!("expected LobbyList");
    };
    let max_of = |id: u64| {
        lobbies
            .iter()
            .find(|l| l.lobby_id == id)
            .unwrap_or_else(|| panic!("lobby {id} is not listed"))
            .max_players
    };
    assert_eq!(max_of(huge_id), MAX_PLAYERS);
    assert_eq!(max_of(zero_id), MAX_PLAYERS);
    assert_eq!(max_of(small_id), 3);
}
