//! The trust boundary, exercised over the wire.
//!
//! Every message a peer receives says who sent it twice: once in the transport, which cannot be
//! forged, and sometimes once more in the message body, which can. These tests send the forged
//! kind and check that nothing believes it: a client cannot speak as the host, edit another
//! player, kick anyone, or have the host announce a roster change on its behalf. And the liveness
//! half: a peer that goes quiet is noticed.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::{
    BroadcastLobbyMessage, EnsembleMessageRegistry, EnsemblePlugin, EnsemblePong, Host, HostUuid,
    Lobby, LobbyBroadcastAppExt, LobbyBroadcastEnvelope, LobbyBroadcastPlugin, LobbyClient,
    LobbyLeft, LobbyLeftReason, LobbyParticipant, LocalMultiplayerPlayerId, PeerLastPong, PeerRtt,
    PeerTimeout, PlayerData, PlayerDataPlugin, ReceivedEnsembleMessage, RefusedPackets,
    RemoveLobbyParticipant, SendMode, SetPlayerData, StartHosting, SyncLobbyParticipant,
    SyncPlayerData, encode_ensemble_message,
};
use bevy_ensemble_loopback::{Link, LoopbackNetwork, LoopbackTransportPlugin, PeerId};
use serde::{Deserialize, Serialize};

const FRAME: Duration = Duration::from_micros(15_625);

#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Chat(String);

#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Name(String);

/// Chat as received, with the sender the transport reported.
#[derive(Resource, Default)]
struct Heard(Vec<(Option<u128>, String)>);

fn collect_chat(mut messages: MessageReader<ReceivedEnsembleMessage<Chat>>, mut out: ResMut<Heard>) {
    for message in messages.read() {
        out.0.push((message.sender, message.message.0.clone()));
    }
}

#[derive(Resource, Default)]
struct Departures(Vec<LobbyLeftReason>);

fn collect_left(mut messages: MessageReader<LobbyLeft>, mut out: ResMut<Departures>) {
    for message in messages.read() {
        out.0.push(message.reason.clone());
    }
}

fn peer(uuid: u128) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
        .add_plugins((EnsemblePlugin, LoopbackTransportPlugin, LobbyBroadcastPlugin))
        .add_plugins(PlayerDataPlugin::<Name>::default())
        .register_broadcast_message::<Chat>("Chat")
        .insert_resource(LocalMultiplayerPlayerId(uuid))
        .init_resource::<Heard>()
        .init_resource::<Departures>()
        .add_systems(Update, (collect_chat, collect_left));
    app
}

/// A host and two clients, run long enough for the roster to settle.
fn trio() -> (LoopbackNetwork, PeerId, PeerId, PeerId) {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1));
    let a = net.add_client(2, peer(2));
    let b = net.add_client(3, peer(3));
    net.run(20);
    (net, host, a, b)
}

fn encode<T: bevy_ensemble::EnsembleMessage>(net: &LoopbackNetwork, at: PeerId, message: &T) -> Vec<u8> {
    let registry = net.app(at).world().resource::<EnsembleMessageRegistry>();
    encode_ensemble_message(registry, message)
}

fn heard(net: &LoopbackNetwork, peer: PeerId) -> Vec<(Option<u128>, String)> {
    net.app(peer).world().resource::<Heard>().0.clone()
}

fn refused(net: &LoopbackNetwork, peer: PeerId) -> u64 {
    net.app(peer)
        .world()
        .get_resource::<RefusedPackets>()
        .map_or(0, RefusedPackets::total)
}

/// Packets `peer` is holding from `sender`, unread, because `sender` never verified a protocol
/// with it.
fn held_from(net: &LoopbackNetwork, peer: PeerId, sender: u128) -> usize {
    net.app(peer)
        .world()
        .resource::<bevy_ensemble::HeldUntilVerified>()
        .held_for(sender)
}

fn participants(net: &mut LoopbackNetwork, peer: PeerId) -> Vec<(u128, bool)> {
    let world = net.app_mut(peer).world_mut();
    let mut list: Vec<(u128, bool)> = world
        .query::<&LobbyParticipant>()
        .iter(world)
        .map(|p| (p.player_uuid, p.is_host))
        .collect();
    list.sort();
    list
}

fn has_lobby(net: &mut LoopbackNetwork, peer: PeerId) -> bool {
    let world = net.app_mut(peer).world_mut();
    world.query_filtered::<(), With<Lobby>>().iter(world).next().is_some()
}

