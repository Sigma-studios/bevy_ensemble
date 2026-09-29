//! Lobby requests through the plugin, against the in-process signalling server: what a refusal
//! ends and what it does not, what a cancelled request leaves behind on the server, and what a
//! joiner is told.
//!
//! The tests that need a WebRTC session to form need real loopback UDP (run outside a sandbox
//! that blocks it); they print `SKIPPED` and pass when ICE never connects, since what they check
//! is the signalling and not the network.
#![cfg(not(target_arch = "wasm32"))]

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_ensemble::{
    EnsemblePlugin, HandshakeVerified, Host, HostUuid, LeaveLobby, Lobby, LobbyJoinFailed,
    LocalMultiplayerPlayerId, PendingLobby, StartHosting, VerifiedHost,
};
use bevy_ensemble_webrtc::protocol::{
    ClientMessage, LobbyInfo, ServerMessage, SignallingError, decode, encode,
};
use bevy_ensemble_webrtc::server::test_support::SignallingServer;
use bevy_ensemble_webrtc::{
    BevyEnsembleWebrtcPlugin, IceServers, JoinFirstLobby, JoinWebrtcLobbyByCode, LobbyWebrtcCode,
    RefreshLobbyList, SignallingRefused,
};

const SETTLE: Duration = Duration::from_secs(10);
const FRAME: Duration = Duration::from_millis(4);

/// Every `LobbyJoinFailed` and `SignallingRefused` an app saw.
#[derive(Resource, Default)]
struct Seen {
    failures: Vec<String>,
    refusals: Vec<SignallingError>,
}

fn collect(
    mut failed: MessageReader<LobbyJoinFailed>,
    mut refused: MessageReader<SignallingRefused>,
    mut seen: ResMut<Seen>,
) {
    seen.failures
        .extend(failed.read().map(|failure| failure.reason.clone()));
    seen.refusals
        .extend(refused.read().map(|refusal| refusal.error));
}

fn app(server_url: String, name: &str) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(EnsemblePlugin)
        .add_plugins(BevyEnsembleWebrtcPlugin {
            server_url,
            display_name: name.into(),
            ice_servers: IceServers::none(),
            ..default()
        })
        .init_resource::<Seen>()
        .add_systems(Update, collect);
    app
}

fn run_until(
    apps: &mut [&mut App],
    within: Duration,
    mut done: impl FnMut(&mut [&mut App]) -> bool,
) -> bool {
    let deadline = Instant::now() + within;
    loop {
        for app in apps.iter_mut() {
            app.update();
        }
        if done(apps) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(FRAME);
    }
}

fn uuid(app: &App) -> Option<u128> {
    app.world()
        .get_resource::<LocalMultiplayerPlayerId>()
        .map(|id| id.0)
}

fn host_uuid(app: &App) -> Option<u128> {
    app.world().get_resource::<HostUuid>().map(|id| id.0)
}

fn code_of<F: bevy::ecs::query::QueryFilter>(app: &mut App) -> Option<String> {
    let world = app.world_mut();
    world
        .query_filtered::<&LobbyWebrtcCode, F>()
        .iter(world)
        .next()
        .map(|c| c.0.clone())
}

fn hosted_code(app: &mut App) -> Option<String> {
    code_of::<(With<Lobby>, With<Host>)>(app)
}

fn count<F: bevy::ecs::query::QueryFilter>(app: &mut App) -> usize {
    let world = app.world_mut();
    world.query_filtered::<(), F>().iter(world).count()
}

fn verified_host(app: &mut App) -> Option<u128> {
    let world = app.world_mut();
    world
        .query_filtered::<&VerifiedHost, (With<Lobby>, With<HandshakeVerified>, Without<Host>)>()
        .iter(world)
        .next()
        .map(|v| v.0)
}

fn server_lobby_of(server: &SignallingServer, uuid: u128) -> Option<u64> {
    server
        .state()
        .connections
        .get(&uuid)
        .and_then(|c| c.lobby_id)
}

fn seen(app: &App) -> &Seen {
    app.world().resource::<Seen>()
}

