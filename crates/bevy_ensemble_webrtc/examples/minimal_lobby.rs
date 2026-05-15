use bevy::prelude::*;
use bevy_ensemble::{
    EnsembleAppExt, EnsemblePlugin, Host, Lobby, LobbyClient, LobbyClientPlayerUuid,
    LobbyMessage, LobbyParticipant, LobbyParticipantOf, LocalMultiplayerPlayerId, PeerRtt,
    PendingLobby, PublicLobbies, ReceivedEnsembleMessage, StartHosting,
};
use bevy_ensemble_webrtc::{
    BevyEnsembleWebrtcPlugin, JoinWebrtcLobby, RefreshLobbyList,
};
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

fn main() {
    let server_url = std::env::var("SIGNALLING_SERVER_URL")
        .ok()
        .or_else(|| option_env!("SIGNALLING_SERVER_URL").map(String::from))
        .unwrap_or_else(|| "ws://localhost:9090/ws".into());

    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(EnsemblePlugin)
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
            .register_ensemble_message_type::<HelloIntent>()
            .register_ensemble_message_type::<DisplayChatMessage>()
            .add_systems(Startup, setup_camera)
            .add_systems(
                Update,
                (
                    render_ui,
                    handle_h_key,
                    handle_number_keys,
                    handle_r_key,
                    relay_hello_messages_on_host,
                    receive_display_chat_messages,
                    handle_escape_key,
                ),
            );
    }
}

#[derive(Message, Clone, Debug, Serialize, Deserialize)]
struct HelloIntent;

#[derive(Message, Clone, Debug, Serialize, Deserialize)]
struct DisplayChatMessage {
    sender_name: String,
    text: String,
}

#[derive(Resource, Default)]
struct ChatLog(VecDeque<String>);

fn setup_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}

fn render_ui(
    ctx: ImmCtx<CapsUi>,
    pending_lobbies: Query<(), With<PendingLobby>>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    client_lobbies: Query<Entity, (With<Lobby>, Without<Host>)>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    lobby_clients: Query<(&LobbyClientPlayerUuid, Option<&PeerRtt>), With<LobbyClient>>,
    lobby_rtt: Query<Option<&PeerRtt>, (With<Lobby>, Without<Host>)>,
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

    let ready_lobby = host_lobbies
        .iter()
        .next()
        .or_else(|| client_lobbies.iter().next());

    let Some(lobby_entity) = ready_lobby else {
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
        return;
    };

    let is_host = host_lobbies.get(lobby_entity).is_ok();
    let roster_text = build_roster_text(
        lobby_entity,
        is_host,
        &participants,
        &lobby_clients,
        &lobby_rtt,
    );
    let messages_text = if chat_log.0.is_empty() {
        "Messages:\n".to_string()
    } else {
        format!(
            "Messages:\n{}",
            chat_log.0.iter().cloned().collect::<Vec<_>>().join("\n")
        )
    };

    let mut lobby_info_text = format!("Players:\n{}", roster_text);
    lobby_info_text.push_str("\n\nPress H to send hello\nPress Escape to leave lobby");
    if is_host {
        lobby_info_text.push_str("\nPress 1-0 to kick a player");
    }
    root.ch_id("lobby_info").on_change_insert(true, move || {
        (
            Node {
                align_self: AlignSelf::Start,
                justify_self: JustifySelf::Start,
                margin: UiRect::axes(px(12.), px(8.)),
                ..default()
            },
            Text::new(lobby_info_text),
        )
    });
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
}

