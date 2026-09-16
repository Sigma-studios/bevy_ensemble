//! The signalling server, driven over a real websocket from an in-process client.
//!
//! What is on the wire is postcard in binary frames — `protocol::encode` / `decode` — which is
//! what the plugin's own connection sends, so that is what these send too.

use std::time::Duration;

use bevy_ensemble_webrtc::protocol::{
    CAPABILITY_HOST_MIGRATION, ClientMessage, ServerMessage, decode, encode,
};
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

/// A fresh, authenticated connection that declares host migration, the way a client that
/// supports it connects: `Authenticate`, then the declaration.
async fn migrating_player(server: &SignallingServer, name: &str) -> (Ws, u128) {
    let (mut ws, uuid) = player(server, name).await;
    send(
        &mut ws,
        &ClientMessage::DeclareCapabilities {
            capabilities: CAPABILITY_HOST_MIGRATION,
        },
    )
    .await;
    (ws, uuid)
}

/// Create a lobby as a migrating player: `LobbyCreated`, then `LobbyMigratable` for the same id.
async fn create_migratable_lobby(ws: &mut Ws) -> (u64, String) {
    let (lobby_id, code) = create_lobby(ws, 8).await;
    match recv(ws).await {
        ServerMessage::LobbyMigratable { lobby_id: id, .. } => assert_eq!(id, lobby_id),
        other => panic!("expected LobbyMigratable after LobbyCreated, got {other:?}"),
    }
    (lobby_id, code)
}

/// Join as a migrating player: `LobbyJoined`, then `LobbyMigratable`. Returns the host it named.
async fn join_migratable(ws: &mut Ws, code: &str) -> u128 {
    let host = match join_by_code(ws, code).await {
        ServerMessage::LobbyJoined { host_uuid, .. } => host_uuid,
        other => panic!("expected LobbyJoined, got {other:?}"),
    };
    assert!(matches!(
        recv(ws).await,
        ServerMessage::LobbyMigratable { .. }
    ));
    host
}

/// Everything the server sends until it has been quiet for [`SILENCE`].
async fn drain(ws: &mut Ws) -> Vec<ServerMessage> {
    let mut messages = Vec::new();
    while let Ok(Some(Ok(frame))) = tokio::time::timeout(SILENCE, ws.next()).await {
        if let Message::Binary(bytes) = frame {
            messages.push(decode::<ServerMessage>(&bytes).expect("decode a server message"));
        }
    }
    messages
}

/// The one `HostChanged` among what the server sends before going quiet.
async fn host_change(ws: &mut Ws) -> (u128, u128, String, Vec<u128>) {
    let changes: Vec<_> = drain(ws)
        .await
        .into_iter()
        .filter_map(|message| match message {
            ServerMessage::HostChanged {
                previous_host,
                new_host,
                code,
                members,
                ..
            } => Some((previous_host, new_host, code, members)),
            _ => None,
        })
        .collect();
    match <[_; 1]>::try_from(changes) {
        Ok([change]) => change,
        Err(changes) => panic!("expected one HostChanged, got {changes:?}"),
    }
}

/// A migratable lobby hosted by `host`, joined in order by a migrating player per name, with every
/// join announcement drained.
async fn migratable_lobby(
    server: &SignallingServer,
    names: &[&str],
) -> ((Ws, u128), String, Vec<(Ws, u128)>) {
    let (mut host, host_uuid) = migrating_player(server, "host").await;
    let (_, code) = create_migratable_lobby(&mut host).await;
    let mut members = Vec::new();
    for name in names {
        let (mut ws, uuid) = migrating_player(server, name).await;
        assert_eq!(join_migratable(&mut ws, &code).await, host_uuid);
        members.push((ws, uuid));
    }
    drain(&mut host).await;
    for (ws, _) in &mut members {
        drain(ws).await;
    }
    ((host, host_uuid), code, members)
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

// ── Host migration ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_host_leaving_a_migratable_lobby_hands_it_to_the_earliest_joined_member() {
    let server = SignallingServer::start();
    let ((mut host, host_uuid), code, mut members) = migratable_lobby(&server, &["a", "b"]).await;
    let (a_uuid, b_uuid) = (members[0].1, members[1].1);

    send(&mut host, &ClientMessage::LeaveLobby).await;

    for (ws, _) in &mut members {
        assert_eq!(
            host_change(ws).await,
            (host_uuid, a_uuid, code.clone(), vec![a_uuid, b_uuid]),
            "every member hears the same change"
        );
    }
    // The leaving host is told the peers it had, as a leaving member always was.
    let departed: Vec<_> = drain(&mut host).await;
    assert!(
        departed
            .iter()
            .all(|message| matches!(message, ServerMessage::PlayerLeft { .. })),
        "the old host was sent {departed:?}"
    );
}