/// Host a lobby and wait for it to exist.
fn hosting(server: &SignallingServer, name: &str) -> (App, String) {
    let mut a = app(server.ws_url(), name);
    assert!(
        run_until(&mut [&mut a], SETTLE, |apps| uuid(apps[0]).is_some()),
        "no Welcome"
    );
    a.world_mut().write_message(StartHosting);
    assert!(
        run_until(&mut [&mut a], SETTLE, |apps| hosted_code(apps[0]).is_some()),
        "the lobby was never created"
    );
    let code = hosted_code(&mut a).unwrap();
    (a, code)
}

/// Push `frames` frames through the app's own connection within a few milliseconds.
/// `RefreshLobbyList` is simply the cheapest way to make the plugin send a frame from outside: it
/// sends one `ListLobbies` per frame it is requested in, and past the general budget of forty
/// the server refuses the rest as `RateLimited`.
fn burst(app: &mut App, frames: usize) {
    for _ in 0..frames {
        app.world_mut().write_message(RefreshLobbyList);
        app.update();
    }
}

// --- A refusal ends only what it names ----------------------------------------------------------

/// A frame refused for its rate is not a failed join. It used to be: every `LobbyError` removed
/// `LocalMultiplayerPlayerId` and `HostUuid`, so a host that sent a few too many messages at once
/// lost its identity in the middle of its own session.
#[test]
fn a_rate_limited_frame_leaves_an_active_host_its_identity() {
    let server = SignallingServer::start();
    let (mut a, _code) = hosting(&server, "a");
    let me = uuid(&a).unwrap();
    assert_eq!(host_uuid(&a), Some(me));

    burst(&mut a, 60);
    run_until(&mut [&mut a], Duration::from_millis(500), |_| false);

    assert!(
        seen(&a).refusals.contains(&SignallingError::RateLimited),
        "precondition: the burst was over the limit, and the refusals were reported"
    );
    assert!(seen(&a).failures.is_empty(), "{:?}", seen(&a).failures);
    assert_eq!(
        count::<(With<Lobby>, With<Host>)>(&mut a),
        1,
        "the hosted lobby is still there"
    );
    assert!(
        server_lobby_of(&server, me).is_some(),
        "and the server still has this peer hosting it"
    );
    assert_eq!((uuid(&a), host_uuid(&a)), (Some(me), Some(me)));
}

#[test]
fn a_rate_limited_frame_leaves_an_active_client_its_identity() {
    let server = SignallingServer::start();
    let (mut a, code) = hosting(&server, "a");
    let host = uuid(&a).unwrap();
    let mut b = app(server.ws_url(), "b");
    assert!(run_until(&mut [&mut a, &mut b], SETTLE, |apps| uuid(
        apps[1]
    )
    .is_some()));
    b.world_mut().write_message(JoinWebrtcLobbyByCode(code));
    if !run_until(&mut [&mut a, &mut b], SETTLE, |apps| {
        verified_host(apps[1]) == Some(host)
    }) {
        println!("SKIPPED: inconclusive (no ICE connection)");
        return;
    }
    let me = uuid(&b).unwrap();

    burst(&mut b, 60);
    run_until(&mut [&mut a, &mut b], Duration::from_millis(500), |_| false);

    assert!(seen(&b).refusals.contains(&SignallingError::RateLimited));
    assert_eq!(
        count::<(With<Lobby>, Without<Host>)>(&mut b),
        1,
        "still in the lobby"
    );
    assert_eq!((uuid(&b), host_uuid(&b)), (Some(me), Some(host)));
}

/// A join the server really refuses ends the pending lobby it made, and says why in words and in
/// type — and keeps the peer's identity, which belongs to the connection and not to the join.
#[test]
fn a_refused_join_ends_its_pending_lobby_and_says_why() {
    let server = SignallingServer::start();
    let mut b = app(server.ws_url(), "b");
    assert!(run_until(&mut [&mut b], SETTLE, |apps| uuid(apps[0]).is_some()));
    let me = uuid(&b);

    b.world_mut()
        .write_message(JoinWebrtcLobbyByCode("ZZZZ".into()));
    assert!(
        run_until(&mut [&mut b], SETTLE, |apps| !seen(apps[0])
            .failures
            .is_empty()),
        "no LobbyJoinFailed for a code that names no lobby"
    );

    assert_eq!(
        seen(&b).refusals,
        [SignallingError::LobbyNotFound],
        "the typed reason"
    );
    assert_eq!(
        seen(&b).failures,
        [SignallingError::LobbyNotFound.to_string()]
    );
    assert_eq!(count::<Or<(With<Lobby>, With<PendingLobby>)>>(&mut b), 0);
    assert_eq!(
        uuid(&b),
        me,
        "the connection's identity outlives a refused join"
    );
}

