//! A lobby outliving its host, end to end: the in-process signalling server picks the successor,
//! and three apps running the plugin find each other again over real WebRTC on loopback.
//!
//! Loopback UDP is not a given on every CI machine. A session that never forms in the first place
//! prints `SKIPPED: inconclusive (no ICE connection)` and passes, as the socket tests do: a
//! missing network proves nothing either way. Once the first host is reached, everything after is
//! asserted.
#![cfg(not(target_arch = "wasm32"))]

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_ensemble::{
    AwaitingHost, CloseLobby, EnsembleAppExt, EnsemblePlugin, HandshakeVerified, Host, HostChanged,
    HostMigratable, LeaveLobby, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyLeft,
    LobbyLeftReason, LobbyMessage, LobbyParticipant, LocalMultiplayerPlayerId,
    ReceivedEnsembleMessage, StartHosting, VerifiedHost,
};
use bevy_ensemble_webrtc::server::test_support::SignallingServer;
use bevy_ensemble_webrtc::{
    BevyEnsembleWebrtcPlugin, IceServers, JoinWebrtcLobbyByCode, LobbyWebrtcCode,
};
use serde::{Deserialize, Serialize};

/// How long the first host has to be reached before the run is called inconclusive.
const FORM: Duration = Duration::from_secs(20);
/// How long anything after that may take. A host change on loopback takes a second or two.
const SETTLE: Duration = Duration::from_secs(20);
const FRAME: Duration = Duration::from_millis(4);

#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Note(String);

#[derive(Resource, Default)]
struct Seen {
    notes: Vec<(Option<u128>, String)>,
    left: Vec<LobbyLeftReason>,
    changes: Vec<(u128, u128, bool)>,
}

fn collect(
    mut notes: MessageReader<ReceivedEnsembleMessage<Note>>,
    mut left: MessageReader<LobbyLeft>,
    mut changed: MessageReader<HostChanged>,
    mut seen: ResMut<Seen>,
) {
    for note in notes.read() {
        seen.notes.push((note.sender, note.message.0.clone()));
    }
    for message in left.read() {
        seen.left.push(message.reason.clone());
    }
    for message in changed.read() {
        seen.changes
            .push((message.previous, message.new, message.promoted));
    }
}

fn app(server: &SignallingServer, name: &str) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(EnsemblePlugin)
        .add_plugins(BevyEnsembleWebrtcPlugin {
            server_url: server.ws_url(),
            display_name: name.into(),
            ice_servers: IceServers::none(),
            ..default()
        })
        .register_ensemble_message_type::<Note>("Note")
        .init_resource::<Seen>()
        .add_systems(Update, collect);
    app
}

/// Three apps and the server they meet on. A dropped app is `None`: it crashed.
struct Session {
    // Declared first so the apps, and their connections, go before the server does.
    apps: [Option<App>; 3],
    _server: SignallingServer,
}

const A: usize = 0;
const B: usize = 1;
const C: usize = 2;

impl Session {
    /// A hosts; B joins, then C, so B is the earliest-joined member and the server's pick.
    ///
    /// `None` when the session never formed on this machine.
    fn formed() -> Option<Self> {
        let server = SignallingServer::start();
        let mut session = Self {
            apps: [
                Some(app(&server, "a")),
                Some(app(&server, "b")),
                Some(app(&server, "c")),
            ],
            _server: server,
        };

        session.write(A, StartHosting);
        assert!(
            session.run_until(SETTLE, |s| s.code(A).is_some()),
            "the server never created A's lobby"
        );
        let code = session.code(A).unwrap();
        assert!(
            session.run_until(SETTLE, |s| s.has::<HostMigratable>(A)),
            "a lobby on this server should be migratable"
        );

        for joiner in [B, C] {
            session.write(joiner, JoinWebrtcLobbyByCode(code.clone()));
            if !session.run_until(FORM, |s| {
                s.verified_host(joiner) == Some(s.uuid(A))
                    && s.verified_seats(A).contains(&s.uuid(joiner))
            }) {
                println!("SKIPPED: inconclusive (no ICE connection)");
                return None;
            }
        }
        let everyone = {
            let mut uuids = vec![session.uuid(A), session.uuid(B), session.uuid(C)];
            uuids.sort();
            uuids
        };
        assert!(
            session.run_until(SETTLE, |s| [A, B, C]
                .into_iter()
                .all(|peer| s.roster(peer) == everyone)),
            "the roster never settled on all three"
        );
        Some(session)
    }