#[tokio::test]
async fn the_successor_is_chosen_by_join_order_not_by_uuid() {
    let server = SignallingServer::start();
    let (mut host, _) = migrating_player(&server, "host").await;
    let (_, code) = create_migratable_lobby(&mut host).await;

    // Whichever of the two has the larger uuid joins first, so that neither end of the uuid order
    // agrees with the join order by accident.
    let mut pair = [
        migrating_player(&server, "one").await,
        migrating_player(&server, "two").await,
    ];
    pair.sort_by_key(|(_, uuid)| std::cmp::Reverse(*uuid));
    let [(mut first, first_uuid), (mut second, second_uuid)] = pair;
    assert!(first_uuid > second_uuid);
    join_migratable(&mut first, &code).await;
    join_migratable(&mut second, &code).await;
    drain(&mut first).await;

    send(&mut host, &ClientMessage::LeaveLobby).await;
    let (_, new_host, _, members) = host_change(&mut second).await;
    assert_eq!(new_host, first_uuid);
    assert_eq!(members, [first_uuid, second_uuid]);
}

#[tokio::test]
async fn a_lobby_whose_host_never_declared_migration_ends_as_it_always_did() {
    let server = SignallingServer::start();
    let (mut host, _) = player(&server, "old host").await;
    let (_, code) = create_lobby(&mut host, 4).await;
    let (mut member, _) = migrating_player(&server, "member").await;
    assert!(matches!(
        join_by_code(&mut member, &code).await,
        ServerMessage::LobbyJoined { .. }
    ));

    send(&mut host, &ClientMessage::LeaveLobby).await;
    match recv(&mut member).await {
        ServerMessage::Disconnected { reason } => assert_eq!(reason, "Host left the lobby"),
        other => panic!("expected Disconnected, got {other:?} (and no LobbyMigratable before it)"),
    }
    match list_lobbies(&mut member).await {
        ServerMessage::LobbyList { lobbies } => assert!(lobbies.is_empty()),
        other => panic!("expected LobbyList, got {other:?}"),
    }
}

#[tokio::test]
async fn a_member_that_never_declared_migration_is_disconnected_and_never_chosen() {
    let server = SignallingServer::start();
    let (mut host, host_uuid) = migrating_player(&server, "host").await;
    let (_, code) = create_migratable_lobby(&mut host).await;

    // The old build joins first, so it would be the successor if capability did not decide.
    let (mut old, _) = player(&server, "old build").await;
    assert!(matches!(
        join_by_code(&mut old, &code).await,
        ServerMessage::LobbyJoined { .. }
    ));
    let (mut new, new_uuid) = migrating_player(&server, "new build").await;
    join_migratable(&mut new, &code).await;

    send(&mut host, &ClientMessage::LeaveLobby).await;

    assert_eq!(
        host_change(&mut new).await,
        (host_uuid, new_uuid, code, vec![new_uuid])
    );
    let told_old = drain(&mut old).await;
    assert!(
        matches!(
            &told_old[..],
            [ServerMessage::PlayerJoined { .. }, ServerMessage::Disconnected { reason }]
                if reason == "Host left the lobby"
        ),
        "the old build was sent {told_old:?}, which must hold nothing it cannot decode"
    );
}

#[tokio::test]
async fn a_migratable_lobby_with_no_capable_member_left_is_removed() {
    let server = SignallingServer::start();
    let (mut host, _) = migrating_player(&server, "host").await;
    let (_, code) = create_migratable_lobby(&mut host).await;
    let (mut old, _) = player(&server, "old build").await;
    join_by_code(&mut old, &code).await;

    send(&mut host, &ClientMessage::LeaveLobby).await;
    assert!(matches!(
        recv(&mut old).await,
        ServerMessage::Disconnected { .. }
    ));
    match list_lobbies(&mut old).await {
        ServerMessage::LobbyList { lobbies } => assert!(lobbies.is_empty()),
        other => panic!("expected LobbyList, got {other:?}"),
    }
}

