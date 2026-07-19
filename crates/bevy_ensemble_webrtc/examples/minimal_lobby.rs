use bevy::prelude::*;
use bevy::window::{PresentMode, PrimaryWindow};
use bevy::winit::WinitSettings;
use bevy_ensemble::{
    BroadcastLobbyMessage, EnsembleAppExt, EnsemblePlugin, Host, Lobby, LobbyBroadcastAppExt,
    LobbyBroadcastPlugin, LobbyClient, LobbyClientPlayerUuid, LobbyMessage, LobbyParticipant,
    LobbyParticipantOf, LocalMultiplayerPlayerId, PeerRtt, PendingLobby, PlayerData,
    PlayerDataPlugin, PublicLobbies, ReceivedEnsembleMessage, SetPlayerData, StartHosting,
};
use bevy_ensemble_webrtc::{BevyEnsembleWebrtcPlugin, JoinWebrtcLobby, RefreshLobbyList};
use bevy_immediate::{BevyImmediatePlugin, ImmCtx, ui::CapsUi};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

const NUMBER_KEYS: &[KeyCode] = &[
    KeyCode::Digit1,
    KeyCode::Digit2,
    KeyCode::Digit3,
    KeyCode::Digit4,
    KeyCode::Digit5,
    KeyCode::Digit6,
    KeyCode::Digit7,
    KeyCode::Digit8,
    KeyCode::Digit9,
    KeyCode::Digit0,
];

const NUMPAD_KEYS: &[KeyCode] = &[
    KeyCode::Numpad1,
    KeyCode::Numpad2,
    KeyCode::Numpad3,
    KeyCode::Numpad4,
    KeyCode::Numpad5,
    KeyCode::Numpad6,
    KeyCode::Numpad7,
    KeyCode::Numpad8,
    KeyCode::Numpad9,
];

const NUMPAD_COLORS: &[[f32; 3]] = &[
    [1.0, 0.2, 0.2], // Red
    [0.2, 1.0, 0.2], // Green
    [0.2, 0.4, 1.0], // Blue
    [1.0, 1.0, 0.2], // Yellow
    [0.8, 0.2, 1.0], // Purple
    [1.0, 0.5, 0.0], // Orange
    [0.0, 1.0, 0.8], // Cyan
    [1.0, 0.4, 0.7], // Pink
    [1.0, 1.0, 1.0], // White
];

/// Per-player cosmetic data synchronized via [`PlayerDataPlugin`].
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
struct PlayerProfile {
    color: [f32; 3],
}

fn main() {
    let server_url = std::env::var("SIGNALLING_SERVER_URL")
        .ok()
        .or_else(|| option_env!("SIGNALLING_SERVER_URL").map(String::from))
        .unwrap_or_else(|| "ws://localhost:9090/ws".into());

    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(EnsemblePlugin)
        .add_plugins(LobbyBroadcastPlugin)
        .add_plugins(PlayerDataPlugin::<PlayerProfile>::default())
        .add_plugins(BevyEnsembleWebrtcPlugin {
            server_url,
            display_name: "Player".into(),
            ..default()
        })
        .add_plugins(BevyImmediatePlugin::<CapsUi>::new())
        .add_plugins(MinimalLobbyExamplePlugin)
        .run();
}

struct MinimalLobbyExamplePlugin;

impl Plugin for MinimalLobbyExamplePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChatLog>()
            .init_resource::<FastLoop>()
            .register_broadcast_message::<ChatMessage>()
            .register_ensemble_message_type::<WaveAction>()
            .add_systems(Startup, setup_camera)
            .add_systems(
                Update,
                (
                    render_ui,
                    update_roster,
                    handle_h_key,
                    handle_t_key,
                    handle_number_keys,
                    handle_numpad_color,
                    handle_r_key,
                    receive_chat_messages,
                    receive_wave_actions,
                    handle_escape_key,
                    handle_fast_loop_toggle,
                    update_fast_loop_hint,
                ),
            );
    }
}

/// A chat message broadcast to all lobby members via [`BroadcastLobbyMessage`].
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
struct ChatMessage {
    sender_name: String,
    text: String,
}

/// A wave action sent through the normal host-relayed [`LobbyMessage`] pipeline.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
struct WaveAction;

#[derive(Resource, Default)]
struct ChatLog(VecDeque<String>);

/// Root entity for the colored player roster (managed outside bevy_immediate).
#[derive(Component)]
struct RosterRoot;

/// Marker for dynamically spawned text spans in the roster.
#[derive(Component)]
struct RosterSpan;

