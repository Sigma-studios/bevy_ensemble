//! A host opening many peer connections at once stays inside the signalling server's budget.
//!
//! Each connection trickles its ICE candidates, and a host that opens seven at once — seven
//! players following one posted code, or the successor of a host migration in a full lobby —
//! produced a signal frame per candidate, all charged against the general per-connection budget
//! of forty. Past it the host's own candidates were refused, and until refusals became typed each
//! refusal also cost the host its identity. Now one frame carries every signal for one peer from
//! one app frame, and signalling has a budget of its own.
//!
//! One plugin app does the hosting; the other lobby members are raw websocket clients, so no
//! WebRTC connection has to form — what is measured is the host's outgoing signalling, which is
//! the same whether or not anybody answers. `BURST_ICE=stun` runs the same measurements with the
//! public STUN pair, which gathers more candidates (and needs the network).
#![cfg(not(target_arch = "wasm32"))]

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_ensemble::{
    EnsemblePlugin, Host, HostUuid, Lobby, LocalMultiplayerPlayerId, PendingLobby, StartHosting,
};
use bevy_ensemble_webrtc::protocol::{
    CAPABILITIES, CAPABILITY_HOST_MIGRATION, ClientMessage, ServerMessage, SignallingError, decode,
    encode,
};
use bevy_ensemble_webrtc::server::test_support::SignallingServer;
use bevy_ensemble_webrtc::{
    BevyEnsembleWebrtcPlugin, IceServers, JoinWebrtcLobbyByCode, LobbyWebrtcCode, SignallingRefused,
};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[derive(Resource, Default)]
struct Refusals(Vec<SignallingError>);

fn collect(mut refused: MessageReader<SignallingRefused>, mut out: ResMut<Refusals>) {
    out.0.extend(refused.read().map(|refusal| refusal.error));
}

fn ice() -> (IceServers, &'static str) {
    match std::env::var("BURST_ICE").as_deref() {
        Ok("stun") => (IceServers::default(), "public STUN pair"),
        _ => (IceServers::none(), "none (host candidates only)"),
    }
}

fn app(server: &SignallingServer, name: &str) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(EnsemblePlugin)
        .add_plugins(BevyEnsembleWebrtcPlugin {
            server_url: server.ws_url(),
            display_name: name.into(),
            ice_servers: ice().0,
            ..default()
        })
        .init_resource::<Refusals>()
        .add_systems(Update, collect);
    app
}

fn pump(app: &mut App, within: Duration, mut done: impl FnMut(&mut App) -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        app.update();
        if done(app) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
}

fn uuid(app: &App) -> Option<u128> {
    app.world()
        .get_resource::<LocalMultiplayerPlayerId>()
        .map(|id| id.0)
}

fn lobby_code(app: &mut App) -> Option<String> {
    let world = app.world_mut();
    world
        .query_filtered::<&LobbyWebrtcCode, With<Lobby>>()
        .iter(world)
        .next()
        .map(|c| c.0.clone())
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send(ws: &mut Ws, msg: &ClientMessage) {
    ws.send(Message::Binary(encode(msg).unwrap().into()))
        .await
        .unwrap();
}

async fn recv(ws: &mut Ws, within: Duration) -> Option<ServerMessage> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => return Some(decode(&b).unwrap()),
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

/// A raw member declaring `capabilities`: none is a client from before batching, which is sent
/// every signal as its own frame; [`CAPABILITIES`] is sent them batched.
async fn raw_player(url: &str, capabilities: u64) -> (Ws, u128) {
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::Authenticate {
            display_name: "raw".into(),
        },
    )
    .await;
    let uuid = match recv(&mut ws, Duration::from_secs(5)).await {
        Some(ServerMessage::Welcome { player_uuid }) => player_uuid,
        other => panic!("expected Welcome, got {other:?}"),
    };
    if capabilities != 0 {
        send(
            &mut ws,
            &ClientMessage::DeclareCapabilities { capabilities },
        )
        .await;
    }
    (ws, uuid)
}

/// What one raw member was relayed from `from`.
#[derive(Default, Debug, Clone)]
struct Seen {
    frames: usize,
    offers: usize,
    candidates: usize,
}

/// `PeerSignal`'s wire shape, mirrored (the sockets crate is not a dev-dependency here).
#[allow(dead_code)]
#[derive(serde::Deserialize)]
enum PeerSignal {
    Offer(String),
    Answer(String),
    IceCandidate(String),
}

async fn collect_signals(mut ws: Ws, from: u128, for_how_long: Duration) -> Seen {
    let mut seen = Seen::default();
    let deadline = tokio::time::Instant::now() + for_how_long;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(msg) = recv(&mut ws, left).await else {
            break;
        };
        let signals = match msg {
            ServerMessage::Signal { sender_uuid, data } if sender_uuid == from => vec![data],
            ServerMessage::Signals {
                sender_uuid,
                signals,
            } if sender_uuid == from => signals,
            _ => continue,
        };
        seen.frames += 1;
        for data in signals {
            match serde_json::from_str::<PeerSignal>(&data).unwrap() {
                PeerSignal::Offer(_) => seen.offers += 1,
                PeerSignal::IceCandidate(_) => seen.candidates += 1,
                PeerSignal::Answer(_) => {}
            }
        }
    }
    seen
}

fn report(label: &str, seen: &[Seen]) {
    println!("=== {label} (ICE servers: {})", ice().1);
    println!(
        "  per member: frames {:?}, offers {:?}, candidates {:?}",
        seen.iter().map(|s| s.frames).collect::<Vec<_>>(),
        seen.iter().map(|s| s.offers).collect::<Vec<_>>(),
        seen.iter().map(|s| s.candidates).collect::<Vec<_>>(),
    );
}