#[tokio::test]
async fn after_a_change_signals_are_relayed_only_to_and_from_the_new_host() {
    let server = SignallingServer::start();
    let ((mut host, host_uuid), _, mut members) = migratable_lobby(&server, &["a", "b", "c"]).await;
    send(&mut host, &ClientMessage::LeaveLobby).await;
    for (ws, _) in &mut members {
        host_change(ws).await;
    }
    let [(mut a, a_uuid), (mut b, b_uuid), (mut c, c_uuid)] =
        <[_; 3]>::try_from(members).unwrap_or_else(|_| unreachable!());

    let signal = |receiver_uuid: u128, data: &str| ClientMessage::Signal {
        receiver_uuid,
        data: data.into(),
    };
    // Member to member, and to the host that left: dropped. Sent first, so that had either been
    // carried it would already be waiting by the time the checks below read.
    send(&mut b, &signal(c_uuid, "b->c")).await;
    send(&mut b, &signal(host_uuid, "b->old host")).await;
    // To and from the new host: carried.
    send(&mut b, &signal(a_uuid, "b->a")).await;
    send(&mut a, &signal(b_uuid, "a->b")).await;

    assert!(
        matches!(recv(&mut a).await, ServerMessage::Signal { sender_uuid, data } if sender_uuid == b_uuid && data == "b->a")
    );
    assert!(
        matches!(recv(&mut b).await, ServerMessage::Signal { sender_uuid, data } if sender_uuid == a_uuid && data == "a->b")
    );
    let leaked_to_c = drain(&mut c).await;
    assert!(leaked_to_c.is_empty(), "c was sent {leaked_to_c:?}");
    let leaked_to_old_host: Vec<_> = drain(&mut host)
        .await
        .into_iter()
        .filter(|message| matches!(message, ServerMessage::Signal { .. }))
        .collect();
    assert!(leaked_to_old_host.is_empty());
}

#[tokio::test]
async fn a_join_after_a_change_names_the_new_host() {
    let server = SignallingServer::start();
    let ((mut host, _), code, mut members) = migratable_lobby(&server, &["a", "b"]).await;
    send(&mut host, &ClientMessage::LeaveLobby).await;
    for (ws, _) in &mut members {
        host_change(ws).await;
    }
    let (a_uuid, b_uuid) = (members[0].1, members[1].1);

    let (mut late, late_uuid) = migrating_player(&server, "late").await;
    match join_by_code(&mut late, &code).await {
        ServerMessage::LobbyJoined {
            host_uuid,
            existing_members,
            ..
        } => {
            assert_eq!(host_uuid, a_uuid);
            assert_eq!(existing_members, [a_uuid, b_uuid]);
        }
        other => panic!("expected LobbyJoined, got {other:?}"),
    }
    assert!(matches!(
        recv(&mut late).await,
        ServerMessage::LobbyMigratable { .. }
    ));
    for (ws, _) in &mut members {
        assert!(
            matches!(recv(ws).await, ServerMessage::PlayerJoined { player_uuid } if player_uuid == late_uuid)
        );
    }
}

#[tokio::test]
async fn the_listing_shows_the_new_hosts_name_under_the_same_code() {
    let server = SignallingServer::start();
    let ((mut host, _), code, mut members) = migratable_lobby(&server, &["a", "b"]).await;
    send(&mut host, &ClientMessage::LeaveLobby).await;
    host_change(&mut members[0].0).await;

    let ServerMessage::LobbyList { lobbies } = list_lobbies(&mut members[0].0).await else {
        panic!("expected LobbyList");
    };
    let [lobby] = &lobbies[..] else {
        panic!("expected one lobby, got {lobbies:?}");
    };
    assert_eq!(lobby.code, code);
    assert_eq!(lobby.host_name, "a");
    assert_eq!(lobby.player_count, 2);
}

/// A join and the host leaving, sent at the same moment from two connections, over and over: the
/// joiner always learns the lobby is migratable before any change to it, and always ends up
/// knowing who the host is.
#[tokio::test]
async fn a_joiner_hears_that_the_lobby_is_migratable_before_any_host_change() {
    let server = SignallingServer::start();
    for round in 0..20 {
        // By hand: only the joiner's stream is read, so nobody else needs draining.
        let (mut host, _) = migrating_player(&server, "host").await;
        let (_, code) = create_migratable_lobby(&mut host).await;
        let (mut a, a_uuid) = migrating_player(&server, "a").await;
        join_migratable(&mut a, &code).await;
        let (mut joiner, _) = migrating_player(&server, "joiner").await;

        let join = ClientMessage::JoinLobbyByCode { code: code.clone() };
        tokio::join!(
            send(&mut joiner, &join),
            send(&mut host, &ClientMessage::LeaveLobby)
        );

        let told = drain(&mut joiner).await;
        let joined = told
            .iter()
            .position(|m| matches!(m, ServerMessage::LobbyJoined { .. }));
        let migratable = told
            .iter()
            .position(|m| matches!(m, ServerMessage::LobbyMigratable { .. }));
        let changed = told
            .iter()
            .position(|m| matches!(m, ServerMessage::HostChanged { .. }));
        let (Some(joined), Some(migratable)) = (joined, migratable) else {
            panic!("round {round}: the joiner was sent {told:?}");
        };
        assert!(joined < migratable, "round {round}: {told:?}");
        let host_known = match changed {
            Some(changed) => {
                assert!(migratable < changed, "round {round}: {told:?}");
                matches!(&told[changed], ServerMessage::HostChanged { new_host, .. } if *new_host == a_uuid)
            }
            None => {
                matches!(&told[joined], ServerMessage::LobbyJoined { host_uuid, .. } if *host_uuid == a_uuid)
            }
        };
        assert!(host_known, "round {round}: the joiner was sent {told:?}");
    }
}