// --- A cancelled request leaves nothing behind on the server -----------------------------------

/// Host, then cancel on the very next frame — before `LobbyCreated` is back. The server had
/// already made the lobby, nothing here was holding it, and every later attempt was refused
/// "Already in a lobby".
#[test]
fn a_host_cancelled_before_the_server_answers_leaves_the_servers_lobby() {
    let server = SignallingServer::start();
    let mut a = app(server.ws_url(), "a");
    assert!(
        run_until(&mut [&mut a], SETTLE, |apps| uuid(apps[0]).is_some()),
        "no Welcome"
    );
    let me = uuid(&a).unwrap();

    a.world_mut().write_message(StartHosting);
    a.update();
    a.world_mut().write_message(LeaveLobby);
    a.update();
    assert!(
        count::<With<Lobby>>(&mut a) == 0 && count::<With<PendingLobby>>(&mut a) == 0,
        "precondition: the pending lobby was cancelled before it was promoted"
    );

    // Let the answer arrive, to nobody.
    run_until(&mut [&mut a], Duration::from_millis(500), |_| false);
    let stranded = server_lobby_of(&server, me);

    a.world_mut().write_message(StartHosting);
    let rehosted = run_until(&mut [&mut a], Duration::from_secs(5), |apps| {
        hosted_code(apps[0]).is_some()
    });

    assert!(
        stranded.is_none() && rehosted,
        "after cancelling, the server still has this connection in lobby {stranded:?}; hosting \
         again succeeded: {rehosted} (join failures: {:?})",
        seen(&a).failures
    );
}

#[test]
fn a_join_cancelled_before_the_server_answers_leaves_the_servers_lobby() {
    let server = SignallingServer::start();
    let (mut a, code) = hosting(&server, "a");
    let mut b = app(server.ws_url(), "b");
    assert!(run_until(&mut [&mut a, &mut b], SETTLE, |apps| uuid(
        apps[1]
    )
    .is_some()));
    let me = uuid(&b).unwrap();

    b.world_mut()
        .write_message(JoinWebrtcLobbyByCode(code.clone()));
    b.update();
    b.world_mut().write_message(LeaveLobby);
    b.update();
    assert_eq!(
        count::<Or<(With<Lobby>, With<PendingLobby>)>>(&mut b),
        0,
        "precondition: cancelled"
    );

    run_until(&mut [&mut a, &mut b], Duration::from_millis(500), |_| false);
    let stranded = server_lobby_of(&server, me);
    let lobby_sizes = server
        .state()
        .lobbies
        .iter()
        .map(|l| l.members.len())
        .collect::<Vec<_>>();

    // Wait out the lobby-operation rate limit (burst 2, 1/s), then join again.
    run_until(&mut [&mut a, &mut b], Duration::from_millis(1100), |_| {
        false
    });
    b.world_mut().write_message(JoinWebrtcLobbyByCode(code));
    run_until(&mut [&mut a, &mut b], Duration::from_millis(300), |_| false);
    let still_joining = count::<Or<(With<Lobby>, With<PendingLobby>)>>(&mut b) > 0;

    assert!(
        stranded.is_none() && still_joining,
        "after cancelling the join, the server still has B in lobby {stranded:?} (lobby sizes \
         {lobby_sizes:?}); the retried join survived: {still_joining} ({:?})",
        seen(&b).failures
    );
}

// --- What a joiner is told ---------------------------------------------------------------------

