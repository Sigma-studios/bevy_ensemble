//! Everybody's ping and route, as the host measures them, on every peer's roster.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::{
    EnsemblePlugin, LobbyClient, LobbyClientPlayerUuid, LobbyParticipant, LocalMultiplayerPlayerId,
    ParticipantLink, PeerRoute,
};
use bevy_ensemble_loopback::{LoopbackNetwork, LoopbackTransportPlugin, PeerId};

const FRAME: Duration = Duration::from_micros(15_625);

const HOST: u128 = 1;
const A: u128 = 2;
const B: u128 = 3;

fn peer(uuid: u128) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
        .add_plugins((EnsemblePlugin, LoopbackTransportPlugin))
        .insert_resource(LocalMultiplayerPlayerId(uuid));
    app
}

/// Every participant's link as this peer has it, by player.
fn links(net: &mut LoopbackNetwork, peer: PeerId) -> Vec<(u128, Option<ParticipantLink>)> {
    let world = net.app_mut(peer).world_mut();
    let mut links: Vec<_> = world
        .query::<(&LobbyParticipant, Option<&ParticipantLink>)>()
        .iter(world)
        .map(|(participant, link)| (participant.player_uuid, link.copied()))
        .collect();
    links.sort_by_key(|(uuid, _)| *uuid);
    links
}

/// Give the host's connection to `client` a route, as the WebRTC backend would once ICE settles.
fn set_route(net: &mut LoopbackNetwork, host: PeerId, client: u128, route: PeerRoute) {
    let world = net.app_mut(host).world_mut();
    let entity = world
        .query_filtered::<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>()
        .iter(world)
        .find(|(_, uuid)| uuid.0 == client)
        .map(|(entity, _)| entity)
        .expect("the host has a LobbyClient for this player");
    world.entity_mut(entity).insert(route);
}

#[test]
fn every_peer_sees_every_players_ping_and_route() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(HOST, peer(HOST));
    let a = net.add_client(A, peer(A));
    let b = net.add_client(B, peer(B));
    net.run(4);
    set_route(&mut net, host, A, PeerRoute::Direct);
    set_route(&mut net, host, B, PeerRoute::Relayed);

    // The pings measure a round trip within a few frames, and the report goes out once a second:
    // the first one can leave before the routes above were set, so wait for one that has them.
    let settled = |net: &mut LoopbackNetwork| {
        let theirs = links(net, host);
        let complete = theirs.len() == 3
            && theirs.iter().all(|(uuid, link)| match link {
                Some(link) => *uuid != HOST && link.route.is_some(),
                None => *uuid == HOST,
            });
        complete && links(net, a) == theirs && links(net, b) == theirs
    };
    let mut agreed = false;
    for _ in 0..(64 * 5) {
        net.step();
        if settled(&mut net) {
            agreed = true;
            break;
        }
    }
    assert!(
        agreed,
        "the peers never agreed on the host's report: host {:?}, a {:?}, b {:?}",
        links(&mut net, host),
        links(&mut net, a),
        links(&mut net, b)
    );

    let seen = links(&mut net, a);
    let route = |uuid| {
        seen.iter()
            .find(|(player, _)| *player == uuid)
            .and_then(|(_, link)| link.and_then(|link| link.route))
    };
    assert_eq!(route(A), Some(PeerRoute::Direct));
    assert_eq!(route(B), Some(PeerRoute::Relayed));
    assert!(
        seen.iter()
            .filter_map(|(_, link)| *link)
            .all(|link| link.rtt.is_finite() && link.rtt >= 0.0)
    );
    assert!(
        seen.iter().filter_map(|(_, link)| *link).all(|link| {
            link.wire_rtt.is_finite() && link.wire_rtt >= 0.0 && link.wire_rtt <= link.rtt
        }),
        "the network's part of a round trip is never more than the round trip"
    );
}