/// Host a lobby from a plugin app; returns the app, its uuid and the code.
fn plugin_host(server: &SignallingServer) -> (App, u128, String) {
    let mut host = app(server, "host");
    assert!(pump(&mut host, Duration::from_secs(5), |a| uuid(a).is_some()));
    let me = uuid(&host).unwrap();
    host.world_mut().write_message(StartHosting);
    assert!(pump(&mut host, Duration::from_secs(5), |a| lobby_code(a)
        .is_some()));
    let code = lobby_code(&mut host).unwrap();
    (host, me, code)
}

/// Seven players join an eight-player lobby at the same moment: a party following a posted code.
/// Four of them are clients from before batching, three are current, so the relay's both shapes
/// are exercised.
#[test]
fn seven_simultaneous_joins_are_signalled_without_a_refusal() {
    let server = SignallingServer::start();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (mut host, me, code) = plugin_host(&server);

    // Connect and authenticate all seven first, then join them together.
    let url = server.ws_url();
    let members: Vec<(Ws, u128)> = rt.block_on(futures_util::future::join_all(
        (0..7).map(|i| raw_player(&url, if i < 4 { 0 } else { CAPABILITIES })),
    ));
    let tasks: Vec<_> = members
        .into_iter()
        .map(|(mut ws, _)| {
            let code = code.clone();
            rt.spawn(async move {
                send(&mut ws, &ClientMessage::JoinLobbyByCode { code }).await;
                collect_signals(ws, me, Duration::from_secs(5)).await
            })
        })
        .collect();
    pump(&mut host, Duration::from_millis(5500), |_| false);
    let seen: Vec<Seen> = tasks.into_iter().map(|t| rt.block_on(t).unwrap()).collect();
    report("seven simultaneous joins", &seen);

    let refusals = &host.world().resource::<Refusals>().0;
    assert!(refusals.is_empty(), "the host was refused: {refusals:?}");
    assert!(
        seen.iter().all(|s| s.offers == 1),
        "every member is sent its offer: {seen:?}"
    );
    let w = host.world_mut();
    assert_eq!(
        w.query_filtered::<(), (With<Lobby>, With<Host>)>()
            .iter(w)
            .count(),
        1,
        "still hosting"
    );
    assert_eq!(uuid(&host), Some(me));
    assert_eq!(
        host.world().get_resource::<HostUuid>().map(|h| h.0),
        Some(me)
    );
}

/// Host migration in an eight-player lobby: the successor opens connections to the six remaining
/// members in one frame.
#[test]
fn a_new_host_signals_every_member_after_migration_without_a_refusal() {
    let server = SignallingServer::start();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let url = server.ws_url();

    // A raw host that declares migration, so the lobby is migratable.
    let (mut old_host, _) = rt.block_on(raw_player(&url, CAPABILITY_HOST_MIGRATION));
    let code = rt.block_on(async {
        send(
            &mut old_host,
            &ClientMessage::CreateLobby { max_players: 8 },
        )
        .await;
        match recv(&mut old_host, Duration::from_secs(5)).await {
            Some(ServerMessage::LobbyCreated { code, .. }) => code,
            other => panic!("expected LobbyCreated, got {other:?}"),
        }
    });

    // The app joins first, so the server picks it as the successor.
    let mut b = app(&server, "b");
    assert!(pump(&mut b, Duration::from_secs(5), |a| uuid(a).is_some()));
    let me = uuid(&b).unwrap();
    b.world_mut()
        .write_message(JoinWebrtcLobbyByCode(code.clone()));
    assert!(pump(&mut b, Duration::from_secs(5), |_| {
        server
            .state()
            .connections
            .get(&me)
            .and_then(|c| c.lobby_id)
            .is_some()
    }));

    // Six more members, joined over time (a lobby filling up normally); half of them current,
    // half from before batching (but after migration).
    let mut members = Vec::new();
    for i in 0..6 {
        let capabilities = if i % 2 == 0 {
            CAPABILITIES
        } else {
            CAPABILITY_HOST_MIGRATION
        };
        let (mut ws, _) = rt.block_on(raw_player(&url, capabilities));
        rt.block_on(async {
            send(
                &mut ws,
                &ClientMessage::JoinLobbyByCode { code: code.clone() },
            )
            .await;
            // (`ServerHello`,) `LobbyJoined`, `LobbyMigratable`
            let expected = if capabilities == CAPABILITIES { 3 } else { 2 };
            for _ in 0..expected {
                recv(&mut ws, Duration::from_secs(5)).await;
            }
        });
        members.push(ws);
        pump(&mut b, Duration::from_millis(50), |_| false);
    }

    // The old host crashes.
    drop(old_host);
    let tasks: Vec<_> = members
        .into_iter()
        .map(|ws| rt.spawn(collect_signals(ws, me, Duration::from_secs(5))))
        .collect();
    pump(&mut b, Duration::from_millis(5500), |_| false);
    let seen: Vec<Seen> = tasks.into_iter().map(|t| rt.block_on(t).unwrap()).collect();
    report("new host after migration, 6 members", &seen);

    let refusals = &b.world().resource::<Refusals>().0;
    assert!(
        refusals.is_empty(),
        "the new host was refused: {refusals:?}"
    );
    assert!(
        seen.iter().all(|s| s.offers == 1),
        "every member is sent the new host's offer: {seen:?}"
    );
    let w = b.world_mut();
    let lobbies = w
        .query_filtered::<(), Or<(With<Lobby>, With<PendingLobby>)>>()
        .iter(w)
        .count();
    assert_eq!(lobbies, 1, "still in the lobby");
    assert_eq!(uuid(&b), Some(me));
}