fn lobby_clients(net: &mut LoopbackNetwork, host: PeerId) -> usize {
    let world = net.app_mut(host).world_mut();
    world.query_filtered::<(), With<LobbyClient>>().iter(world).count()
}

fn name_of(net: &mut LoopbackNetwork, peer: PeerId, uuid: u128) -> Option<String> {
    let world = net.app_mut(peer).world_mut();
    world
        .query::<(&LobbyParticipant, &PlayerData<Name>)>()
        .iter(world)
        .find(|(p, _)| p.player_uuid == uuid)
        .map(|(_, data)| data.0.0.clone())
}

#[test]
fn a_relayed_envelope_carries_the_transport_sender_not_the_claimed_one() {
    let (mut net, host, a, b) = trio();
    // B forges an envelope that claims the host said it.
    let payload = encode(&net, b, &Chat("I am the host".into()));
    let forged = LobbyBroadcastEnvelope {
        sender: 1,
        payload,
        send_mode: SendMode::Reliable,
    };
    let bytes = encode(&net, b, &forged);
    net.deliver_raw(host, 3, bytes);
    net.run(6);

    assert_eq!(heard(&net, host), vec![(Some(3), "I am the host".into())], "the host knows better");
    assert_eq!(heard(&net, a), vec![(Some(3), "I am the host".into())], "and so does A");
    assert_eq!(heard(&net, b), vec![(Some(3), "I am the host".into())]);
}

#[test]
fn a_broadcast_from_a_client_is_delivered_everywhere_as_that_client() {
    let (mut net, host, a, b) = trio();
    let lobby = net.lobby(a);
    net.app_mut(a)
        .world_mut()
        .trigger(BroadcastLobbyMessage::new(lobby, Chat("hello".into())));
    net.run(6);
    for peer in [host, a, b] {
        assert_eq!(heard(&net, peer), vec![(Some(2), "hello".into())]);
    }
}

#[test]
fn a_kick_wrapped_in_an_envelope_does_not_kick() {
    let (mut net, host, a, b) = trio();
    let payload = encode(&net, b, &RemoveLobbyParticipant { player_uuid: 2 });
    let envelope = LobbyBroadcastEnvelope {
        sender: 3,
        payload,
        send_mode: SendMode::Reliable,
    };
    let bytes = encode(&net, b, &envelope);
    net.deliver_raw(host, 3, bytes);
    net.run(10);

    assert!(has_lobby(&mut net, a), "A is still in the session");
    assert_eq!(lobby_clients(&mut net, host), 2);
    assert!(refused(&net, host) > 0, "the host counted the refusal");
    assert!(heard(&net, a).is_empty());
}

#[test]
fn a_nested_envelope_does_not_amplify() {
    let (mut net, host, _a, b) = trio();
    net.trace_packets(true);
    let inner = encode(&net, b, &Chat("deep".into()));
    let mut envelope = LobbyBroadcastEnvelope {
        sender: 3,
        payload: inner,
        send_mode: SendMode::Reliable,
    };
    for _ in 0..4 {
        let bytes = encode(&net, b, &envelope);
        envelope = LobbyBroadcastEnvelope {
            sender: 3,
            payload: bytes,
            send_mode: SendMode::Reliable,
        };
    }
    let bytes = encode(&net, b, &envelope);
    net.deliver_raw(host, 3, bytes);
    net.run(10);

    let relayed: Vec<_> = net
        .trace()
        .iter()
        .filter(|p| p.from == host && p.mode == SendMode::Reliable && p.bytes.len() > 40)
        .collect();
    assert!(
        relayed.is_empty(),
        "an envelope wrapping an envelope is a control message and is not relayed; {} were",
        relayed.len()
    );
    assert!(refused(&net, host) > 0);
}

#[test]
fn a_roster_sync_from_a_non_host_is_ignored() {
    let (mut net, _host, a, b) = trio();
    let before = participants(&mut net, a);
    let bytes = encode(
        &net,
        b,
        &SyncLobbyParticipant {
            player_uuid: 99,
            is_host: true,
        },
    );
    net.deliver_raw(a, 3, bytes);
    net.run(4);
    assert_eq!(participants(&mut net, a), before, "no phantom host appeared");
    // B never verified a protocol with A — B is not A's host — so A holds B's bytes unread.
    assert_eq!(held_from(&net, a, 3), 1);
}

