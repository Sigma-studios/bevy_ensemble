//! A peer that has been in a session and left is a fresh peer.
//!
//! Everything that means something only inside one session — packets held for an unverified
//! peer, handshakes that matched before there was a seat, player data waiting for its
//! participant, the last profile asked for, the link to the host — lives on the lobby entity, so
//! that despawning it is the reset. These tests hold the core to that: after hosting, joining,
//! following a new host and being promoted to one, and leaving each time, the app holds exactly
//! the entities it held before its first session, and the next session works.

use std::collections::HashSet;
use std::time::Duration;

use bevy::ecs::component::ComponentId;
use bevy::ecs::resource::IsResource;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::{
    AwaitingHost, EnsembleAppExt, EnsemblePlugin, HostMigratable, HostUuid, Lobby,
    LobbyParticipant, LocalMultiplayerPlayerId, PlayerData, PlayerDataPlugin,
    ReceivedEnsembleMessage, SendMode, SetPlayerData,
};
use bevy_ensemble_loopback::{LoopbackNetwork, LoopbackTransportPlugin, PeerId};
use serde::{Deserialize, Serialize};

const FRAME: Duration = Duration::from_micros(15_625);

const HOST: u128 = 1;
const A: u128 = 2;
const B: u128 = 3;

#[derive(Message, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Ping(u32);

#[derive(Message, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Profile(String);

#[derive(Resource, Default)]
struct Received(Vec<(u128, u32)>);

fn collect(mut messages: MessageReader<ReceivedEnsembleMessage<Ping>>, mut out: ResMut<Received>) {
    for message in messages.read() {
        out.0.push((message.sender.unwrap_or(0), message.message.0));
    }
}

fn peer(uuid: u128) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
        .add_plugins((EnsemblePlugin, LoopbackTransportPlugin))
        .add_plugins(PlayerDataPlugin::<Profile>::default())
        .register_ensemble_message_type::<Ping>("Ping")
        .insert_resource(LocalMultiplayerPlayerId(uuid))
        .init_resource::<Received>()
        .add_systems(Update, collect);
    // Whatever the plugins spawn for themselves is there before any session is.
    app.update();
    app
}

/// What a world holds: its entities, and which resources it has.
struct Snapshot {
    entities: HashSet<Entity>,
    resources: HashSet<ComponentId>,
}

fn snapshot_of(world: &mut World) -> Snapshot {
    // A removed resource can leave its entity behind; what counts is whether it holds a value.
    let resources = world
        .query::<&IsResource>()
        .iter(world)
        .map(IsResource::resource_component_id)
        .collect::<Vec<_>>()
        .into_iter()
        .filter(|id| world.contains_resource_by_id(*id))
        .collect();
    let entities = world
        .query_filtered::<Entity, Without<IsResource>>()
        .iter(world)
        .collect();
    Snapshot {
        entities,
        resources,
    }
}

/// A peer's app, with a picture of its world taken before it has been in any session.
fn fresh(uuid: u128) -> (App, Snapshot) {
    let mut app = peer(uuid);
    let before = snapshot_of(app.world_mut());
    (app, before)
}

fn roster(net: &mut LoopbackNetwork, peer: PeerId) -> Vec<u128> {
    let world = net.app_mut(peer).world_mut();
    let mut list: Vec<u128> = world
        .query::<&LobbyParticipant>()
        .iter(world)
        .map(|p| p.player_uuid)
        .collect();
    list.sort();
    list
}

fn profile_of(net: &mut LoopbackNetwork, at: PeerId, uuid: u128) -> Option<String> {
    let world = net.app_mut(at).world_mut();
    world
        .query::<(&LobbyParticipant, &PlayerData<Profile>)>()
        .iter(world)
        .find(|(participant, _)| participant.player_uuid == uuid)
        .map(|(_, data)| data.0.0.clone())
}

fn set_profile(net: &mut LoopbackNetwork, peer: PeerId, name: &str) {
    let lobby = net.lobby(peer);
    let name = name.to_owned();
    let world = net.app_mut(peer).world_mut();
    world.trigger(SetPlayerData::new(lobby, Profile(name)));
    // Now, in the state the peer is in at this moment, rather than whenever the queue next runs.
    world.flush();
}

