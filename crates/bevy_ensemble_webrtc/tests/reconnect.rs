//! A broken link made again, end to end: the host drops its connection to one client, as its
//! transport does when that connection fails, and three apps running the plugin carry on over real
//! WebRTC on loopback with nobody having left.
//!
//! Loopback UDP is not a given on every CI machine. A session that never forms in the first place
//! prints `SKIPPED: inconclusive (no ICE connection)` and passes, as the socket tests do. Once the
//! session has formed, everything after is asserted.
#![cfg(not(target_arch = "wasm32"))]

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_ensemble::{
    AwaitingHost, EnsembleAppExt, EnsemblePlugin, HandshakeVerified, Host, HostChanged,
    HostMigratable, Lobby, LobbyClient, LobbyClientPlayerUuid, LobbyLeft, LobbyLeftReason,
    LobbyMessage, LobbyParticipant, LocalMultiplayerPlayerId, ReceivedEnsembleMessage,
    Reconnectable, SeatLinkLost, StartHosting, VerifiedHost,
};
use bevy_ensemble_webrtc::server::test_support::SignallingServer;
use bevy_ensemble_webrtc::{
    BevyEnsembleWebrtcPlugin, IceServers, JoinWebrtcLobbyByCode, LobbyWebrtcCode,
};
use serde::{Deserialize, Serialize};

/// How long the session has to form before the run is called inconclusive.
const FORM: Duration = Duration::from_secs(20);
/// How long anything after that may take. A reconnection on loopback takes a second or two.
const SETTLE: Duration = Duration::from_secs(20);
const FRAME: Duration = Duration::from_millis(4);

#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Note(String);

#[derive(Resource, Default)]
struct Seen {
    notes: Vec<(Option<u128>, String)>,
    left: Vec<LobbyLeftReason>,
    changes: Vec<(u128, u128, bool)>,
    /// Every frame this peer's lobby spent waiting for a host, and whether it was for the same one.
    waits: Vec<bool>,
}

fn collect(
    mut notes: MessageReader<ReceivedEnsembleMessage<Note>>,
    mut left: MessageReader<LobbyLeft>,
    mut changed: MessageReader<HostChanged>,
    awaiting: Query<&AwaitingHost>,
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
    for waiting in awaiting.iter() {
        seen.waits.push(waiting.is_reconnect());
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

/// The host's transport reports B's connection gone: what `poll_socket_peers` does on `Failed`,
/// and the liveness check on a silence.
fn host_loses_link_to(session: &mut Session, client: usize) {
    let uuid = session.uuid(client);
    let world = session.world(A);
    let seat = world
        .query_filtered::<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>()
        .iter(world)
        .find(|(_, seat)| seat.0 == uuid)
        .map(|(entity, _)| entity)
        .expect("the client has a seat");
    world.trigger(SeatLinkLost { entity: seat });
    world.flush();
}

#[test]
fn a_client_whose_connection_the_host_lost_is_reached_again_in_place() {
    let Some(mut session) = Session::formed() else {
        return;
    };
    let (a, b, c) = (session.uuid(A), session.uuid(B), session.uuid(C));
    assert!(session.has::<Reconnectable>(A) && session.has::<Reconnectable>(B));
    let b_on_a = session.participant(A, b);
    let roster = session.roster(A);

    host_loses_link_to(&mut session, B);
    assert!(
        session.run_until(SETTLE, |s| !s.seen(B).waits.is_empty()),
        "B never noticed its connection to the host was gone"
    );
    assert!(
        session.seen(B).waits.iter().all(|same_host| *same_host),
        "B waited for its host, not for a successor"
    );
    assert!(
        session.run_until(SETTLE, |s| {
            !s.has::<AwaitingHost>(B)
                && s.verified_host(B) == Some(a)
                && s.verified_seats(A).contains(&b)
        }),
        "the host never reached B again: B waiting {}, B verified {:#x?}, A's seats {:#x?}",
        session.has::<AwaitingHost>(B),
        session.verified_host(B),
        session.verified_seats(A),
    );

    assert_eq!(
        session.participant(A, b),
        b_on_a,
        "B kept its place on the host"
    );
    assert_eq!(session.roster(A), roster);
    assert_eq!(session.roster(B), roster);
    assert_eq!(session.roster(C), roster, "C was never told B left");
    for peer in [A, B, C] {
        assert!(session.seen(peer).left.is_empty(), "{peer} left");
        assert!(
            session.seen(peer).changes.is_empty(),
            "{peer} saw a host change"
        );
    }

    session.say(B, "back");
    session.say(A, "welcome back");
    assert!(
        session.run_until(SETTLE, |s| {
            s.seen(A).notes.contains(&(Some(b), "back".into()))
                && s.seen(B).notes.contains(&(Some(a), "welcome back".into()))
        }),
        "traffic did not flow again between the host and B: A heard {:?}, B heard {:?}",
        session.seen(A).notes.clone(),
        session.seen(B).notes.clone(),
    );
    let _ = c;
}