#[test]
fn a_removal_from_a_non_host_does_not_remove_anyone() {
    let (mut net, _host, a, b) = trio();
    let bytes = encode(&net, b, &RemoveLobbyParticipant { player_uuid: 2 });
    net.deliver_raw(a, 3, bytes);
    net.run(4);
    assert!(has_lobby(&mut net, a), "A was told to leave by somebody who may not say so");
    assert_eq!(held_from(&net, a, 3), 1, "and never read what B said");
}

#[test]
fn a_client_cannot_overwrite_another_players_data() {
    let (mut net, host, a, b) = trio();
    let lobby = net.lobby(a);
    net.app_mut(a)
        .world_mut()
        .trigger(SetPlayerData::new(lobby, Name("Alice".into())));
    net.run(10);
    assert_eq!(name_of(&mut net, host, 2).as_deref(), Some("Alice"));
    assert_eq!(name_of(&mut net, b, 2).as_deref(), Some("Alice"));

    // B tries to rename A.
    let bytes = encode(
        &net,
        b,
        &SyncPlayerData {
            player_uuid: 2,
            data: Name("Mallory".into()),
        },
    );
    net.deliver_raw(host, 3, bytes);
    net.run(10);
    assert_eq!(name_of(&mut net, host, 2).as_deref(), Some("Alice"));
    assert_eq!(name_of(&mut net, b, 2).as_deref(), Some("Alice"));
    assert_eq!(name_of(&mut net, a, 2).as_deref(), Some("Alice"));
}

#[test]
fn player_data_on_a_client_is_taken_only_from_the_host() {
    let (mut net, _host, a, b) = trio();
    let bytes = encode(
        &net,
        b,
        &SyncPlayerData {
            player_uuid: 3,
            data: Name("Direct".into()),
        },
    );
    net.deliver_raw(a, 3, bytes);
    net.run(4);
    assert_eq!(name_of(&mut net, a, 3), None);
    assert_eq!(held_from(&net, a, 3), 1);
}

#[test]
fn an_unsolicited_pong_is_ignored_and_an_absurd_one_cannot_poison_the_estimate() {
    let (mut net, host, a, _b) = trio();
    let rtt = |net: &mut LoopbackNetwork| -> Option<f64> {
        let world = net.app_mut(host).world_mut();
        world
            .query_filtered::<&PeerRtt, With<LobbyClient>>()
            .iter(world)
            .next()
            .map(|r| r.0)
    };
    // Real pings have been flowing since the join: the estimate exists and is finite.
    net.run(64);
    let measured = rtt(&mut net).expect("pongs arrived");
    assert!(measured.is_finite() && measured >= 0.0);

    // A flood of garbage: unknown sequence numbers, absurd dwell. (The unit tests in
    // `ping.rs` pin that an unsolicited pong is not a sample at all; this pins that a flood
    // of them leaves the estimate a number.)
    for seq in 0..200u32 {
        let bytes = encode(
            &net,
            a,
            &EnsemblePong {
                seq: seq.wrapping_mul(7919),
                reliable: false,
                dwell_micros: u32::MAX,
            },
        );
        net.deliver_raw(host, 2, bytes);
    }
    net.run(2);
    let after = rtt(&mut net).expect("still there");
    assert!(after.is_finite() && after >= 0.0);
}

#[test]
fn a_peer_that_stops_answering_pings_is_removed_after_the_timeout() {
    let (mut net, host, a, _b) = trio();
    for peer in [host, a] {
        net.app_mut(peer)
            .world_mut()
            .insert_resource(PeerTimeout(Some(Duration::from_secs(1))));
    }
    net.run(64);
    assert_eq!(lobby_clients(&mut net, host), 2);

    net.half_open(a);
    // 64 frames is one second; give the timeout a little more than that.
    net.run(90);
    assert_eq!(lobby_clients(&mut net, host), 1, "the host dropped the silent client");
    assert!(!has_lobby(&mut net, a), "and the client, hearing no pong, left");
    assert_eq!(
        net.app(a).world().resource::<Departures>().0,
        vec![LobbyLeftReason::PeerTimeout]
    );
    assert!(net.app(a).world().get_resource::<LocalMultiplayerPlayerId>().is_none());
}

