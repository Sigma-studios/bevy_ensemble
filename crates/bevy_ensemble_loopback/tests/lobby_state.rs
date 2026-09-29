//! What a peer holds about its lobby, when the messages that change it arrive together, and when
//! the session they belonged to is over.
//!
//! The roster is changed by more than one wire type, and a session that is over can still have
//! packets on their way. Both used to leave something behind that nothing would ever take away:
//! a participant on a client's roster that the host had long removed, a packet from an abandoned
//! join read into the next one.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::{
    EnsembleAppExt, EnsembleMessageRegistry, EnsemblePlugin, HeldUntilVerified, Lobby,
    LobbyParticipant, LocalMultiplayerPlayerId, ReceivedEnsembleMessage, encode_ensemble_message,
};
use bevy_ensemble_loopback::{LoopbackNetwork, LoopbackTransportPlugin, PeerId};
use serde::{Deserialize, Serialize};

const FRAME: Duration = Duration::from_micros(15_625);

#[derive(Message, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Ping(u32);

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
        .register_ensemble_message_type::<Ping>("Ping")
        .insert_resource(LocalMultiplayerPlayerId(uuid))
        .init_resource::<Received>()
        .add_systems(Update, collect);
    app
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

fn held_from(net: &mut LoopbackNetwork, peer: PeerId, sender: u128) -> usize {
    let world = net.app_mut(peer).world_mut();
    world
        .query::<&HeldUntilVerified>()
        .iter(world)
        .map(|held| held.held_for(sender))
        .sum()
}

// ── One ordered stream for the roster ────────────────────────────────────────

#[test]
fn a_participant_added_and_removed_in_one_frame_is_not_on_the_roster() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1));
    let a = net.add_client(2, peer(2));
    net.run(20);
    assert_eq!(roster(&mut net, a), vec![1, 2], "A's roster settled");

    // X joins while A is frozen for a few frames (a hitch, a backgrounded tab): the host
    // announces X to A, and A does not read it yet.
    let x = net.add_client(3, peer(3));
    for _ in 0..10 {
        net.step_only(&[host, x]);
    }
    assert_eq!(roster(&mut net, host), vec![1, 2, 3], "the host seated X");

    // X drops before A has read anything: the host removes it and tells A.
    net.disconnect(x);
    for _ in 0..5 {
        net.step_only(&[host]);
    }
    assert_eq!(
        roster(&mut net, host),
        vec![1, 2],
        "the host no longer lists X"
    );

    // A reads the announcement and the removal in one frame, in the order they were sent.
    net.run(120);
    assert_eq!(
        roster(&mut net, a),
        vec![1, 2],
        "X came and went; A must not keep it as a participant nothing will ever remove"
    );
}

#[test]
fn a_participant_added_and_removed_in_separate_frames_is_not_on_the_roster() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1));
    let a = net.add_client(2, peer(2));
    net.run(20);
    let x = net.add_client(3, peer(3));
    net.run(10);
    assert_eq!(roster(&mut net, a), vec![1, 2, 3], "A saw X join");
    net.disconnect(x);
    net.run(120);
    assert_eq!(roster(&mut net, host), vec![1, 2]);
    assert_eq!(roster(&mut net, a), vec![1, 2]);
}

// ── Nothing from one session in the next ─────────────────────────────────────

#[test]
fn packets_held_by_a_join_that_died_pending_are_not_read_in_the_next_session() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1));
    let client = net.add_pending_client(2, peer(2));
    net.run(2);

    // Something the host sent during the first attempt, before the protocol was verified: held.
    let stale = {
        let registry = net.app(host).world().resource::<EnsembleMessageRegistry>();
        encode_ensemble_message(registry, &Ping(99))
    };
    net.deliver_raw(client, 1, stale);
    net.run(2);
    assert_eq!(
        held_from(&mut net, client, 1),
        1,
        "the packet is held for the unverified host"
    );

    // The attempt dies while still pending (cancelled, timed out, refused): the `PendingLobby`
    // is despawned without ever having been a `Lobby`.
    net.leave(client);
    net.run(2);
    assert_eq!(
        held_from(&mut net, client, 1),
        0,
        "what was held went with the lobby it was held for"
    );

    // Join the same host again.
    net.rejoin(client);
    net.run(20);
    let rejoined = {
        let world = net.app_mut(client).world_mut();
        world
            .query_filtered::<(), With<Lobby>>()
            .iter(world)
            .next()
            .is_some()
    };
    assert!(rejoined, "the client is back in a lobby with the same host");
    let got = net.app(client).world().resource::<Received>().0.clone();
    assert!(
        !got.contains(&(1, 99)),
        "Ping(99) belonged to the abandoned attempt, and the new session read it: {got:?}"
    );
}

#[test]
fn a_packet_that_arrives_with_no_lobby_is_dropped_not_held() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1));
    let client = net.add_client(2, peer(2));
    net.run(20);
    net.leave(client);
    net.run(2);

    let late = {
        let registry = net.app(host).world().resource::<EnsembleMessageRegistry>();
        encode_ensemble_message(registry, &Ping(7))
    };
    net.deliver_raw(client, 1, late);
    net.run(2);
    assert_eq!(held_from(&mut net, client, 1), 0);

    net.rejoin(client);
    net.run(20);
    let got = net.app(client).world().resource::<Received>().0.clone();
    assert!(
        !got.contains(&(1, 7)),
        "read into the next session: {got:?}"
    );
}