fn send(net: &mut LoopbackNetwork, from: PeerId, value: u32) {
    let lobby = net.lobby(from);
    net.app_mut(from)
        .world_mut()
        .trigger(bevy_ensemble::LobbyMessage {
            entity: lobby,
            message: Ping(value),
            send_mode: SendMode::Reliable,
        });
}

fn received(net: &LoopbackNetwork, peer: PeerId) -> Vec<(u128, u32)> {
    net.app(peer).world().resource::<Received>().0.clone()
}

/// What an entity is, in the words of the public components it might carry, for a failure
/// message. Anything else shows as its component count.
fn describe(world: &World, entity: Entity) -> String {
    let entity = world.entity(entity);
    let mut words: Vec<&str> = Vec::new();
    for (has, word) in [
        (entity.contains::<Lobby>(), "Lobby"),
        (
            entity.contains::<bevy_ensemble::PendingLobby>(),
            "PendingLobby",
        ),
        (
            entity.contains::<bevy_ensemble::LobbyClient>(),
            "LobbyClient",
        ),
        (entity.contains::<LobbyParticipant>(), "LobbyParticipant"),
        (
            entity.contains::<bevy_ensemble::HeldUntilVerified>(),
            "HeldUntilVerified",
        ),
        (entity.contains::<PlayerData<Profile>>(), "PlayerData"),
    ] {
        if has {
            words.push(word);
        }
    }
    format!(
        "{} components: {words:?}",
        entity.archetype().component_count()
    )
}

/// Nothing from the session is left: no entity that was not there before the peer's first
/// session, no resource it did not have then, and no host to trust.
///
/// Every piece of lobby-scoped state is a component on the lobby entity or on an entity related
/// to it — held packets, early handshake matches, player data waiting for its participant, the
/// last profile asked for, the host link, the participants and seats — so "no entity left over"
/// covers all of them at once, including any added later.
fn assert_fresh(net: &mut LoopbackNetwork, peer: PeerId, before: &Snapshot, what: &str) {
    let now = snapshot_of(net.app_mut(peer).world_mut());
    let world = net.app(peer).world();
    let leftover: Vec<String> = now
        .entities
        .difference(&before.entities)
        .map(|entity| describe(world, *entity))
        .collect();
    assert!(
        leftover.is_empty(),
        "{what}: entities outlived the session: {leftover:?}"
    );
    let new_resources: Vec<&ComponentId> = now.resources.difference(&before.resources).collect();
    assert!(
        new_resources.is_empty(),
        "{what}: resources that were not there before the session: {new_resources:?}"
    );
    assert!(
        world.get_resource::<HostUuid>().is_none(),
        "{what}: no host is trusted outside a session"
    );
}

fn assert_in_session(net: &mut LoopbackNetwork, peer: PeerId, host: PeerId, roster_is: &[u128]) {
    let world = net.app_mut(peer).world_mut();
    assert!(
        world
            .query_filtered::<(), With<Lobby>>()
            .iter(world)
            .next()
            .is_some(),
        "back in a lobby"
    );
    assert_eq!(roster(net, peer), roster_is);
    let marker = 1000 + net.frame() as u32;
    send(net, host, marker);
    net.run(10);
    assert!(
        received(net, peer).contains(&(net.uuid(host), marker)),
        "the new session carries messages"
    );
}

#[test]
fn a_client_that_leaves_and_joins_again_is_a_fresh_peer() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(HOST, peer(HOST));
    let (app, before) = fresh(A);
    let a = net.add_client(A, app);
    net.run(30);
    set_profile(&mut net, a, "first");
    net.run(10);
    assert_eq!(profile_of(&mut net, host, A).as_deref(), Some("first"));

    net.leave(a);
    net.run(5);
    assert_fresh(&mut net, a, &before, "after leaving");

    net.rejoin(a);
    net.run(30);
    assert_in_session(&mut net, a, host, &[HOST, A]);
    set_profile(&mut net, a, "second");
    net.run(10);
    assert_eq!(profile_of(&mut net, host, A).as_deref(), Some("second"));
    assert_eq!(profile_of(&mut net, a, A).as_deref(), Some("second"));
}

