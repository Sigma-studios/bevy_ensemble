//! The server browser, and the name a lobby is listed under.
//!
//! Everything here is settled over the WebSocket to the signalling server, so unlike
//! `host_migration` there is no ICE to be missing on a CI machine and nothing is ever
//! inconclusive. Real apps running the real plugin against a real in-process server; what is
//! stubbed out is only WebRTC, which none of these reach.
#![cfg(not(target_arch = "wasm32"))]

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_ensemble::{EnsemblePlugin, LeaveLobby, Lobby, PublicLobbies, StartHosting};
use bevy_ensemble_webrtc::server::test_support::SignallingServer;
use bevy_ensemble_webrtc::{
    BevyEnsembleWebrtcPlugin, IceServers, LobbyWebrtcCode, SignallingDisplayName,
};

const SETTLE: Duration = Duration::from_secs(20);
const FRAME: Duration = Duration::from_millis(4);

fn app(server: &SignallingServer, name: &str) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(EnsemblePlugin)
        .add_plugins(BevyEnsembleWebrtcPlugin {
            server_url: server.ws_url(),
            display_name: name.into(),
            ice_servers: IceServers::none(),
            ..default()
        });
    app
}

fn run_until(
    apps: &mut Vec<App>,
    within: Duration,
    mut done: impl FnMut(&mut Vec<App>) -> bool,
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

/// The code of the lobby this peer is in, if it is in one the server has named.
fn code(app: &mut App) -> Option<String> {
    let world = app.world_mut();
    world
        .query_filtered::<&LobbyWebrtcCode, With<Lobby>>()
        .iter(world)
        .next()
        .map(|code| code.0.clone())
}

/// The server list as this peer has been told it: code and host name, in order.
fn listed(app: &App) -> Vec<(String, String)> {
    app.world()
        .get_resource::<PublicLobbies>()
        .map(|lobbies| {
            lobbies
                .0
                .iter()
                .map(|lobby| (lobby.code.clone(), lobby.host_name.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn host(apps: &mut Vec<App>, peer: usize) -> String {
    apps[peer].world_mut().write_message(StartHosting);
    assert!(
        run_until(apps, SETTLE, |apps| code(&mut apps[peer]).is_some()),
        "the server never created the lobby"
    );
    code(&mut apps[peer]).unwrap()
}

#[test]
fn a_fresh_connection_is_given_the_listing_without_asking_for_it() {
    let server = SignallingServer::start();
    let mut apps = vec![app(&server, "ava")];
    let hosted = host(&mut apps, 0);

    // Somebody else opens the game and asks for nothing at all. A browser that is empty until the
    // player finds the refresh button reads as "there is nobody playing", which is the one thing
    // it must never say by accident.
    apps.push(app(&server, "bo"));
    assert!(
        run_until(&mut apps, SETTLE, |apps| !listed(&apps[1]).is_empty()),
        "the listing stayed empty with nobody having pressed refresh"
    );
    assert_eq!(listed(&apps[1]), vec![(hosted, "ava".to_owned())]);
}

#[test]
fn a_name_set_before_the_first_frame_still_reaches_the_server() {
    let server = SignallingServer::start();
    let mut apps = vec![app(&server, "Player")];
    // A game that knows who the player is at startup — read back from a save, or from the page's
    // storage — and fills the resource in before anything has run.
    apps[0]
        .world_mut()
        .resource_mut::<SignallingDisplayName>()
        .0 = "ava".into();

    let hosted = host(&mut apps, 0);
    apps.push(app(&server, "bo"));
    assert!(run_until(&mut apps, SETTLE, |apps| !listed(&apps[1]).is_empty()));
    assert_eq!(
        listed(&apps[1]),
        vec![(hosted, "ava".to_owned())],
        "the lobby is listed under the name the plugin was built with, not the one it was given"
    );
}

#[test]
fn a_lobby_is_listed_under_its_hosts_name_after_the_host_has_left_another() {
    let server = SignallingServer::start();
    let mut apps = vec![app(&server, "Player")];
    apps[0]
        .world_mut()
        .resource_mut::<SignallingDisplayName>()
        .0 = "ava".into();

    // Host one, leave it — which rebuilds the signalling socket, and so authenticates again —
    // then host another.
    host(&mut apps, 0);
    apps[0].world_mut().write_message(LeaveLobby);
    assert!(
        run_until(&mut apps, SETTLE, |apps| code(&mut apps[0]).is_none()),
        "never left the first lobby"
    );
    let second = host(&mut apps, 0);

    apps.push(app(&server, "bo"));
    assert!(run_until(&mut apps, SETTLE, |apps| !listed(&apps[1]).is_empty()));
    assert_eq!(
        listed(&apps[1]),
        vec![(second, "ava".to_owned())],
        "the rebuilt connection re-authenticated as the plugin's placeholder"
    );
}
