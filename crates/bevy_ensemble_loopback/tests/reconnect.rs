//! A broken link made again, over the wire.
//!
//! `break_link` is a connection failing at both ends while the arbiter still has both in the
//! lobby; `reconnect` is the host reaching the client again. Everything between the two is the
//! core's: the host holding the seat, the client waiting for the same host, and the handshake
//! that verifies the new connection.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::{
    AwaitingHost, AwaitingSeat, EnsembleAppExt, EnsemblePlugin, HandshakeVerified, HostChanged,
    HostMigratable, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyLeft, LobbyLeftReason,
    LobbyMessage, LobbyParticipant, LocalMultiplayerPlayerId, PeerTimeout, ReceivedEnsembleMessage,
    Reconnectable, VerifiedHost,
};
use bevy_ensemble_loopback::{LoopbackNetwork, LoopbackTransportPlugin, PeerId};
use serde::{Deserialize, Serialize};

const FRAME: Duration = Duration::from_micros(15_625);

const HOST: u128 = 1;
const A: u128 = 2;
const B: u128 = 3;

/// One second: 64 frames.
const RECONNECT: Reconnectable = Reconnectable {
    within: Duration::from_secs(1),
};

#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Note(String);

#[derive(Resource, Default)]
struct Heard(Vec<(Option<u128>, String)>);

#[derive(Resource, Default)]
struct Departures(Vec<LobbyLeftReason>);

#[derive(Resource, Default)]
struct Changes(usize);

fn collect(
    mut notes: MessageReader<ReceivedEnsembleMessage<Note>>,
    mut left: MessageReader<LobbyLeft>,
    mut changed: MessageReader<HostChanged>,
    mut heard: ResMut<Heard>,
    mut departures: ResMut<Departures>,
    mut changes: ResMut<Changes>,
) {
    for message in notes.read() {
        heard.0.push((message.sender, message.message.0.clone()));
    }
    for message in left.read() {
        departures.0.push(message.reason.clone());
    }
    changes.0 += changed.read().count();
}

fn peer(uuid: u128) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
        .add_plugins((EnsemblePlugin, LoopbackTransportPlugin))
        .register_ensemble_message_type::<Note>("Note")
        .insert_resource(LocalMultiplayerPlayerId(uuid))
        .init_resource::<Heard>()
        .init_resource::<Departures>()
        .init_resource::<Changes>()
        .add_systems(Update, collect);
    app
}

/// A host and two clients, reconnectable or not, run until the roster settles.
fn session(reconnection: Option<Reconnectable>) -> (LoopbackNetwork, PeerId, PeerId, PeerId) {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(HOST, peer(HOST));
    let a = net.add_client(A, peer(A));
    let b = net.add_client(B, peer(B));
    net.set_reconnection(reconnection);
    net.run(20);
    (net, host, a, b)
}

fn has_lobby(net: &mut LoopbackNetwork, peer: PeerId) -> bool {
    let world = net.app_mut(peer).world_mut();
    world
        .query_filtered::<(), With<Lobby>>()
        .iter(world)
        .next()
        .is_some()
}

fn awaiting(net: &mut LoopbackNetwork, peer: PeerId) -> Option<AwaitingHost> {
    let world = net.app_mut(peer).world_mut();
    world.query::<&AwaitingHost>().iter(world).next().copied()
}

fn roster(net: &mut LoopbackNetwork, peer: PeerId) -> Vec<u128> {
    let world = net.app_mut(peer).world_mut();
    let mut list: Vec<u128> = world
        .query::<&LobbyParticipant>()
        .iter(world)
        .map(|participant| participant.player_uuid)
        .collect();
    list.sort();
    list
}

fn held_seats(net: &mut LoopbackNetwork, host: PeerId) -> Vec<u128> {
    let world = net.app_mut(host).world_mut();
    world
        .query_filtered::<&LobbyParticipant, With<AwaitingSeat>>()
        .iter(world)
        .map(|participant| participant.player_uuid)
        .collect()
}

fn verified_seats(net: &mut LoopbackNetwork, host: PeerId) -> Vec<u128> {
    let world = net.app_mut(host).world_mut();
    let mut seats: Vec<u128> = world
        .query_filtered::<&LobbyClientPlayerUuid, (With<LobbyClient>, With<HandshakeVerified>)>()
        .iter(world)
        .map(|uuid| uuid.0)
        .collect();
    seats.sort();
    seats
}

fn follows_verified(net: &mut LoopbackNetwork, peer: PeerId, host: u128) -> bool {
    let world = net.app_mut(peer).world_mut();
    world
        .query_filtered::<&VerifiedHost, (With<Lobby>, With<HandshakeVerified>)>()
        .iter(world)
        .any(|verified| verified.0 == host)
}

