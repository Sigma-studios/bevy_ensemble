//! Protocol v2 over the wire: named types, the join handshake, framing, and both-channel pings.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::{
    EnsembleAppExt, EnsembleMessageRegistry, EnsemblePlugin, HandshakeVerified, Lobby,
    LobbyClient, LobbyJoinFailed, LobbyLeft, LobbyLeftReason, LobbyMessage,
    LocalMultiplayerPlayerId, PeerReliableRtt, PeerRtt, ReceivedEnsembleMessage, SendMode,
    unframe_packet,
};
use bevy_ensemble_loopback::{LoopbackNetwork, LoopbackTransportPlugin, PeerId};
use serde::{Deserialize, Serialize};

const FRAME: Duration = Duration::from_micros(15_625);

#[derive(Message, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Alpha(u32);

#[derive(Message, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Beta(u32);

#[derive(Resource, Default)]
struct Received(Vec<u32>);

fn collect(
    mut alphas: MessageReader<ReceivedEnsembleMessage<Alpha>>,
    mut betas: MessageReader<ReceivedEnsembleMessage<Beta>>,
    mut out: ResMut<Received>,
) {
    for message in alphas.read() {
        out.0.push(message.message.0);
    }
    for message in betas.read() {
        out.0.push(1000 + message.message.0);
    }
}

#[derive(Resource, Default)]
struct Departures(Vec<LobbyLeftReason>);

fn collect_left(mut messages: MessageReader<LobbyLeft>, mut out: ResMut<Departures>) {
    for message in messages.read() {
        out.0.push(message.reason.clone());
    }
}

#[derive(Resource, Default)]
struct JoinFailures(Vec<String>);

fn collect_failed(mut messages: MessageReader<LobbyJoinFailed>, mut out: ResMut<JoinFailures>) {
    for message in messages.read() {
        out.0.push(message.reason.clone());
    }
}

/// A peer that registers `Alpha` then `Beta`, or the other way round.
fn peer(uuid: u128, beta_first: bool) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
        .add_plugins((EnsemblePlugin, LoopbackTransportPlugin))
        .insert_resource(LocalMultiplayerPlayerId(uuid))
        .init_resource::<Received>()
        .init_resource::<Departures>()
        .init_resource::<JoinFailures>()
        .add_systems(Update, (collect, collect_left, collect_failed));
    if beta_first {
        app.register_ensemble_message_type::<Beta>("Beta")
            .register_ensemble_message_type::<Alpha>("Alpha");
    } else {
        app.register_ensemble_message_type::<Alpha>("Alpha")
            .register_ensemble_message_type::<Beta>("Beta");
    }
    app
}

fn send<T: bevy_ensemble::EnsembleMessage>(
    net: &mut LoopbackNetwork,
    from: PeerId,
    message: T,
    mode: SendMode,
) {
    let lobby = net.lobby(from);
    net.app_mut(from).world_mut().trigger(LobbyMessage {
        entity: lobby,
        message,
        send_mode: mode,
    });
}

fn received(net: &LoopbackNetwork, peer: PeerId) -> Vec<u32> {
    net.app(peer).world().resource::<Received>().0.clone()
}

fn has<C: Component>(net: &mut LoopbackNetwork, peer: PeerId) -> bool {
    let world = net.app_mut(peer).world_mut();
    world.query_filtered::<(), With<C>>().iter(world).next().is_some()
}

fn lobby_clients(net: &mut LoopbackNetwork, host: PeerId) -> usize {
    let world = net.app_mut(host).world_mut();
    world.query_filtered::<(), With<LobbyClient>>().iter(world).count()
}

fn verified_clients(net: &mut LoopbackNetwork, host: PeerId) -> usize {
    let world = net.app_mut(host).world_mut();
    world
        .query_filtered::<(), (With<LobbyClient>, With<HandshakeVerified>)>()
        .iter(world)
        .count()
}

#[test]
fn wire_indices_do_not_depend_on_registration_order() {
    let a = peer(1, false);
    let b = peer(2, true);
    let index = |app: &App| {
        let registry = app.world().resource::<EnsembleMessageRegistry>();
        (
            registry.index_of::<Alpha>().unwrap(),
            registry.index_of::<Beta>().unwrap(),
            registry.wire_hash(),
        )
    };
    assert_eq!(index(&a), index(&b));
    assert_eq!(
        a.world().resource::<EnsembleMessageRegistry>().wire_names(),
        b.world().resource::<EnsembleMessageRegistry>().wire_names()
    );
}

#[test]
fn peers_that_registered_in_different_orders_still_understand_each_other() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1, false));
    let client = net.add_client(2, peer(2, true));
    send(&mut net, client, Alpha(7), SendMode::Reliable);
    send(&mut net, client, Beta(8), SendMode::Reliable);
    net.run(4);
    assert_eq!(received(&net, host), vec![7, 1008]);
}

