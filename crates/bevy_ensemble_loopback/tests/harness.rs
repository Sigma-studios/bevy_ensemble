//! The harness bites.
//!
//! Every test elsewhere in this workspace that runs two peers over the loopback backend assumes
//! the backend does what its docs say: one step is one frame, packets cross, reliable is reliable,
//! the knobs do what they are named for. None of that was tested. A harness that is itself broken
//! makes every failure it reports a lie in one direction and every pass a lie in the other, so
//! this is the suite to read when a netcode test does something surprising.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::prelude::*;
use bevy_ensemble::{
    EnsembleMessageRegistry, EnsemblePlugin, Host, Lobby, LobbyClient, LobbyClientPlayerUuid,
    LobbyMessage, LocalMultiplayerPlayerId, PendingLobby, ReceivedEnsembleMessage, SendMode,
};
use bevy_ensemble_loopback::{
    Link, LoopbackNetwork, LoopbackTransportPlugin, PacketFate, PeerId, SeededRng, SentPacket,
};
use serde::{Deserialize, Serialize};

const FRAME: Duration = Duration::from_micros(15_625);

#[derive(Message, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Ping(u32);

/// Everything this peer has received, in the order it was decoded.
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
        .register_ensemble_message_type::<Ping>()
        .insert_resource(LocalMultiplayerPlayerId(uuid))
        .init_resource::<Received>()
        .add_systems(Update, collect);
    app
}

/// A host and one client, both attached and connected.
fn pair() -> (LoopbackNetwork, PeerId, PeerId) {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1));
    let client = net.add_client(2, peer(2));
    (net, host, client)
}

fn send(net: &mut LoopbackNetwork, from: PeerId, to: PeerId, ping: Ping, mode: SendMode) {
    let target = if net.host() == from {
        // A host addresses a `LobbyClient`; find the one that stands for `to`.
        let uuid = net.uuid(to);
        let world = net.app_mut(from).world_mut();
        world
            .query_filtered::<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>()
            .iter(world)
            .find(|(_, client)| client.0 == uuid)
            .map(|(entity, _)| entity)
            .expect("the host has a LobbyClient for this peer")
    } else {
        net.lobby(from)
    };
    net.app_mut(from).world_mut().trigger(LobbyMessage {
        entity: target,
        message: ping,
        send_mode: mode,
    });
}

fn received(net: &LoopbackNetwork, peer: PeerId) -> Vec<(u128, u32)> {
    net.app(peer).world().resource::<Received>().0.clone()
}

fn values(net: &LoopbackNetwork, peer: PeerId) -> Vec<u32> {
    received(net, peer).into_iter().map(|(_, v)| v).collect()
}

/// The traced packets that carry a `Ping` from `from` to `to`. `bevy_ensemble` keeps its own
/// traffic on the wire (roster sync, pings), so a test about one packet must pick it out.
fn pings(net: &LoopbackNetwork, from: PeerId, to: PeerId) -> Vec<SentPacket> {
    let index = net
        .app(from)
        .world()
        .resource::<EnsembleMessageRegistry>()
        .index_of::<Ping>()
        .expect("registered")
        .to_le_bytes();
    net.trace()
        .iter()
        .filter(|p| p.from == from && p.to == to && p.bytes.starts_with(&index))
        .cloned()
        .collect()
}

fn lobby_clients(net: &mut LoopbackNetwork, host: PeerId) -> usize {
    let world = net.app_mut(host).world_mut();
    world
        .query_filtered::<(), With<LobbyClient>>()
        .iter(world)
        .count()
}

fn has<C: Component>(net: &mut LoopbackNetwork, peer: PeerId) -> bool {
    let world = net.app_mut(peer).world_mut();
    world.query_filtered::<(), With<C>>().iter(world).next().is_some()
}

#[test]
fn one_step_is_exactly_one_frame() {
    let (mut net, host, _client) = pair();
    assert_eq!(net.frame(), 0);
    net.step();
    net.step();
    assert_eq!(net.frame(), 2);
    let delta = net.app(host).world().resource::<Time>().delta();
    assert_eq!(delta, FRAME, "the app's clock must move by the frame the network models");
}