fn handle_h_key(
    mut commands: Commands,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    mut start_hosting: MessageWriter<StartHosting>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    client_lobbies: Query<Entity, (With<Lobby>, Without<Host>)>,
    pending_lobbies: Query<(), With<PendingLobby>>,
    mut chat_log: ResMut<ChatLog>,
) {
    if !keyboard_input.just_pressed(KeyCode::KeyH) {
        return;
    }

    if let Some(host_lobby) = host_lobbies.iter().next() {
        let sender_name = local_player
            .as_ref()
            .map(|p| format!("Player {}", p.0 % 10000))
            .unwrap_or_else(|| "Me".to_string());
        let message = DisplayChatMessage {
            sender_name: sender_name.clone(),
            text: "Hello".to_string(),
        };
        push_chat_message(&mut chat_log, &sender_name, "Hello");
        commands
            .entity(host_lobby)
            .trigger(move |entity| LobbyMessage::<DisplayChatMessage> { entity, message });
        return;
    }

    if let Some(client_lobby) = client_lobbies.iter().next() {
        commands
            .entity(client_lobby)
            .trigger(|entity| LobbyMessage::<HelloIntent> {
                entity,
                message: HelloIntent,
            });
        return;
    }

    if pending_lobbies.is_empty() {
        start_hosting.write(StartHosting);
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

    // In a host lobby: kick the nth non-host participant
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

    // Not in a lobby: join the nth available lobby
    if !existing_lobbies.is_empty() || !pending_lobbies.is_empty() {
        return;
    }
    let Some(lobby_list) = lobby_list else { return };
    if let Some(lobby) = lobby_list.0.get(pressed_index) {
        join_writer.write(JoinWebrtcLobby(lobby.lobby_id));
    }
}

fn handle_r_key(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    mut refresh_writer: MessageWriter<RefreshLobbyList>,
) {
    if keyboard_input.just_pressed(KeyCode::KeyR) {
        refresh_writer.write(RefreshLobbyList);
    }
}

fn relay_hello_messages_on_host(
    mut commands: Commands,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    mut messages: MessageReader<ReceivedEnsembleMessage<HelloIntent>>,
    mut chat_log: ResMut<ChatLog>,
) {
    let Some(host_lobby) = host_lobbies.iter().next() else {
        return;
    };

    for message in messages.read() {
        let sender_name = message
            .sender
            .map(|uuid| format!("Player {}", uuid % 10000))
            .unwrap_or_else(|| "Unknown".to_string());
        let display = DisplayChatMessage {
            sender_name,
            text: "Hello".to_string(),
        };
        push_chat_message(&mut chat_log, &display.sender_name, &display.text);
        commands
            .entity(host_lobby)
            .trigger(move |entity| LobbyMessage::<DisplayChatMessage> {
                entity,
                message: display,
            });
    }
}

fn receive_display_chat_messages(
    mut messages: MessageReader<ReceivedEnsembleMessage<DisplayChatMessage>>,
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

fn build_roster_text(
    lobby_entity: Entity,
    is_host: bool,
    participants: &Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    lobby_clients: &Query<(&LobbyClientPlayerUuid, Option<&PeerRtt>), With<LobbyClient>>,
    lobby_rtt: &Query<Option<&PeerRtt>, (With<Lobby>, Without<Host>)>,
) -> String {
    let mut players: Vec<(u128, bool)> = participants
        .iter()
        .filter(|(_, pof)| pof.0 == lobby_entity)
        .map(|(p, _)| (p.player_uuid, p.is_host))
        .collect();
    players.sort_by_key(|(uuid, _)| *uuid);

    if players.is_empty() {
        return "(No players)".to_string();
    }

    let mut lines = Vec::new();
    let mut kick_index = 0usize;

    for (uuid, participant_is_host) in &players {
        let mut line = String::new();

        // Show kick index for non-host participants (only when we are the host)
        if is_host && !participant_is_host {
            let key = if kick_index < 9 {
                kick_index + 1
            } else {
                0
            };
            line.push_str(&format!("[{}] ", key));
            kick_index += 1;
        }

        line.push_str(&format!("Player {}", uuid % 10000));
        if *participant_is_host {
            line.push_str(" (Host)");
        }

        // Show ping
        if is_host && !participant_is_host {
            // Host: look up RTT on the LobbyClient entity for this peer
            if let Some((_, rtt)) = lobby_clients.iter().find(|(id, _)| id.0 == *uuid) {
                if let Some(rtt) = rtt {
                    line.push_str(&format!(" - {:.0}ms", rtt.0 * 1000.0));
                }
            }
        } else if !is_host && *participant_is_host {
            // Client: show our ping to the host on the host's line
            if let Ok(Some(rtt)) = lobby_rtt.get(lobby_entity) {
                line.push_str(&format!(" - {:.0}ms", rtt.0 * 1000.0));
            }
        }

        lines.push(line);
    }

    lines.join("\n")
}

fn push_chat_message(chat_log: &mut ChatLog, sender_name: &str, text: &str) {
    while chat_log.0.len() >= 8 {
        chat_log.0.pop_front();
    }
    chat_log.0.push_back(format!("{sender_name}: {text}"));
}
