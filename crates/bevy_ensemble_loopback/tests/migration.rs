//! A lobby outliving its host, over the wire.
//!
//! The arbiter — the signalling server, Steam — is the test here: `lose_host` is the host becoming
//! unreachable, `name_host` is the decision about who replaces it, and everything between the two
//! is what the core does on its own. Every peer is a real app decoding real packets.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ensemble::{
    AwaitingHost, AwaitingSeat, BroadcastLobbyMessage, CloseLobby, EnsembleAppExt,
    EnsembleMessageRegistry, EnsemblePlugin, HandshakeVerified, HeldUntilVerified, Host,
    HostChanged, HostLossCause, HostMigratable, HostMigrationTimeouts, HostUuid, Lobby,
    LobbyBroadcastAppExt, LobbyBroadcastPlugin, LobbyClient, LobbyClientPlayerUuid, LobbyLeft,
    LobbyLeftReason, LobbyMessage, LobbyParticipant, LocalMultiplayerPlayerId, PeerLastPong,
    PeerTimeout, PlayerData, PlayerDataPlugin, ReceivedEnsembleMessage, SetPlayerData,
    VerifiedHost, encode_ensemble_message,
};
use bevy_ensemble_loopback::{HostDeparture, LoopbackNetwork, LoopbackTransportPlugin, PeerId};
use serde::{Deserialize, Serialize};

const FRAME: Duration = Duration::from_micros(15_625);

const HOST: u128 = 1;
const A: u128 = 2;
const B: u128 = 3;
const C: u128 = 4;

/// Waits long enough that nothing in a test runs out by accident.
const MIGRATION: HostMigratable = HostMigratable {
    successor_within: Duration::from_secs(4),
    reach_within: Duration::from_secs(2),
};

#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Chat(String);

#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Note(String);

#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Name(String);

#[derive(Resource, Default)]
struct Heard(Vec<(Option<u128>, String)>);

#[derive(Resource, Default)]
struct Departures(Vec<LobbyLeftReason>);

#[derive(Resource, Default)]
struct Changes(Vec<(u128, u128, bool)>);

fn collect(
    mut chat: MessageReader<ReceivedEnsembleMessage<Chat>>,
    mut notes: MessageReader<ReceivedEnsembleMessage<Note>>,
    mut left: MessageReader<LobbyLeft>,
    mut changed: MessageReader<HostChanged>,
    mut heard: ResMut<Heard>,
    mut departures: ResMut<Departures>,
    mut changes: ResMut<Changes>,
) {
    for message in chat.read() {
        heard
            .0
            .push((message.sender, format!("chat:{}", message.message.0)));
    }
    for message in notes.read() {
        heard
            .0
            .push((message.sender, format!("note:{}", message.message.0)));
    }
    for message in left.read() {
        departures.0.push(message.reason.clone());
    }
    for message in changed.read() {
        changes
            .0
            .push((message.previous, message.new, message.promoted));
    }
}

fn peer(uuid: u128) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
        .add_plugins((
            EnsemblePlugin,
            LoopbackTransportPlugin,
            LobbyBroadcastPlugin,
        ))
        .add_plugins(PlayerDataPlugin::<Name>::default())
        .register_broadcast_message::<Chat>("Chat")
        .register_ensemble_message_type::<Note>("Note")
        .insert_resource(LocalMultiplayerPlayerId(uuid))
        .init_resource::<Heard>()
        .init_resource::<Departures>()
        .init_resource::<Changes>()
        .add_systems(Update, collect);
    app
}

/// A host and `clients` clients (uuids from 2), migratable or not, run until the roster settles.
fn session(clients: u128, migratable: bool) -> (LoopbackNetwork, Vec<PeerId>) {
    let mut net = LoopbackNetwork::new(FRAME);
    let mut peers = vec![net.add_host(HOST, peer(HOST))];
    for uuid in A..A + clients {
        peers.push(net.add_client(uuid, peer(uuid)));
    }
    if migratable {
        net.set_host_migration(Some(MIGRATION));
    }
    net.run(20);
    (net, peers)
}

fn has_lobby(net: &mut LoopbackNetwork, peer: PeerId) -> bool {
    let world = net.app_mut(peer).world_mut();
    world
        .query_filtered::<(), With<Lobby>>()
        .iter(world)
        .next()
        .is_some()
}