#[test]
fn matching_peers_are_verified_at_the_join() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1, false));
    let client = net.add_client(2, peer(2, true));
    net.run(6);
    assert!(has::<HandshakeVerified>(&mut net, client), "the client verified its host");
    assert_eq!(verified_clients(&mut net, host), 1, "the host verified its client");
}

#[test]
fn a_peer_with_a_different_registry_is_refused_at_the_join_and_told_why() {
    #[derive(Message, Clone, Copy, Debug, Serialize, Deserialize)]
    struct Gamma;

    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1, false));
    let mut extra = peer(2, false);
    extra.register_ensemble_message_type::<Gamma>("Gamma");
    let client = net.add_client(2, extra);
    net.run(8);

    assert!(!has::<Lobby>(&mut net, client), "the client left");
    assert_eq!(lobby_clients(&mut net, host), 0, "the host dropped it");
    let departures = &net.app(client).world().resource::<Departures>().0;
    assert!(
        matches!(&departures[..], [LobbyLeftReason::ProtocolMismatch(reason)] if reason.contains("Gamma")),
        "the reason names the registration that differs: {departures:?}"
    );
    let failures = &net.app(host).world().resource::<JoinFailures>().0;
    assert!(
        failures.iter().any(|f| f.contains("Gamma")),
        "the host learns which name differed: {failures:?}"
    );
}

#[test]
fn two_messages_to_one_peer_in_one_frame_share_a_packet() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1, false));
    let client = net.add_client(2, peer(2, false));
    net.run(4);
    net.trace_packets(true);
    send(&mut net, client, Alpha(1), SendMode::Reliable);
    send(&mut net, client, Beta(2), SendMode::Reliable);
    net.run(3);
    let reliable: Vec<_> = net
        .trace()
        .iter()
        .filter(|p| p.from == client && p.to == host && p.mode == SendMode::Reliable)
        .collect();
    assert_eq!(reliable.len(), 1, "one reliable datagram for the frame");
    let inner = unframe_packet(&reliable[0].bytes).expect("a frame");
    assert!(inner.len() >= 2, "both messages (and any control traffic) rode in it");
    assert_eq!(received(&net, host), vec![1, 1002]);
}

#[test]
fn reliable_and_unreliable_never_share_a_frame() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1, false));
    let client = net.add_client(2, peer(2, false));
    net.run(4);
    net.trace_packets(true);
    send(&mut net, client, Alpha(1), SendMode::Reliable);
    send(&mut net, client, Beta(2), SendMode::Unreliable);
    net.run(3);
    for packet in net.trace().iter().filter(|p| p.from == client && p.to == host) {
        if let Some(inner) = unframe_packet(&packet.bytes) {
            // Every message in a frame was sent on the frame's channel: an `Alpha` never
            // appears in an unreliable frame and a `Beta` never in a reliable one.
            let registry = net.app(client).world().resource::<EnsembleMessageRegistry>();
            let alpha = registry.index_of::<Alpha>().unwrap().to_le_bytes();
            let beta = registry.index_of::<Beta>().unwrap().to_le_bytes();
            for message in inner {
                if message.starts_with(&alpha) {
                    assert!(packet.mode.is_reliable());
                }
                if message.starts_with(&beta) {
                    assert!(!packet.mode.is_reliable());
                }
            }
        }
    }
    let mut got = received(&net, host);
    got.sort();
    assert_eq!(got, vec![1, 1002]);
}

#[test]
fn a_no_delay_message_is_not_held_for_the_batch() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1, false));
    let client = net.add_client(2, peer(2, false));
    net.run(4);
    net.trace_packets(true);
    send(&mut net, client, Alpha(1), SendMode::ReliableNoDelay);
    send(&mut net, client, Alpha(2), SendMode::ReliableNoDelay);
    net.run(3);
    let registry = net.app(client).world().resource::<EnsembleMessageRegistry>();
    let alpha = registry.index_of::<Alpha>().unwrap().to_le_bytes();
    let alone: Vec<_> = net
        .trace()
        .iter()
        .filter(|p| p.from == client && p.to == host && p.bytes.starts_with(&alpha))
        .collect();
    assert_eq!(alone.len(), 2, "each went as its own unframed packet");
}

#[test]
fn a_frame_round_trips_through_decode_in_order() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1, false));
    let client = net.add_client(2, peer(2, false));
    net.run(4);
    for value in 0..50 {
        send(&mut net, client, Alpha(value), SendMode::Reliable);
    }
    net.run(3);
    assert_eq!(received(&net, host), (0..50).collect::<Vec<_>>());
}

#[test]
fn both_channels_are_measured() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1, false));
    let _client = net.add_client(2, peer(2, false));
    net.run(80);
    let world = net.app_mut(host).world_mut();
    let (rtt, reliable) = world
        .query_filtered::<(&PeerRtt, &PeerReliableRtt), With<LobbyClient>>()
        .iter(world)
        .next()
        .map(|(a, b)| (a.0, b.0))
        .expect("both estimates exist");
    assert!(rtt.is_finite() && reliable.is_finite());
}