fn departures(net: &LoopbackNetwork, peer: PeerId) -> Vec<LobbyLeftReason> {
    net.app(peer).world().resource::<Departures>().0.clone()
}

fn note(net: &mut LoopbackNetwork, from: PeerId, text: &str) {
    let lobby = net.lobby(from);
    let world = net.app_mut(from).world_mut();
    world.trigger(LobbyMessage::new(lobby, Note(text.into())));
    world.flush();
}

fn heard(net: &LoopbackNetwork, peer: PeerId) -> Vec<(Option<u128>, String)> {
    net.app(peer).world().resource::<Heard>().0.clone()
}

#[test]
fn a_broken_link_holds_the_seat_and_tells_nobody_anyone_left() {
    let (mut net, host, a, b) = session(Some(RECONNECT));
    net.break_link(a);
    net.run(10);

    assert_eq!(held_seats(&mut net, host), [A], "the host holds A's place");
    assert_eq!(roster(&mut net, host), [HOST, A, B]);
    assert_eq!(roster(&mut net, b), [HOST, A, B], "B was told nothing");
    assert!(departures(&net, b).is_empty());

    let waiting = awaiting(&mut net, a).expect("A waits for its host");
    assert!(waiting.is_reconnect(), "for the same host: {waiting:?}");
    assert_eq!(waiting.previous, HOST);
    assert!(has_lobby(&mut net, a));
    assert!(departures(&net, a).is_empty());
}

#[test]
fn the_host_reaching_the_client_again_resumes_the_session() {
    let (mut net, host, a, _b) = session(Some(RECONNECT));
    net.break_link(a);
    net.run(10);
    net.reconnect(a);
    net.run(10);

    assert_eq!(awaiting(&mut net, a), None, "A was reached again");
    assert!(
        follows_verified(&mut net, a, HOST),
        "the new link is verified"
    );
    assert_eq!(verified_seats(&mut net, host), [A, B]);
    assert!(held_seats(&mut net, host).is_empty());
    assert_eq!(roster(&mut net, a), [HOST, A, B]);
    assert_eq!(
        net.app(a).world().resource::<Changes>().0,
        0,
        "nobody changed hands"
    );
    assert!(departures(&net, a).is_empty());

    note(&mut net, host, "welcome back");
    note(&mut net, a, "thanks");
    net.run(5);
    assert!(heard(&net, a).contains(&(Some(HOST), "welcome back".into())));
    assert!(heard(&net, host).contains(&(Some(A), "thanks".into())));
}

#[test]
fn a_client_that_is_not_reached_in_time_is_dropped_and_leaves() {
    let (mut net, host, a, b) = session(Some(RECONNECT));
    net.break_link(a);
    net.run(80);

    assert!(held_seats(&mut net, host).is_empty());
    assert_eq!(roster(&mut net, host), [HOST, B]);
    assert_eq!(
        roster(&mut net, b),
        [HOST, B],
        "B is told A left once it is given up on"
    );
    assert!(!has_lobby(&mut net, a));
    assert_eq!(departures(&net, a), [LobbyLeftReason::HostGone]);
}

#[test]
fn a_migratable_lobby_waits_for_a_successor_once_the_host_has_not_come_back() {
    let (mut net, _host, a, _b) = session(Some(RECONNECT));
    net.set_host_migration(Some(HostMigratable {
        successor_within: Duration::from_secs(4),
        reach_within: Duration::from_secs(2),
    }));
    net.break_link(a);
    net.run(80);

    let waiting = awaiting(&mut net, a).expect("still in the lobby");
    assert_eq!(waiting.successor, None, "now waiting to be told who hosts");
    assert!(
        waiting.waited >= RECONNECT.within,
        "the clock carried on: {waiting:?}"
    );
    assert!(departures(&net, a).is_empty());
}

#[test]
fn without_reconnection_a_broken_link_ends_as_it_always_did() {
    let (mut net, host, a, b) = session(None);
    net.break_link(a);
    net.run(10);

    assert!(held_seats(&mut net, host).is_empty());
    assert_eq!(roster(&mut net, host), [HOST, B]);
    assert_eq!(roster(&mut net, b), [HOST, B]);
    assert!(!has_lobby(&mut net, a));
    assert_eq!(departures(&net, a), [LobbyLeftReason::HostGone]);
}

#[test]
fn a_client_gone_silent_is_held_rather_than_dropped() {
    let (mut net, host, a, b) = session(Some(RECONNECT));
    net.app_mut(host)
        .insert_resource(PeerTimeout(Some(Duration::from_millis(200))));
    net.half_open(a);
    net.run(30);

    assert_eq!(held_seats(&mut net, host), [A]);
    assert_eq!(roster(&mut net, b), [HOST, A, B]);
    assert!(departures(&net, b).is_empty());
}
