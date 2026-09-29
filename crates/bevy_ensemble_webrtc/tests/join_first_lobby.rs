//! [`JoinFirstLobby`] against the in-process signalling server: a joiner started before its host
//! waits for it and joins exactly once, and a join that fails is tried again from a listing asked
//! for after the failure.
//!
//! Everything checked here is signalling — which lobby the server has each peer in, and what it
//! refused — so none of it waits on ICE, and it passes the same with or without loopback UDP.
#![cfg(not(target_arch = "wasm32"))]

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_ensemble::{
    EnsemblePlugin, Host, LeaveLobby, Lobby, LobbyJoinFailed, LocalMultiplayerPlayerId,
    PendingLobby, StartHosting,
};
use bevy_ensemble_webrtc::protocol::SignallingError;
use bevy_ensemble_webrtc::server::test_support::SignallingServer;
use bevy_ensemble_webrtc::{
    BevyEnsembleWebrtcPlugin, IceServers, JoinFirstLobby, LobbyWebrtcCode, SignallingRefused,
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

fn app(server_url: String, name: &str, max_players: u32) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(EnsemblePlugin)
        .add_plugins(BevyEnsembleWebrtcPlugin {
            server_url,
            display_name: name.into(),
            max_players,
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

fn count<F: bevy::ecs::query::QueryFilter>(app: &mut App) -> usize {
    let world = app.world_mut();
    world.query_filtered::<(), F>().iter(world).count()
}

fn hosted_code(app: &mut App) -> Option<String> {
    let world = app.world_mut();
    world
        .query_filtered::<&LobbyWebrtcCode, (With<Lobby>, With<Host>)>()
        .iter(world)
        .next()
        .map(|c| c.0.clone())
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

/// A connected app, with its uuid.
fn connected(server: &SignallingServer, name: &str, max_players: u32) -> (App, u128) {
    let mut app = app(server.ws_url(), name, max_players);
    assert!(
        run_until(&mut [&mut app], SETTLE, |apps| uuid(apps[0]).is_some()),
        "no Welcome"
    );
    let me = uuid(&app).unwrap();
    (app, me)
}

fn host(app: &mut App) {
    app.world_mut().write_message(StartHosting);
    assert!(
        run_until(&mut [app], SETTLE, |apps| hosted_code(apps[0]).is_some()),
        "the lobby was never created"
    );
}

/// The joiner asks before there is anything to join, as it does when a script starts both
/// windows at once, and joins the host's lobby once it exists — once: nothing is refused on the
/// way, in particular not a second join sent while the first was being answered, which is what
/// the hand-written versions' one-second timer was there to prevent.
#[test]
fn a_joiner_started_before_its_host_joins_it_once_it_exists() {
    let server = SignallingServer::start();
    let (mut b, joiner) = connected(&server, "b", 8);
    b.insert_resource(JoinFirstLobby::default());

    // A second of an empty listing: asked for, nothing to join.
    run_until(&mut [&mut b], Duration::from_secs(1), |_| false);
    assert_eq!(count::<Or<(With<Lobby>, With<PendingLobby>)>>(&mut b), 0);
    assert!(
        seen(&b).refusals.is_empty(),
        "refreshing the listing was refused: {:?}",
        seen(&b).refusals
    );

    let (mut a, hosting) = connected(&server, "a", 8);
    host(&mut a);
    let lobby = server_lobby_of(&server, hosting).expect("the host is in its lobby");

    assert!(
        run_until(&mut [&mut a, &mut b], SETTLE, |_| {
            server_lobby_of(&server, joiner) == Some(lobby)
        }),
        "the joiner never joined the host's lobby ({:?})",
        seen(&b).failures
    );
    // Long enough for a second join to have been sent and refused, had one been.
    run_until(&mut [&mut a, &mut b], Duration::from_millis(1500), |_| {
        false
    });

    assert!(
        seen(&b).refusals.is_empty(),
        "refused on the way: {:?}",
        seen(&b).refusals
    );
    assert_eq!(
        count::<(Or<(With<Lobby>, With<PendingLobby>)>, Without<Host>)>(&mut b),
        1,
        "one lobby, joined or still connecting"
    );
    assert_eq!(server_lobby_of(&server, joiner), Some(lobby));
}

/// The first lobby listed is full, so the join fails. The request stays, and once that lobby is
/// gone and another is listed, the joiner joins that one: the retry is from a listing asked for
/// after the failure, not the same stale entry sent again.
#[test]
fn a_failed_join_is_tried_again_from_a_fresh_listing() {
    let server = SignallingServer::start();
    // A lobby for one: the host fills it.
    let (mut full, full_host) = connected(&server, "full", 1);
    host(&mut full);
    let (mut b, joiner) = connected(&server, "b", 8);
    b.insert_resource(JoinFirstLobby::default());

    assert!(
        run_until(&mut [&mut full, &mut b], SETTLE, |apps| {
            !seen(apps[1]).failures.is_empty()
        }),
        "precondition: a join into the full lobby failed"
    );
    assert!(
        seen(&b).refusals.contains(&SignallingError::LobbyFull),
        "{:?}",
        seen(&b).refusals
    );
    assert!(
        b.world().contains_resource::<JoinFirstLobby>(),
        "the request outlives a failed join"
    );

    // The full lobby goes away, and a lobby with room takes its place.
    full.world_mut().write_message(LeaveLobby);
    assert!(run_until(&mut [&mut full, &mut b], SETTLE, |_| {
        server_lobby_of(&server, full_host).is_none()
    }));
    let (mut a, hosting) = connected(&server, "a", 8);
    host(&mut a);
    let lobby = server_lobby_of(&server, hosting).expect("the host is in its lobby");

    assert!(
        run_until(&mut [&mut a, &mut b], SETTLE, |_| {
            server_lobby_of(&server, joiner) == Some(lobby)
        }),
        "the joiner never joined the lobby with room (failures: {:?}, refusals: {:?})",
        seen(&b).failures,
        seen(&b).refusals
    );
    assert!(
        !seen(&b).refusals.contains(&SignallingError::AlreadyInLobby),
        "a join was sent while another was pending: {:?}",
        seen(&b).refusals
    );
}