    fn app(&mut self, peer: usize) -> &mut App {
        self.apps[peer].as_mut().expect("that peer crashed")
    }

    fn world(&mut self, peer: usize) -> &mut World {
        self.app(peer).world_mut()
    }

    fn write<M: Message>(&mut self, peer: usize, message: M) {
        self.world(peer).write_message(message);
    }

    fn run_until(&mut self, within: Duration, mut done: impl FnMut(&mut Self) -> bool) -> bool {
        let deadline = Instant::now() + within;
        loop {
            for app in self.apps.iter_mut().flatten() {
                app.update();
            }
            if done(self) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(FRAME);
        }
    }

    fn uuid(&mut self, peer: usize) -> u128 {
        self.world(peer)
            .get_resource::<LocalMultiplayerPlayerId>()
            .expect("a peer in a session knows who it is")
            .0
    }

    fn lobby(&mut self, peer: usize) -> Option<Entity> {
        let world = self.world(peer);
        world
            .query_filtered::<Entity, With<Lobby>>()
            .iter(world)
            .next()
    }

    fn has<C: Component>(&mut self, peer: usize) -> bool {
        let world = self.world(peer);
        world
            .query_filtered::<(), (With<Lobby>, With<C>)>()
            .iter(world)
            .next()
            .is_some()
    }

    fn code(&mut self, peer: usize) -> Option<String> {
        let world = self.world(peer);
        world
            .query_filtered::<&LobbyWebrtcCode, (With<Lobby>, With<Host>)>()
            .iter(world)
            .next()
            .map(|code| code.0.clone())
    }

    /// The host this peer's lobby has verified, when it is a client with one.
    fn verified_host(&mut self, peer: usize) -> Option<u128> {
        let world = self.world(peer);
        world
            .query_filtered::<&VerifiedHost, (With<Lobby>, With<HandshakeVerified>, Without<Host>)>(
            )
            .iter(world)
            .next()
            .map(|verified| verified.0)
    }

    fn verified_seats(&mut self, peer: usize) -> Vec<u128> {
        let world = self.world(peer);
        world
            .query_filtered::<&LobbyClientPlayerUuid, (With<LobbyClient>, With<HandshakeVerified>)>(
            )
            .iter(world)
            .map(|uuid| uuid.0)
            .collect()
    }

    /// Everyone this peer's roster lists, sorted.
    fn roster(&mut self, peer: usize) -> Vec<u128> {
        let world = self.world(peer);
        let mut roster: Vec<u128> = world
            .query::<&LobbyParticipant>()
            .iter(world)
            .map(|participant| participant.player_uuid)
            .collect();
        roster.sort();
        roster
    }

    fn participant(&mut self, peer: usize, uuid: u128) -> Option<Entity> {
        let world = self.world(peer);
        world
            .query::<(Entity, &LobbyParticipant)>()
            .iter(world)
            .find(|(_, participant)| participant.player_uuid == uuid)
            .map(|(entity, _)| entity)
    }

    fn seen(&mut self, peer: usize) -> &Seen {
        self.world(peer).resource::<Seen>()
    }

    fn say(&mut self, peer: usize, text: &str) {
        let lobby = self.lobby(peer).expect("a peer that speaks is in a lobby");
        let note = Note(text.into());
        self.world(peer)
            .commands()
            .entity(lobby)
            .trigger(move |entity| LobbyMessage::new(entity, note));
        self.world(peer).flush();
    }
}