#[test]
fn packets_actually_cross_the_link() {
    let (mut net, host, client) = pair();
    send(&mut net, client, host, Ping(7), SendMode::Reliable);
    send(&mut net, host, client, Ping(9), SendMode::Reliable);
    net.run(3);
    assert_eq!(received(&net, host), vec![(2, 7)], "the host hears the client, as the client");
    assert_eq!(received(&net, client), vec![(1, 9)], "the client hears the host, as the host");
}

#[test]
fn a_perfect_link_delivers_on_the_next_frame() {
    let (mut net, host, client) = pair();
    send(&mut net, client, host, Ping(1), SendMode::Reliable);
    // The packet is in the client's outbox; the step collects it and the next step delivers it.
    net.step();
    assert!(values(&net, host).is_empty(), "nothing arrives on the frame it was sent");
    net.step();
    assert_eq!(values(&net, host), vec![1]);
}

#[test]
fn an_unreliable_packet_can_be_duplicated() {
    let (mut net, host, client) = pair();
    net.set_link(Link::perfect().with_duplicate(1.0));
    net.trace_packets(true);
    send(&mut net, client, host, Ping(3), SendMode::Unreliable);
    net.run(4);
    assert_eq!(values(&net, host), vec![3, 3], "delivered twice, one frame apart");
    assert!(matches!(pings(&net, client, host)[0].fate, PacketFate::Duplicated { .. }));
}

#[test]
fn an_unreliable_packet_can_be_reordered() {
    let (mut net, host, client) = pair();
    net.set_link(Link::perfect().with_reorder(1.0));
    send(&mut net, client, host, Ping(1), SendMode::Unreliable);
    send(&mut net, client, host, Ping(2), SendMode::Unreliable);
    net.run(4);
    assert_eq!(values(&net, host), vec![2, 1], "the second overtook the first");
}

#[test]
fn a_reliable_packet_is_never_reordered_duplicated_or_lost() {
    let (mut net, host, client) = pair();
    net.set_link(
        Link::perfect()
            .with_reorder(1.0)
            .with_duplicate(1.0)
            .with_loss(0.5)
            .with_jitter(Duration::from_millis(100)),
    );
    for value in 0..20 {
        send(&mut net, client, host, Ping(value), SendMode::Reliable);
        net.step();
    }
    net.run(60);
    assert_eq!(values(&net, host), (0..20).collect::<Vec<_>>());
}

#[test]
fn an_oversize_unreliable_packet_is_dropped_and_a_reliable_one_is_not() {
    let (mut net, host, client) = pair();
    // A `Ping` packet is 2 bytes of index plus a varint; 3 bytes is under, 2 is over.
    net.set_link(Link::perfect().with_max_message_size(2));
    net.trace_packets(true);
    send(&mut net, client, host, Ping(1), SendMode::Unreliable);
    send(&mut net, client, host, Ping(2), SendMode::Reliable);
    net.run(3);
    assert_eq!(values(&net, host), vec![2]);
    let sent = pings(&net, client, host);
    assert_eq!(sent[0].fate, PacketFate::Oversize);
    assert!(sent[1].was_delivered());
}

#[test]
fn links_can_differ_per_direction() {
    let (mut net, host, client) = pair();
    net.set_link_pair(
        client,
        host,
        Link::perfect(),
        Link::delayed(FRAME * 10),
    );
    send(&mut net, client, host, Ping(1), SendMode::Reliable);
    send(&mut net, host, client, Ping(2), SendMode::Reliable);
    net.run(3);
    assert_eq!(values(&net, host), vec![1], "the uplink is instant");
    assert!(values(&net, client).is_empty(), "the downlink is ten frames long");
    net.run(10);
    assert_eq!(values(&net, client), vec![2]);
}

#[test]
fn a_half_open_peer_receives_but_is_never_heard() {
    let (mut net, host, client) = pair();
    net.half_open(client);
    net.trace_packets(true);
    send(&mut net, client, host, Ping(1), SendMode::Reliable);
    send(&mut net, host, client, Ping(2), SendMode::Reliable);
    net.run(3);
    assert!(values(&net, host).is_empty());
    assert_eq!(values(&net, client), vec![2]);
    assert_eq!(pings(&net, client, host)[0].fate, PacketFate::Unreachable);
    assert_eq!(lobby_clients(&mut net, host), 1, "the transport has not noticed anything");

    net.reconnect(client);
    send(&mut net, client, host, Ping(3), SendMode::Reliable);
    net.run(3);
    assert_eq!(values(&net, host), vec![3]);
}