/// A joiner's lobby carries its code just as the host's does, and a code is found however it was
/// typed: lower case, spaced, with a trailing newline from a paste.
#[test]
fn a_joiner_is_told_the_code_of_the_lobby_it_joined_however_it_was_typed() {
    let server = SignallingServer::start();
    let (mut a, code) = hosting(&server, "a");
    let mut b = app(server.ws_url(), "b");
    assert!(run_until(&mut [&mut a, &mut b], SETTLE, |apps| uuid(
        apps[1]
    )
    .is_some()));

    let typed = format!(
        " {} {}\n",
        code[..2].to_lowercase(),
        code[2..].to_lowercase()
    );
    b.world_mut().write_message(JoinWebrtcLobbyByCode(typed));
    assert!(
        run_until(&mut [&mut a, &mut b], SETTLE, |apps| {
            code_of::<Without<Host>>(apps[1]).is_some()
        }),
        "the joiner's lobby never carried a code ({:?})",
        seen(&b).failures
    );
    assert_eq!(code_of::<Without<Host>>(&mut b), Some(code));
}

// --- An outdated server is said so, not waited out --------------------------------------------

/// A signalling server from before typed requests, listing `lobbies`: it authenticates and lists,
/// as every server has, and ignores everything else. Kept running for as long as the runtime is.
fn outdated_server(lobbies: Vec<LobbyInfo>) -> (tokio::runtime::Runtime, String) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    runtime.spawn(async move {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        while let Ok((stream, _)) = listener.accept().await {
            let lobbies = lobbies.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(message)) = ws.next().await {
                    let Message::Binary(bytes) = message else {
                        continue;
                    };
                    // What an older server does with a variant it does not know: nothing.
                    let reply = match decode::<ClientMessage>(&bytes) {
                        Ok(ClientMessage::Authenticate { .. }) => {
                            ServerMessage::Welcome { player_uuid: 7 }
                        }
                        Ok(ClientMessage::ListLobbies) => ServerMessage::LobbyList {
                            lobbies: lobbies.clone(),
                        },
                        _ => continue,
                    };
                    let bytes = encode(&reply).unwrap();
                    if ws.send(Message::Binary(bytes.into())).await.is_err() {
                        return;
                    }
                }
            });
        }
    });

    (runtime, url)
}

/// A signalling server from before typed requests: it authenticates and lists, as every server
/// has, and cannot decode a single lobby request from this client. A join used to sit out its
/// whole timeout against one; it is refused as soon as the server is known for what it is.
#[test]
fn a_join_through_a_server_without_typed_requests_fails_at_once_and_says_why() {
    let (_server, url) = outdated_server(Vec::new());
    let mut b = app(url, "b");
    b.world_mut()
        .write_message(JoinWebrtcLobbyByCode("ABCD".into()));
    assert!(
        run_until(&mut [&mut b], Duration::from_secs(5), |apps| !seen(apps[0])
            .failures
            .is_empty()),
        "the join against an outdated server was not refused"
    );
    assert!(
        seen(&b).failures[0].contains("older"),
        "{:?}",
        seen(&b).failures
    );
    assert_eq!(count::<Or<(With<Lobby>, With<PendingLobby>)>>(&mut b), 0);

    // And a later attempt is refused on the spot.
    b.world_mut().write_message(StartHosting);
    run_until(&mut [&mut b], Duration::from_millis(100), |_| false);
    assert_eq!(seen(&b).failures.len(), 2, "{:?}", seen(&b).failures);
    assert_eq!(count::<Or<(With<Lobby>, With<PendingLobby>)>>(&mut b), 0);
}

/// `JoinFirstLobby` against a server nothing can be joined through does not go on trying: one
/// failed join, not one every time the listing comes back.
#[test]
fn join_first_lobby_stops_at_a_server_without_typed_requests() {
    let (_server, url) = outdated_server(vec![LobbyInfo {
        lobby_id: 5,
        code: "ABCD".into(),
        host_name: "host".into(),
        player_count: 1,
        max_players: 8,
    }]);
    let mut b = app(url, "b");
    b.insert_resource(JoinFirstLobby::default());
    run_until(&mut [&mut b], Duration::from_secs(2), |_| false);
    assert!(
        seen(&b).failures.len() <= 1,
        "{} failed joins in two seconds: {:?}",
        seen(&b).failures.len(),
        seen(&b).failures
    );
}