#[test]
fn a_host_that_stops_hosting_and_joins_someone_else_is_a_fresh_peer() {
    let mut net = LoopbackNetwork::new(FRAME);
    let (app, before) = fresh(HOST);
    let host = net.add_host(HOST, app);
    let a = net.add_client(A, peer(A));
    net.run(30);
    set_profile(&mut net, host, "hosting");
    net.run(10);

    // The host quits and A hosts a lobby of its own: everybody else has left.
    net.rehost(a);
    net.run(5);
    assert_fresh(
        &mut net,
        host,
        &before,
        "the old host, after its lobby went",
    );

    net.rejoin(host);
    net.run(30);
    assert_in_session(&mut net, host, a, &[HOST, A]);
}

#[test]
fn peers_that_went_through_a_host_migration_are_fresh_once_they_leave() {
    let mut net = LoopbackNetwork::new(FRAME);
    net.set_host_migration(Some(HostMigratable {
        successor_within: Duration::from_secs(4),
        reach_within: Duration::from_secs(2),
    }));
    let _host = net.add_host(HOST, peer(HOST));
    let (app_a, before_a) = fresh(A);
    let a = net.add_client(A, app_a);
    let (app_b, before_b) = fresh(B);
    let b = net.add_client(B, app_b);
    net.run(30);
    set_profile(&mut net, a, "a, before");
    set_profile(&mut net, b, "b, before");
    net.run(10);

    // The host crashes; A takes over and B follows it.
    net.migrate(a);
    net.run(60);
    assert_eq!(roster(&mut net, b), vec![A, B], "B followed A");

    // B leaves the migrated lobby, and comes back.
    net.leave(b);
    net.run(5);
    assert_fresh(&mut net, b, &before_b, "a follower, after leaving");
    net.rejoin(b);
    net.run(30);
    assert_in_session(&mut net, b, a, &[A, B]);

    // A, promoted, gives up its lobby; B hosts, and A joins it as a client.
    net.rehost(b);
    net.run(5);
    assert_fresh(
        &mut net,
        a,
        &before_a,
        "a promoted host, after its lobby went",
    );
    net.rejoin(a);
    net.run(30);
    assert_in_session(&mut net, a, b, &[A, B]);
}

// ── What a change of host must not lose ──────────────────────────────────────

#[test]
fn a_profile_edited_while_switching_hosts_reaches_the_new_host() {
    let mut net = LoopbackNetwork::new(FRAME);
    net.set_host_migration(Some(HostMigratable {
        successor_within: Duration::from_secs(4),
        reach_within: Duration::from_secs(2),
    }));
    let _host = net.add_host(HOST, peer(HOST));
    let a = net.add_client(A, peer(A));
    let b = net.add_client(B, peer(B));
    net.run(30);
    set_profile(&mut net, b, "before");
    net.run(10);
    assert_eq!(profile_of(&mut net, a, B).as_deref(), Some("before"));

    // A is named; B has not reached it yet, so there is nobody to send an edit to.
    net.migrate(a);
    let lobby = net.lobby(b);
    assert!(
        net.app(b)
            .world()
            .get::<AwaitingHost>(lobby)
            .is_some_and(|awaiting| awaiting.successor == Some(A)),
        "B is on its way to A"
    );
    set_profile(&mut net, b, "while switching");
    net.run(60);

    assert_eq!(
        profile_of(&mut net, a, B).as_deref(),
        Some("while switching"),
        "the new host holds B's edit"
    );
    assert_eq!(
        profile_of(&mut net, b, B).as_deref(),
        Some("while switching"),
        "and has told B"
    );
}

#[test]
fn a_promoted_peer_keeps_the_profile_it_last_asked_for() {
    let mut net = LoopbackNetwork::new(FRAME);
    net.set_host_migration(Some(HostMigratable {
        successor_within: Duration::from_secs(4),
        reach_within: Duration::from_secs(2),
    }));
    let _host = net.add_host(HOST, peer(HOST));
    let a = net.add_client(A, peer(A));
    let b = net.add_client(B, peer(B));
    net.run(30);

    // A asks, and the host crashes before its answer is broadcast.
    set_profile(&mut net, a, "unanswered");
    net.migrate(a);
    net.run(60);

    assert_eq!(profile_of(&mut net, a, A).as_deref(), Some("unanswered"));
    assert_eq!(profile_of(&mut net, b, A).as_deref(), Some("unanswered"));
}