#[test]
fn a_pending_client_is_not_in_the_roster_until_promoted() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1));
    let client = net.add_pending_client(2, peer(2));

    assert!(net.is_pending(client));
    assert!(has::<PendingLobby>(&mut net, client));
    assert!(!has::<Lobby>(&mut net, client));
    assert_eq!(lobby_clients(&mut net, host), 0, "the host has not been told");

    // The data channel is up before the join is finished: the client's packets already flow.
    send(&mut net, client, host, Ping(1), SendMode::Reliable);
    net.run(3);
    assert_eq!(values(&net, host), vec![1]);

    net.promote(client);
    assert!(net.is_connected(client));
    assert!(!has::<PendingLobby>(&mut net, client));
    assert!(has::<Lobby>(&mut net, client));
    assert_eq!(lobby_clients(&mut net, host), 1);
    send(&mut net, host, client, Ping(2), SendMode::Reliable);
    net.run(3);
    assert_eq!(values(&net, client), vec![2]);
}

#[test]
fn the_local_id_can_be_withheld_and_granted_later() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1));
    net.set_local_id(host, None);
    assert!(net.app(host).world().get_resource::<LocalMultiplayerPlayerId>().is_none());
    net.set_local_id(host, Some(42));
    assert_eq!(
        net.app(host).world().resource::<LocalMultiplayerPlayerId>().0,
        42
    );
}

#[test]
fn a_peer_can_leave_and_rejoin_with_the_same_identity() {
    let (mut net, host, client) = pair();
    net.leave(client);
    assert!(net.has_left(client));
    assert!(net.try_lobby(client).is_none());
    assert!(!has::<Lobby>(&mut net, client));
    assert_eq!(lobby_clients(&mut net, host), 0);

    net.rejoin(client);
    assert!(net.is_connected(client));
    assert!(has::<Lobby>(&mut net, client));
    assert_eq!(lobby_clients(&mut net, host), 1);
    assert_eq!(net.uuid(client), 2);
    send(&mut net, client, host, Ping(5), SendMode::Reliable);
    net.run(3);
    assert_eq!(received(&net, host), vec![(2, 5)]);
}

#[test]
fn rehosting_moves_the_lobby_and_everyone_has_to_rejoin() {
    let mut net = LoopbackNetwork::new(FRAME);
    let old_host = net.add_host(1, peer(1));
    let a = net.add_client(2, peer(2));
    let b = net.add_client(3, peer(3));
    net.rehost(a);

    assert_eq!(net.host(), a);
    assert!(has::<Host>(&mut net, a));
    assert!(!has::<Host>(&mut net, old_host));
    assert!(!has::<Lobby>(&mut net, old_host));
    assert!(!has::<Lobby>(&mut net, b));
    assert!(net.has_left(old_host) && net.has_left(b));

    net.rejoin(b);
    net.rejoin(old_host);
    assert_eq!(lobby_clients(&mut net, a), 2);
    send(&mut net, b, a, Ping(8), SendMode::Reliable);
    net.run(3);
    assert_eq!(received(&net, a), vec![(3, 8)]);
}

#[test]
fn a_frozen_peer_neither_sends_nor_reads_until_it_runs_again() {
    let (mut net, host, client) = pair();
    send(&mut net, host, client, Ping(1), SendMode::Reliable);
    // Only the host runs for a while: the client's app never updates, so its inbox fills and its
    // Update systems never see the packet.
    for _ in 0..5 {
        net.step_only(&[host]);
    }
    assert!(values(&net, client).is_empty(), "a frozen peer reads nothing");
    net.step();
    assert_eq!(values(&net, client), vec![1], "and catches up when it runs");
}