fn setup_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
    commands.spawn((
        RosterRoot,
        Text::default(),
        Node {
            align_self: AlignSelf::Start,
            justify_self: JustifySelf::Start,
            margin: UiRect::axes(px(12.), px(8.)),
            ..default()
        },
        Visibility::Hidden,
    ));

    // Always-visible corner hint explaining the fast-loop toggle (see below).
    commands.spawn((
        FastLoopHint,
        Text::default(),
        TextFont {
            font_size: FontSize::Px(13.0),
            ..default()
        },
        TextColor(Color::srgb(0.7, 0.8, 0.9)),
        Node {
            position_type: PositionType::Absolute,
            top: px(8.),
            right: px(8.),
            max_width: px(360.),
            ..default()
        },
    ));
}

fn profile_color(profile: Option<&PlayerData<PlayerProfile>>) -> Color {
    profile
        .map(|p| {
            let [r, g, b] = p.0.color;
            Color::srgb(r, g, b)
        })
        .unwrap_or(Color::WHITE)
}

/// Manages the colored player roster using [`TextSpan`] children.
fn update_roster(
    mut commands: Commands,
    mut roster: Query<(Entity, &mut Visibility), With<RosterRoot>>,
    roster_spans: Query<Entity, With<RosterSpan>>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    client_lobbies: Query<Entity, (With<Lobby>, Without<Host>)>,
    participants: Query<(
        &LobbyParticipant,
        &LobbyParticipantOf,
        Option<&PlayerData<PlayerProfile>>,
    )>,
    lobby_clients: Query<(&LobbyClientPlayerUuid, Option<&PeerRtt>), With<LobbyClient>>,
    lobby_rtt: Query<Option<&PeerRtt>, (With<Lobby>, Without<Host>)>,
) {
    let Ok((roster_entity, mut vis)) = roster.single_mut() else {
        return;
    };

    for entity in roster_spans.iter() {
        commands.entity(entity).try_despawn();
    }

    let is_host = !host_lobbies.is_empty();
    let lobby_entity = host_lobbies
        .iter()
        .next()
        .or_else(|| client_lobbies.iter().next());

    let Some(lobby_entity) = lobby_entity else {
        *vis = Visibility::Hidden;
        return;
    };

    *vis = Visibility::Visible;

    let mut players: Vec<(u128, bool, Option<&PlayerData<PlayerProfile>>)> = participants
        .iter()
        .filter(|(_, pof, _)| pof.0 == lobby_entity)
        .map(|(p, _, data)| (p.player_uuid, p.is_host, data))
        .collect();
    players.sort_by_key(|(uuid, _, _)| *uuid);

    commands.entity(roster_entity).with_children(|parent| {
        parent.spawn((RosterSpan, TextSpan::new("Players:\n")));

        let mut kick_index = 0usize;
        for (uuid, participant_is_host, profile) in &players {
            let color = profile_color(*profile);
            let mut line = String::from("  ");

            if is_host && !participant_is_host {
                let key = if kick_index < 9 { kick_index + 1 } else { 0 };
                line.push_str(&format!("[{}] ", key));
                kick_index += 1;
            }

            line.push_str(&format!("Player {}", uuid % 10000));
            if *participant_is_host {
                line.push_str(" (Host)");
            }

            if is_host && !participant_is_host {
                if let Some((_, rtt)) = lobby_clients.iter().find(|(id, _)| id.0 == *uuid) {
                    if let Some(rtt) = rtt {
                        line.push_str(&format!(" - {:.0}ms", rtt.0 * 1000.0));
                    }
                }
            } else if !is_host && *participant_is_host {
                if let Ok(Some(rtt)) = lobby_rtt.get(lobby_entity) {
                    line.push_str(&format!(" - {:.0}ms", rtt.0 * 1000.0));
                }
            }

            line.push('\n');
            parent.spawn((RosterSpan, TextSpan::new(line), TextColor(color)));
        }

        let mut controls =
            "\nPress H to send hello (broadcast)\nNumpad 1-9 to change name color\nPress Escape to leave lobby"
                .to_string();
        if is_host {
            controls.push_str("\nPress T to wave (host-only action)\nPress 1-0 to kick a player");
        }
        parent.spawn((RosterSpan, TextSpan::new(controls)));
    });
}