#[tokio::test]
async fn a_successor_that_leaves_at_once_hands_over_to_the_next() {
    let server = SignallingServer::start();
    let ((mut host, _), code, mut members) = migratable_lobby(&server, &["a", "b"]).await;
    let (a_uuid, b_uuid) = (members[0].1, members[1].1);

    send(&mut host, &ClientMessage::LeaveLobby).await;
    host_change(&mut members[0].0).await;
    send(&mut members[0].0, &ClientMessage::LeaveLobby).await;

    let told_b = drain(&mut members[1].0).await;
    let changes: Vec<_> = told_b
        .iter()
        .filter_map(|m| match m {
            ServerMessage::HostChanged {
                previous_host,
                new_host,
                code,
                members,
                ..
            } => Some((*previous_host, *new_host, code.clone(), members.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        changes.last(),
        Some(&(a_uuid, b_uuid, code, vec![b_uuid])),
        "b was sent {told_b:?}"
    );
}

#[tokio::test]
async fn an_idle_host_is_replaced_after_the_idle_timeout() {
    let server = SignallingServer::start_with(Limits {
        idle_timeout: Duration::from_secs(1),
        ..Limits::default()
    });
    // Built by hand rather than with `migratable_lobby`, whose draining would outlast a one-second
    // idle timeout on every connection, not only the host's.
    let (mut host, host_uuid) = migrating_player(&server, "host").await;
    let (_, code) = create_migratable_lobby(&mut host).await;
    let (mut a, a_uuid) = migrating_player(&server, "a").await;
    join_migratable(&mut a, &code).await;
    let (a, a_uuid) = (&mut a, &a_uuid);

    // The host says nothing more; the member keeps its own connection alive while it waits.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the member was never told the silent host was replaced"
        );
        send(a, &ClientMessage::KeepAlive).await;
        if let Ok(Some(Ok(Message::Binary(bytes)))) =
            tokio::time::timeout(Duration::from_millis(300), a.next()).await
        {
            if let ServerMessage::HostChanged {
                previous_host,
                new_host,
                ..
            } = decode::<ServerMessage>(&bytes).unwrap()
            {
                assert_eq!((previous_host, new_host), (host_uuid, *a_uuid));
                break;
            }
        }
    }
}

#[tokio::test]
async fn a_closed_lobby_is_not_migrated() {
    let server = SignallingServer::start();
    let ((mut host, _), _, mut members) = migratable_lobby(&server, &["a"]).await;

    // From a member, closing is ignored.
    send(&mut members[0].0, &ClientMessage::CloseLobby).await;
    match list_lobbies(&mut members[0].0).await {
        ServerMessage::LobbyList { lobbies } => assert_eq!(lobbies.len(), 1),
        other => panic!("expected LobbyList, got {other:?}"),
    }

    send(&mut host, &ClientMessage::CloseLobby).await;
    match recv(&mut members[0].0).await {
        ServerMessage::Disconnected { reason } => assert_eq!(reason, "Host closed the lobby"),
        other => panic!("expected Disconnected, got {other:?}"),
    }
    match list_lobbies(&mut members[0].0).await {
        ServerMessage::LobbyList { lobbies } => assert!(lobbies.is_empty()),
        other => panic!("expected LobbyList, got {other:?}"),
    }
}

/// What an older server does with a newer client's messages, from the other side: a frame it
/// cannot decode is skipped, and the connection carries on.
#[tokio::test]
async fn an_unknown_client_variant_does_not_close_the_connection() {
    let server = SignallingServer::start();
    let (mut ws, _) = player(&server, "from the future").await;
    // Variant 200, in postcard's varint: no build of this server has that many.
    ws.send(Message::Binary(vec![0xC8, 0x01].into()))
        .await
        .expect("send to the server");
    match list_lobbies(&mut ws).await {
        ServerMessage::LobbyList { .. } => {}
        other => panic!("expected LobbyList, got {other:?}"),
    }
}