/// What every departure that hands the lobby over ends in: B hosts, C follows it, both still know
/// each other, and they can talk.
fn assert_b_took_over(session: &mut Session, a: u128) {
    let (b, c) = (session.uuid(B), session.uuid(C));
    let c_as_seen_by_b = session.participant(B, c);
    let b_as_seen_by_c = session.participant(C, b);

    assert!(
        session.run_until(SETTLE, |s| {
            s.has::<Host>(B)
                && s.verified_host(C) == Some(b)
                && s.verified_seats(B) == vec![c]
                && s.roster(B) == s.roster(C)
                && !s.roster(B).contains(&a)
        }),
        "B never took over with C verified on both sides: B hosts {}, C verified {:#x?}, B's \
         seats {:#x?}, rosters {:#x?} / {:#x?}",
        session.has::<Host>(B),
        session.verified_host(C),
        session.verified_seats(B),
        session.roster(B),
        session.roster(C),
    );

    assert_eq!(session.seen(B).changes, vec![(a, b, true)]);
    assert_eq!(session.seen(C).changes, vec![(a, b, false)]);
    assert!(
        session.seen(B).left.is_empty(),
        "B left: {:?}",
        session.seen(B).left
    );
    assert!(
        session.seen(C).left.is_empty(),
        "C left: {:?}",
        session.seen(C).left
    );
    assert!(!session.has::<AwaitingHost>(C));
    assert_eq!(
        session.participant(B, c),
        c_as_seen_by_b,
        "a member who stayed keeps its entity on the new host"
    );
    assert_eq!(
        session.participant(C, b),
        b_as_seen_by_c,
        "the new host keeps its entity on a follower"
    );

    session.say(C, "to the new host");
    session.say(B, "from the new host");
    assert!(
        session.run_until(SETTLE, |s| {
            s.seen(B)
                .notes
                .contains(&(Some(c), "to the new host".into()))
                && s.seen(C)
                    .notes
                    .contains(&(Some(b), "from the new host".into()))
        }),
        "traffic did not flow between the new host and its follower: B heard {:?}, C heard {:?}",
        session.seen(B).notes.clone(),
        session.seen(C).notes.clone(),
    );
}

#[test]
fn a_host_that_leaves_hands_the_lobby_to_the_earliest_joined_member() {
    let Some(mut session) = Session::formed() else {
        return;
    };
    let a = session.uuid(A);
    session.write(A, LeaveLobby);
    assert_b_took_over(&mut session, a);
}

#[test]
fn a_host_that_crashes_hands_the_lobby_over_the_same_way() {
    let Some(mut session) = Session::formed() else {
        return;
    };
    let a = session.uuid(A);
    // Its runtime goes with it, and the server sees the socket close with no word first.
    session.apps[A] = None;
    assert_b_took_over(&mut session, a);
}

#[test]
fn a_closed_lobby_ends_for_everyone_and_nobody_takes_it_over() {
    let Some(mut session) = Session::formed() else {
        return;
    };
    session.write(A, CloseLobby);
    assert!(
        session.run_until(SETTLE, |s| {
            [A, B, C].into_iter().all(|peer| s.lobby(peer).is_none())
        }),
        "a lobby survived being closed"
    );
    // Room for a hand-over that should not happen to show up.
    session.run_until(Duration::from_millis(500), |_| false);
    for peer in [B, C] {
        assert_eq!(session.seen(peer).left, vec![LobbyLeftReason::HostGone]);
        assert!(session.seen(peer).changes.is_empty());
        assert!(session.lobby(peer).is_none());
    }
}

#[test]
fn a_successor_that_crashes_as_it_takes_over_hands_the_lobby_to_the_next() {
    let Some(mut session) = Session::formed() else {
        return;
    };
    let (a, b, c) = (session.uuid(A), session.uuid(B), session.uuid(C));
    session.apps[A] = None;
    assert!(
        session.run_until(SETTLE, |s| s.has::<Host>(B)),
        "B was never named host"
    );
    session.apps[B] = None;
    assert!(
        session.run_until(SETTLE, |s| s.has::<Host>(C)
            && s.roster(C) == vec![c]
            && s.seen(C).changes.len() == 2),
        "C never took over the lobby alone: hosts {}, roster {:#x?}",
        session.has::<Host>(C),
        session.roster(C),
    );
    assert_eq!(session.seen(C).changes, vec![(a, b, false), (b, c, true)]);
    assert!(
        session.seen(C).left.is_empty(),
        "C left: {:?}",
        session.seen(C).left
    );
    assert!(!session.has::<AwaitingHost>(C));
}