/// Handles menu, loading, and chat log display via bevy_immediate.
/// The roster is handled separately by [`update_roster`].
fn render_ui(
    ctx: ImmCtx<CapsUi>,
    pending_lobbies: Query<(), With<PendingLobby>>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    client_lobbies: Query<Entity, (With<Lobby>, Without<Host>)>,
    lobby_list: Option<Res<PublicLobbies>>,
    chat_log: Res<ChatLog>,
) {
    let mut root = ctx.build_immediate_root("minimal_lobby");

    if !pending_lobbies.is_empty() {
        root.ch_id("loading").on_change_insert(true, || {
            (
                Node {
                    align_self: AlignSelf::Center,
                    justify_self: JustifySelf::Center,
                    ..default()
                },
                Text::new("Connecting..."),
            )
        });
        return;
    }

    let in_lobby = host_lobbies.iter().next().is_some() || client_lobbies.iter().next().is_some();

    if in_lobby {
        // Roster + controls are handled by update_roster; just show messages here
        let messages_text = if chat_log.0.is_empty() {
            "Messages:\n".to_string()
        } else {
            format!(
                "Messages:\n{}",
                chat_log.0.iter().cloned().collect::<Vec<_>>().join("\n")
            )
        };
        root.ch_id("messages").on_change_insert(true, move || {
            (
                Node {
                    align_self: AlignSelf::End,
                    justify_self: JustifySelf::Start,
                    margin: UiRect::axes(px(12.), px(8.)),
                    ..default()
                },
                Text::new(messages_text),
            )
        });
        return;
    }

    // Main menu
    let mut menu_text =
        "Host: Press H\nRefresh lobbies: Press R\nJoin lobby: Press 1-0".to_string();

    if let Some(lobby_list) = &lobby_list {
        if lobby_list.0.is_empty() {
            menu_text.push_str("\n\nNo lobbies available");
        } else {
            menu_text.push_str("\n\nAvailable lobbies:");
            for (i, lobby) in lobby_list.0.iter().enumerate() {
                let key = if i < 9 { i + 1 } else { 0 };
                menu_text.push_str(&format!(
                    "\n  [{}] {} - {}/{} players (id: {})",
                    key, lobby.host_name, lobby.player_count, lobby.max_players, lobby.lobby_id,
                ));
                if i >= 9 {
                    break;
                }
            }
        }
    }

    root.ch_id("menu").on_change_insert(true, move || {
        (
            Node {
                align_self: AlignSelf::Center,
                justify_self: JustifySelf::Center,
                ..default()
            },
            Text::new(menu_text),
        )
    });
}

fn handle_h_key(
    mut commands: Commands,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    mut start_hosting: MessageWriter<StartHosting>,
    lobbies: Query<Entity, With<Lobby>>,
    pending_lobbies: Query<(), With<PendingLobby>>,
) {
    if !keyboard_input.just_pressed(KeyCode::KeyH) {
        return;
    }

    if let Some(lobby) = lobbies.iter().next() {
        let sender_name = local_player
            .as_ref()
            .map(|p| format!("Player {}", p.0 % 10000))
            .unwrap_or_else(|| "Me".to_string());
        commands
            .entity(lobby)
            .trigger(|entity| BroadcastLobbyMessage::new(entity, ChatMessage {
                sender_name,
                text: "Hello".to_string(),
            }));
        return;
    }

    if pending_lobbies.is_empty() {
        start_hosting.write(StartHosting);
    }
}

fn handle_t_key(
    mut commands: Commands,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
) {
    if !keyboard_input.just_pressed(KeyCode::KeyT) {
        return;
    }

    if let Some(lobby) = lobbies.iter().next() {
        commands
            .entity(lobby)
            .trigger(|entity| LobbyMessage::new(entity, WaveAction));
    }
}

fn handle_number_keys(
    mut commands: Commands,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    lobby_list: Option<Res<PublicLobbies>>,
    mut join_writer: MessageWriter<JoinWebrtcLobby>,
    existing_lobbies: Query<(), With<Lobby>>,
    pending_lobbies: Query<(), With<PendingLobby>>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    lobby_clients: Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
) {
    let Some(pressed_index) = NUMBER_KEYS
        .iter()
        .position(|k| keyboard_input.just_pressed(*k))
    else {
        return;
    };

    if let Some(host_lobby) = host_lobbies.iter().next() {
        let mut kickable: Vec<u128> = participants
            .iter()
            .filter(|(p, pof)| pof.0 == host_lobby && !p.is_host)
            .map(|(p, _)| p.player_uuid)
            .collect();
        kickable.sort();

        if let Some(&target_uuid) = kickable.get(pressed_index) {
            if let Some((client_entity, _)) = lobby_clients
                .iter()
                .find(|(_, uuid)| uuid.0 == target_uuid)
            {
                commands.entity(client_entity).try_despawn();
            }
        }
        return;
    }

    if !existing_lobbies.is_empty() || !pending_lobbies.is_empty() {
        return;
    }
    let Some(lobby_list) = lobby_list else { return };
    if let Some(lobby) = lobby_list.0.get(pressed_index) {
        join_writer.write(JoinWebrtcLobby(lobby.lobby_id));
    }
}