#[test]
fn a_live_peer_is_never_dropped_under_satellite_jitter() {
    let (mut net, host, _a, _b) = trio();
    net.set_link(Link::satellite());
    net.run(64 * 12);
    assert_eq!(lobby_clients(&mut net, host), 2);
    let world = net.app_mut(host).world_mut();
    let worst = world
        .query_filtered::<&PeerLastPong, With<LobbyClient>>()
        .iter(world)
        .map(|p| p.0)
        .fold(0.0, f64::max);
    assert!(worst < 2.0, "the longest silence was {worst}s");
}

#[test]
fn liveness_is_armed_from_the_moment_a_peer_is_known() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1));
    let a = net.add_client(2, peer(2));
    net.app_mut(host)
        .world_mut()
        .insert_resource(PeerTimeout(Some(Duration::from_secs(1))));
    // A never answers: half-open from the start.
    net.half_open(a);
    net.run(90);
    assert_eq!(lobby_clients(&mut net, host), 0, "a peer that never pongs still times out");
}

#[test]
fn no_identity_is_published_until_the_backend_has_one() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
        .add_plugins((EnsemblePlugin, LoopbackTransportPlugin));
    app.world_mut().write_message(StartHosting);
    app.update();
    app.update();
    assert!(
        app.world().get_resource::<LocalMultiplayerPlayerId>().is_none(),
        "no placeholder"
    );
    let world = app.world_mut();
    assert!(world.query_filtered::<(), With<Host>>().iter(world).next().is_some());
}

#[test]
fn the_host_participant_appears_once_the_identity_is_known() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1));
    net.set_local_id(host, None);
    net.run(3);
    assert!(participants(&mut net, host).is_empty());
    net.set_local_id(host, Some(1));
    net.run(3);
    assert_eq!(participants(&mut net, host), vec![(1, true)]);
    assert_eq!(net.app(host).world().resource::<HostUuid>().0, 1);
}

#[test]
fn a_broadcast_with_no_identity_is_not_sent_as_nobody() {
    let mut net = LoopbackNetwork::new(FRAME);
    let host = net.add_host(1, peer(1));
    let a = net.add_client(2, peer(2));
    net.run(5);
    net.set_local_id(a, None);
    let lobby = net.lobby(a);
    net.app_mut(a)
        .world_mut()
        .trigger(BroadcastLobbyMessage::new(lobby, Chat("who".into())));
    net.run(5);
    assert!(heard(&net, host).is_empty());
}

#[test]
fn a_kicked_client_learns_why() {
    let (mut net, host, a, _b) = trio();
    let world = net.app_mut(host).world_mut();
    let entity = world
        .query_filtered::<(Entity, &bevy_ensemble::LobbyClientPlayerUuid), With<LobbyClient>>()
        .iter(world)
        .find(|(_, uuid)| uuid.0 == 2)
        .map(|(entity, _)| entity)
        .expect("A is a client");
    world.despawn(entity);
    net.run(6);
    assert!(!has_lobby(&mut net, a));
    assert_eq!(
        net.app(a).world().resource::<Departures>().0,
        vec![LobbyLeftReason::Kicked]
    );
}

// ── Liveness grace ───────────────────────────────────────────────────────────

/// A transport that knows why a peer is silent (an ICE restart in flight) can hold the
/// liveness check off for that long, and no longer: the grace adds to the timeout.
#[test]
fn a_peer_under_liveness_grace_outlives_the_timeout_but_not_the_grace() {
    use bevy_ensemble::LivenessGrace;

    let (mut net, host, _a, _b) = trio();
    net.app_mut(host)
        .insert_resource(PeerTimeout(Some(Duration::from_millis(100))));
    let seat = net
        .app_mut(host)
        .world_mut()
        .query_filtered::<Entity, With<LobbyClient>>()
        .iter(net.app(host).world())
        .next()
        .unwrap();
    net.app_mut(host).world_mut().entity_mut(seat).insert((
        LivenessGrace {
            extra: Duration::from_millis(300),
        },
        // Silence: as if the client's pongs stopped a quarter second ago.
        PeerLastPong(0.25),
    ));
    net.step_only(&[host]);
    assert!(
        net.app(host).world().get_entity(seat).is_ok(),
        "past the timeout but inside the grace, the seat stands"
    );

    net.app_mut(host)
        .world_mut()
        .entity_mut(seat)
        .insert(PeerLastPong(0.45));
    net.step_only(&[host]);
    assert!(
        net.app(host).world().get_entity(seat).is_err(),
        "past timeout plus grace, the peer is gone like any other"
    );
}