#[test]
fn the_trace_records_bytes_and_fates() {
    let (mut net, host, client) = pair();
    net.trace_packets(true);
    send(&mut net, client, host, Ping(0xAB), SendMode::Reliable);
    net.run(2);
    let sent = pings(&net, client, host);
    assert_eq!(sent.len(), 1);
    let packet = &sent[0];
    assert_eq!(packet.mode, SendMode::Reliable);
    assert!(packet.contains(&[0xAB]), "the payload is on the wire");
    assert!(packet.was_delivered());
    // Counters cover everything on the link, `bevy_ensemble`'s own traffic included, and agree
    // with the trace.
    let all: Vec<&SentPacket> = net
        .trace()
        .iter()
        .filter(|p| p.from == client && p.to == host)
        .collect();
    assert_eq!(net.packets_sent(client, host), all.len() as u64);
    assert_eq!(
        net.bytes_sent(client, host),
        all.iter().map(|p| p.bytes.len() as u64).sum::<u64>()
    );
    let taken = net.take_trace().len();
    assert!(taken >= 1 && net.trace().is_empty());
}

#[test]
fn drop_next_loses_exactly_the_packets_asked_for() {
    let (mut net, host, client) = pair();
    net.trace_packets(true);
    net.drop_next(client, host, 2);
    for value in 0..4 {
        send(&mut net, client, host, Ping(value), SendMode::Unreliable);
    }
    net.run(3);
    assert_eq!(values(&net, host), vec![2, 3]);
    let sent = pings(&net, client, host);
    assert_eq!(sent[0].fate, PacketFate::Dropped);
    assert_eq!(sent[1].fate, PacketFate::Dropped);
    assert!(sent[2].was_delivered());
}

#[test]
fn corrupt_next_rewrites_one_packet_and_the_decoder_survives_it() {
    let (mut net, host, client) = pair();
    net.corrupt_next(client, host, |bytes| bytes.truncate(1));
    send(&mut net, client, host, Ping(1), SendMode::Reliable);
    send(&mut net, client, host, Ping(2), SendMode::Reliable);
    net.run(3);
    assert_eq!(values(&net, host), vec![2], "the truncated one is refused, the next is fine");
}

#[test]
fn deliver_raw_puts_bytes_in_front_of_the_decoder() {
    let (mut net, host, client) = pair();
    let mut rng = SeededRng::new(7);
    for _ in 0..200 {
        let len = rng.below(64) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
        net.deliver_raw(host, net.uuid(client), bytes);
    }
    net.run(2);
    // No panic is the assertion; anything that happened to decode as a `Ping` is fine too.
    assert!(net.frame() == 2);
}

#[test]
fn the_same_seed_replays_the_same_trace() {
    fn run(seed: u64) -> Vec<(u64, PacketFate)> {
        let (mut net, host, client) = pair();
        net.seed(seed);
        net.set_link(Link::bad_wifi().with_reorder(0.2));
        net.trace_packets(true);
        for value in 0..300 {
            send(&mut net, client, host, Ping(value), SendMode::Unreliable);
            net.step();
        }
        net.run(100);
        net.trace().iter().map(|p| (p.frame, p.fate)).collect()
    }
    let first = run(99);
    assert_eq!(first, run(99));
    assert_ne!(first, run(100), "a different seed is a different run");
    assert!(
        first.iter().any(|(_, fate)| *fate == PacketFate::Dropped),
        "bad wifi drops something in 300 packets"
    );
}

#[test]
fn a_disconnected_peer_is_gone_from_the_host_and_can_reconnect() {
    let (mut net, host, client) = pair();
    net.disconnect(client);
    assert_eq!(lobby_clients(&mut net, host), 0);
    assert!(has::<Lobby>(&mut net, client), "the client has not noticed");
    net.reconnect(client);
    assert_eq!(lobby_clients(&mut net, host), 1);
    send(&mut net, host, client, Ping(4), SendMode::Reliable);
    net.run(3);
    assert_eq!(values(&net, client), vec![4]);
}

#[test]
fn peer_rtt_reflects_each_clients_own_links() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1));
    let near = net.add_client(2, peer(2));
    let far = net.add_client(3, peer(3));
    net.set_link_pair(far, host, Link::satellite(), Link::satellite());
    net.step();
    let rtt = |net: &LoopbackNetwork, peer: PeerId| {
        net.app(peer)
            .world()
            .get::<bevy_ensemble::PeerRtt>(net.lobby(peer))
            .map(|rtt| rtt.0)
            .expect("published")
    };
    assert!(rtt(&net, near) < 0.01);
    assert!(rtt(&net, far) > 0.5);
}