/// Numpad 1-9 changes the local player's name color via [`PlayerData`].
fn handle_numpad_color(
    mut commands: Commands,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    lobbies: Query<Entity, With<Lobby>>,
) {
    let Some(pressed_index) = NUMPAD_KEYS
        .iter()
        .position(|k| keyboard_input.just_pressed(*k))
    else {
        return;
    };

    let Some(lobby) = lobbies.iter().next() else {
        return;
    };

    let color = NUMPAD_COLORS[pressed_index];
    commands
        .entity(lobby)
        .trigger(|entity| SetPlayerData::new(entity, PlayerProfile { color }));
}

fn handle_r_key(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    mut refresh_writer: MessageWriter<RefreshLobbyList>,
) {
    if keyboard_input.just_pressed(KeyCode::KeyR) {
        refresh_writer.write(RefreshLobbyList);
    }
}

fn receive_chat_messages(
    mut messages: MessageReader<ReceivedEnsembleMessage<ChatMessage>>,
    mut chat_log: ResMut<ChatLog>,
) {
    for message in messages.read() {
        push_chat_message(
            &mut chat_log,
            &message.message.sender_name,
            &message.message.text,
        );
    }
}

fn receive_wave_actions(
    mut messages: MessageReader<ReceivedEnsembleMessage<WaveAction>>,
    mut chat_log: ResMut<ChatLog>,
) {
    for message in messages.read() {
        let sender_name = message
            .sender
            .map(|uuid| format!("Player {}", uuid % 10000))
            .unwrap_or_else(|| "Unknown".to_string());
        push_chat_message(&mut chat_log, &sender_name, "waves");
    }
}

fn handle_escape_key(
    mut commands: Commands,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    lobbies: Query<Entity, Or<(With<Lobby>, With<PendingLobby>)>>,
    mut chat_log: ResMut<ChatLog>,
) {
    if !keyboard_input.just_pressed(KeyCode::Escape) {
        return;
    }

    for entity in lobbies.iter() {
        commands.entity(entity).try_despawn();
    }

    commands.remove_resource::<LocalMultiplayerPlayerId>();
    chat_log.0.clear();
}

/// Runtime state of the fast-loop toggle (see [`handle_fast_loop_toggle`]).
#[derive(Resource, Default)]
struct FastLoop(bool);

/// Marker for the corner hint text that explains the fast-loop toggle.
#[derive(Component)]
struct FastLoopHint;

/// Press **F** to toggle a "fast loop" mode that makes the app run as fast as possible.
///
/// It flips two Bevy defaults at once:
/// - [`WinitSettings::continuous`] — stop throttling the window when it loses focus. By
///   default ([`WinitSettings::game`]) an unfocused window drops to a 60Hz low-power loop.
///   That matters when you run two instances on one PC: only one has focus, so the
///   background one wakes ~60×/sec and adds latency to everything it handles.
/// - [`PresentMode::AutoNoVsync`] — remove the vsync frame cap (the default is vsync, which
///   pins updates to the monitor refresh — e.g. 144Hz is one update every ~6.9ms).
///
/// Why it matters for the ping readout: on localhost a packet spends ~no time on the wire,
/// so the RTT you see is almost entirely frame quantization — each hop waits for the next
/// frame on each side. Turning this on shrinks the frames toward zero, so the `rtt`/`wire`
/// numbers collapse toward the true (~sub-ms) transit time. It's a diagnostic switch, not
/// something a shipping game would leave on (it spins the CPU at 100%).
fn handle_fast_loop_toggle(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    mut fast: ResMut<FastLoop>,
    mut winit: ResMut<WinitSettings>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
) {
    if !keyboard_input.just_pressed(KeyCode::KeyF) {
        return;
    }
    fast.0 = !fast.0;

    *winit = if fast.0 {
        WinitSettings::continuous()
    } else {
        WinitSettings::game()
    };
    let present_mode = if fast.0 {
        PresentMode::AutoNoVsync
    } else {
        PresentMode::AutoVsync
    };
    for mut window in windows.iter_mut() {
        window.present_mode = present_mode;
    }
}

/// Keep the corner hint text in sync with the [`FastLoop`] state.
fn update_fast_loop_hint(fast: Res<FastLoop>, mut hint: Single<&mut Text, With<FastLoopHint>>) {
    if !fast.is_changed() {
        return;
    }
    ***hint = if fast.0 {
        "[F] Fast loop: ON\nUncapped update + no vsync. Frame quantization is gone, so \
         localhost rtt/wire collapse toward true transit (~sub-ms). Spins the CPU — \
         diagnostic only."
    } else {
        "[F] Fast loop: OFF\nVsync on, and an unfocused window is throttled to 60Hz. On \
         localhost the ping is mostly frame time, not network. Press F to see real latency."
    }
    .to_string();
}

fn push_chat_message(chat_log: &mut ChatLog, sender_name: &str, text: &str) {
    while chat_log.0.len() >= 8 {
        chat_log.0.pop_front();
    }
    chat_log.0.push_back(format!("{sender_name}: {text}"));
}