fn hosts(net: &mut LoopbackNetwork, peer: PeerId) -> bool {
    let world = net.app_mut(peer).world_mut();
    world
        .query_filtered::<(), (With<Lobby>, With<Host>)>()
        .iter(world)
        .next()
        .is_some()
}

fn awaiting(net: &mut LoopbackNetwork, peer: PeerId) -> Option<AwaitingHost> {
    let world = net.app_mut(peer).world_mut();
    world.query::<&AwaitingHost>().iter(world).next().copied()
}

fn host_uuid(net: &LoopbackNetwork, peer: PeerId) -> Option<u128> {
    net.app(peer)
        .world()
        .get_resource::<HostUuid>()
        .map(|host| host.0)
}

fn participants(net: &mut LoopbackNetwork, peer: PeerId) -> Vec<(u128, bool)> {
    let world = net.app_mut(peer).world_mut();
    let mut list: Vec<(u128, bool)> = world
        .query::<&LobbyParticipant>()
        .iter(world)
        .map(|participant| (participant.player_uuid, participant.is_host))
        .collect();
    list.sort();
    list
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

fn changes(net: &LoopbackNetwork, peer: PeerId) -> Vec<(u128, u128, bool)> {
    net.app(peer).world().resource::<Changes>().0.clone()
}

fn heard(net: &LoopbackNetwork, peer: PeerId) -> Vec<(Option<u128>, String)> {
    net.app(peer).world().resource::<Heard>().0.clone()
}

fn note(net: &mut LoopbackNetwork, from: PeerId, text: &str) {
    let lobby = net.lobby(from);
    let world = net.app_mut(from).world_mut();
    world.trigger(LobbyMessage::new(lobby, Note(text.into())));
    world.flush();
}

fn chat(net: &mut LoopbackNetwork, from: PeerId, text: &str) {
    let lobby = net.lobby(from);
    let world = net.app_mut(from).world_mut();
    world.trigger(BroadcastLobbyMessage::new(lobby, Chat(text.into())));
    world.flush();
}

fn encode<T: bevy_ensemble::EnsembleMessage>(
    net: &LoopbackNetwork,
    at: PeerId,
    message: &T,
) -> Vec<u8> {
    let registry = net.app(at).world().resource::<EnsembleMessageRegistry>();
    encode_ensemble_message(registry, message)
}

fn held_from(net: &LoopbackNetwork, peer: PeerId, sender: u128) -> usize {
    net.app(peer)
        .world()
        .get_resource::<HeldUntilVerified>()
        .map_or(0, |held| held.held_for(sender))
}

// ── Losing the host ──────────────────────────────────────────────────────────

#[test]
fn without_migration_a_lost_host_still_ends_the_session() {
    let (mut net, peers) = session(2, false);
    net.lose_host(HostDeparture::Crashes);
    net.run(2);
    for &client in &peers[1..] {
        assert!(!has_lobby(&mut net, client));
        assert_eq!(departures(&net, client), [LobbyLeftReason::HostGone]);
    }
}

#[test]
fn a_client_that_loses_its_host_keeps_its_lobby_and_waits() {
    let (mut net, peers) = session(2, true);
    net.lose_host(HostDeparture::Crashes);
    net.run(10);
    for &client in &peers[1..] {
        assert!(has_lobby(&mut net, client));
        let waiting = awaiting(&mut net, client).expect("waiting for a host");
        assert_eq!(waiting.previous, HOST);
        assert_eq!(waiting.successor, None);
        assert_eq!(waiting.cause, HostLossCause::Transport);
        assert!(departures(&net, client).is_empty());
    }
}

#[test]
fn a_host_that_merely_went_quiet_is_waited_for_and_taken_back() {
    let (mut net, peers) = session(2, true);
    for &client in &peers[1..] {
        net.app_mut(client)
            .insert_resource(PeerTimeout(Some(Duration::from_millis(200))));
    }
    let host = net.lose_host(HostDeparture::FallsSilent);
    net.run(30);
    for &client in &peers[1..] {
        assert_eq!(
            awaiting(&mut net, client).map(|waiting| waiting.cause),
            Some(HostLossCause::Silence),
            "a silence past the timeout is waited out, not left"
        );
    }

    net.reconnect(host);
    net.run(10);
    for &client in &peers[1..] {
        assert_eq!(awaiting(&mut net, client), None, "the host answered again");
        assert!(has_lobby(&mut net, client));
        assert!(departures(&net, client).is_empty());
        assert!(changes(&net, client).is_empty(), "nobody replaced the host");
    }
}

#[test]
fn a_host_that_stays_silent_still_ends_the_session_as_a_timeout() {
    let (mut net, peers) = session(1, true);
    let client = peers[1];
    net.set_host_migration(Some(HostMigratable {
        successor_within: Duration::from_millis(300),
        ..MIGRATION
    }));
    net.app_mut(client)
        .insert_resource(PeerTimeout(Some(Duration::from_millis(200))));
    net.lose_host(HostDeparture::FallsSilent);
    net.run(60);
    assert!(!has_lobby(&mut net, client));
    assert_eq!(departures(&net, client), [LobbyLeftReason::PeerTimeout]);
}

#[test]
fn no_successor_before_the_deadline_ends_the_session_as_host_gone() {
    let (mut net, peers) = session(1, true);
    let client = peers[1];
    net.set_host_migration(Some(HostMigratable {
        successor_within: Duration::from_millis(300),
        ..MIGRATION
    }));
    net.lose_host(HostDeparture::Crashes);
    net.run(10);
    assert!(has_lobby(&mut net, client), "still inside the wait");
    net.run(20);
    assert!(!has_lobby(&mut net, client));
    assert_eq!(departures(&net, client), [LobbyLeftReason::HostGone]);
}

#[test]
fn the_timeout_override_wins_over_the_backends_default() {
    let (mut net, peers) = session(1, true);
    let client = peers[1];
    net.app_mut(client).insert_resource(HostMigrationTimeouts {
        successor_within: Some(Duration::from_millis(300)),
        reach_within: None,
    });
    net.lose_host(HostDeparture::Crashes);
    net.run(30);
    assert_eq!(departures(&net, client), [LobbyLeftReason::HostGone]);
}

// ── A new host ───────────────────────────────────────────────────────────────

#[test]
fn the_named_successor_becomes_the_host_and_everyone_else_follows() {
    let (mut net, peers) = session(3, true);
    let (a, b, c) = (peers[1], peers[2], peers[3]);
    net.migrate(a);
    net.run(20);

    assert!(hosts(&mut net, a));
    assert_eq!(net.host(), a);
    for peer in [a, b, c] {
        assert_eq!(host_uuid(&net, peer), Some(A));
        assert_eq!(
            participants(&mut net, peer),
            [(A, true), (B, false), (C, false)],
            "the roster on peer {peer:?}"
        );
        assert!(departures(&net, peer).is_empty());
    }
    for follower in [b, c] {
        assert!(!hosts(&mut net, follower));
        assert_eq!(awaiting(&mut net, follower), None, "reached the new host");
        assert!(follows_verified(&mut net, follower, A));
    }
    assert_eq!(verified_seats(&mut net, a), [B, C]);
}

#[test]
fn every_peer_is_told_who_the_host_became() {
    let (mut net, peers) = session(2, true);
    let (a, b) = (peers[1], peers[2]);
    net.migrate(a);
    net.run(5);
    assert_eq!(changes(&net, a), [(HOST, A, true)]);
    assert_eq!(changes(&net, b), [(HOST, A, false)]);
}

#[test]
fn a_follower_reads_nothing_from_the_new_host_until_its_protocol_matches() {
    let (mut net, peers) = session(2, true);
    let (a, b) = (peers[1], peers[2]);
    net.lose_host(HostDeparture::Crashes);
    net.name_host(a);

    // Something from the new host, before the two have compared protocols.
    let early = encode(&net, a, &Note("early".into()));
    net.deliver_raw(b, A, early);
    net.run(1);
    assert!(
        heard(&net, b).is_empty(),
        "read before the protocol matched"
    );

    net.run(10);
    assert_eq!(
        heard(&net, b),
        [(Some(A), "note:early".into())],
        "held, then read once the protocol matched"
    );
}

#[test]
fn a_stale_verification_does_not_vouch_for_the_new_host() {
    let (mut net, peers) = session(2, true);
    let (a, b) = (peers[1], peers[2]);
    // A backend that switched the host without telling the core: the lobby still carries the
    // old host's verification.
    net.app_mut(b).insert_resource(HostUuid(A));
    let bytes = encode(&net, a, &Note("unverified".into()));
    net.deliver_raw(b, A, bytes);
    net.run(3);
    assert!(heard(&net, b).is_empty());
}

#[test]
fn the_old_hosts_word_counts_for_nothing_after_the_change() {
    let (mut net, peers) = session(2, true);
    let (old_host, a, b) = (peers[0], peers[1], peers[2]);
    // The arbiter names a successor while the old host is still running and connected.
    net.name_host(a);
    net.run(10);

    // The old host kicks b, as a host would.
    let world = net.app_mut(old_host).world_mut();
    let seat = world
        .query_filtered::<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>()
        .iter(world)
        .find(|(_, uuid)| uuid.0 == B)
        .map(|(entity, _)| entity)
        .expect("the old host still seats b");
    world.despawn(seat);
    net.run(10);

    assert!(
        has_lobby(&mut net, b),
        "a kick from the replaced host is not read"
    );
    assert!(departures(&net, b).is_empty());
    assert!(
        held_from(&net, b, HOST) > 0,
        "it is held unread: the old host is no longer a peer b has verified"
    );
}

#[test]
fn a_host_that_quits_a_migratable_lobby_kicks_nobody() {
    let (mut net, peers) = session(2, true);
    let (a, b) = (peers[1], peers[2]);
    net.lose_host(HostDeparture::Quits);
    net.name_host(a);
    net.run(20);
    assert!(hosts(&mut net, a));
    assert!(has_lobby(&mut net, b));
    assert!(departures(&net, b).is_empty());
}

#[test]
fn participant_entities_and_player_data_survive_a_host_change() {
    let (mut net, peers) = session(2, true);
    let (a, b) = (peers[1], peers[2]);
    for (peer, name) in [(a, "alice"), (b, "bob")] {
        let lobby = net.lobby(peer);
        let world = net.app_mut(peer).world_mut();
        world.trigger(SetPlayerData::new(lobby, Name(name.into())));
        world.flush();
    }
    net.run(10);

    let entities_on_b = |net: &mut LoopbackNetwork| -> Vec<(u128, Entity, Option<String>)> {
        let world = net.app_mut(b).world_mut();
        let mut list: Vec<_> = world
            .query::<(Entity, &LobbyParticipant, Option<&PlayerData<Name>>)>()
            .iter(world)
            .map(|(entity, participant, name)| {
                (
                    participant.player_uuid,
                    entity,
                    name.map(|name| name.0.0.clone()),
                )
            })
            .collect();
        list.sort();
        list
    };
    let before = entities_on_b(&mut net);

    net.migrate(a);
    net.run(20);
    let after = entities_on_b(&mut net);

    let without_host: Vec<_> = before
        .into_iter()
        .filter(|(uuid, ..)| *uuid != HOST)
        .collect();
    assert_eq!(after, without_host);
    assert_eq!(after[0].2.as_deref(), Some("alice"));
}

#[test]
fn a_player_who_left_while_the_host_was_gone_is_in_nobodys_roster() {
    let (mut net, peers) = session(3, true);
    let (a, b, c) = (peers[1], peers[2], peers[3]);
    net.lose_host(HostDeparture::Crashes);
    net.leave(c);
    net.name_host(a);
    net.run(20);
    for peer in [a, b] {
        assert_eq!(participants(&mut net, peer), [(A, true), (B, false)]);
    }
}

#[test]
fn a_follower_that_never_reaches_the_new_host_leaves_and_is_dropped_from_the_roster() {
    let (mut net, peers) = session(3, true);
    let (a, b, c) = (peers[1], peers[2], peers[3]);
    net.set_host_migration(Some(HostMigratable {
        reach_within: Duration::from_millis(300),
        ..MIGRATION
    }));
    net.lose_host(HostDeparture::Crashes);
    net.disconnect(c);
    net.name_host(a);
    net.run(5);

    let world = net.app_mut(a).world_mut();
    assert!(
        world
            .query::<(&LobbyParticipant, &AwaitingSeat)>()
            .iter(world)
            .any(|(participant, _)| participant.player_uuid == C),
        "the new host holds c's place while it may still arrive"
    );

    net.run(30);
    for peer in [a, b] {
        assert_eq!(participants(&mut net, peer), [(A, true), (B, false)]);
    }
    assert!(!has_lobby(&mut net, c));
    assert_eq!(departures(&net, c), [LobbyLeftReason::HostGone]);
}

#[test]
fn a_successor_that_dies_mid_migration_hands_over_to_the_next() {
    let (mut net, peers) = session(3, true);
    let (a, b, c) = (peers[1], peers[2], peers[3]);
    net.lose_host(HostDeparture::Crashes);
    net.name_host(a);
    net.lose_host(HostDeparture::Crashes);
    net.name_host(b);
    net.run(20);

    assert!(hosts(&mut net, b));
    assert_eq!(host_uuid(&net, c), Some(B));
    assert!(follows_verified(&mut net, c, B));
    assert_eq!(participants(&mut net, c), [(B, true), (C, false)]);
    assert_eq!(changes(&net, c), [(HOST, A, false), (A, B, false)]);
}

#[test]
fn a_pending_joiner_named_a_new_host_joins_it_instead() {
    let (mut net, peers) = session(2, true);
    let a = peers[1];
    let joiner = net.add_pending_client(C, peer(C));
    net.run(5);

    net.name_host(a);
    net.promote(joiner);
    net.run(20);

    assert!(has_lobby(&mut net, joiner));
    assert_eq!(host_uuid(&net, joiner), Some(A));
    assert!(follows_verified(&mut net, joiner, A));
    assert!(verified_seats(&mut net, a).contains(&C));
}

// ── After the change ─────────────────────────────────────────────────────────

#[test]
fn messages_sent_while_switching_hosts_are_dropped_not_delivered_late() {
    let (mut net, peers) = session(2, true);
    let (a, b) = (peers[1], peers[2]);
    net.lose_host(HostDeparture::Crashes);
    net.name_host(a);
    note(&mut net, b, "while switching");
    net.run(20);
    note(&mut net, b, "after");
    net.run(5);
    assert_eq!(heard(&net, a), [(Some(B), "note:after".into())]);
}

#[test]
fn a_broadcast_after_the_change_reaches_everyone_through_the_new_host() {
    let (mut net, peers) = session(3, true);
    let (a, b, c) = (peers[1], peers[2], peers[3]);
    net.migrate(a);
    net.run(20);
    chat(&mut net, c, "hello");
    net.run(5);
    for peer in [a, b, c] {
        assert_eq!(
            heard(&net, peer),
            [(Some(C), "chat:hello".into())],
            "on peer {peer:?}"
        );
    }
}

#[test]
fn liveness_resumes_against_the_new_host() {
    let (mut net, peers) = session(2, true);
    let (a, b) = (peers[1], peers[2]);
    net.migrate(a);
    net.run(400);
    assert!(has_lobby(&mut net, b));
    assert!(departures(&net, b).is_empty());
    let world = net.app_mut(b).world_mut();
    let last_pong = world
        .query_filtered::<&PeerLastPong, With<Lobby>>()
        .iter(world)
        .next()
        .expect("the liveness clock runs again")
        .0;
    assert!(last_pong < 1.0, "no pong from the new host in {last_pong}s");
    assert_eq!(verified_seats(&mut net, a), [B]);
}

#[test]
fn a_second_migration_works_like_the_first() {
    let (mut net, peers) = session(3, true);
    let (a, b, c) = (peers[1], peers[2], peers[3]);
    net.migrate(a);
    net.run(20);
    net.migrate(b);
    net.run(20);
    assert!(hosts(&mut net, b));
    assert_eq!(participants(&mut net, c), [(B, true), (C, false)]);
    chat(&mut net, c, "again");
    net.run(5);
    assert_eq!(heard(&net, b), [(Some(C), "chat:again".into())]);
}

#[test]
fn a_closed_lobby_ends_for_everyone_and_does_not_migrate() {
    let (mut net, peers) = session(2, true);
    let host = peers[0];
    net.app_mut(host).world_mut().write_message(CloseLobby);
    net.run(6);
    assert!(
        !has_lobby(&mut net, host),
        "the host left once it had said so"
    );
    for &client in &peers[1..] {
        assert!(!has_lobby(&mut net, client));
        assert_eq!(departures(&net, client), [LobbyLeftReason::HostGone]);
        assert!(changes(&net, client).is_empty());
    }
}
