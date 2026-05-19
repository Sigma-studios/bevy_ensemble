use bevy::prelude::*;
use bevy_ensemble::{
    EnsembleAppExt, EnsemblePlugin, Host, Lobby, LobbyClient, LobbyClientPlayerUuid,
    LobbyMessage, LobbyParticipant, LobbyParticipantOf, LocalMultiplayerPlayerId, PeerRtt,
    PendingLobby, PlayerData, PlayerDataPlugin, PublicLobbies, ReceivedEnsembleMessage,
    SetPlayerData, StartHosting,
};
use bevy_ensemble_webrtc::{BevyEnsembleWebrtcPlugin, JoinWebrtcLobby, RefreshLobbyList};
use serde::{Deserialize, Serialize};

const MOVE_SPEED: f32 = 200.0;
const CIRCLE_RADIUS: f32 = 25.0;

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

fn main() {
    let server_url = std::env::var("SIGNALLING_SERVER_URL")
        .ok()
        .or_else(|| option_env!("SIGNALLING_SERVER_URL").map(String::from))
        .unwrap_or_else(|| "ws://localhost:9090/ws".into());

    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(EnsemblePlugin)
        .add_plugins(PlayerDataPlugin::<PlayerProfile>::default())
        .add_plugins(BevyEnsembleWebrtcPlugin {
            server_url,
            display_name: "Player".into(),
            ..default()
        })
        .add_plugins(CircleSyncPlugin)
        .run();
}

struct CircleSyncPlugin;

impl Plugin for CircleSyncPlugin {
    fn build(&self, app: &mut App) {
        app.register_ensemble_message_type::<MoveIntent>()
            .register_ensemble_message_type::<PlayerPosition>()
            .add_systems(Startup, setup)
            .add_systems(
                Update,
                (
                    handle_h_key,
                    handle_number_keys,
                    handle_numpad_color,
                    handle_r_key,
                    handle_escape_key,
                    update_menu_text,
                    spawn_local_circle,
                    handle_wasd_input,
                    relay_moves_on_host,
                    receive_player_positions,
                    apply_player_colors,
                    cleanup_circles_on_lobby_leave,
                    cleanup_disconnected_players,
                ),
            );
    }
}

// -- Messages --

#[derive(Message, Clone, Debug, Serialize, Deserialize)]
struct MoveIntent {
    x: f32,
    y: f32,
}

#[derive(Message, Clone, Debug, Serialize, Deserialize)]
struct PlayerPosition {
    player_uuid: u128,
    x: f32,
    y: f32,
}

/// Per-player cosmetic data synchronized via [`PlayerDataPlugin`].
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
struct PlayerProfile {
    color: [f32; 3],
}

// -- Components --

#[derive(Component)]
struct PlayerCircle(u128);

#[derive(Component)]
struct MenuText;

/// Marker for dynamically spawned text spans in the roster.
#[derive(Component)]
struct RosterSpan;

// -- Setup --

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);
    commands.spawn((
        MenuText,
        Text::new("Press H to host\nPress R to refresh lobbies\nPress 1-0 to join a lobby"),
        Node {
            align_self: AlignSelf::Center,
            justify_self: JustifySelf::Center,
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

// -- Menu / lobby UI --

fn update_menu_text(
    mut commands: Commands,
    mut menu: Query<(Entity, &mut Text, &mut Visibility), With<MenuText>>,
    roster_spans: Query<Entity, With<RosterSpan>>,
    pending_lobbies: Query<(), With<PendingLobby>>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
    lobby_list: Option<Res<PublicLobbies>>,
    participants: Query<(
        &LobbyParticipant,
        &LobbyParticipantOf,
        Option<&PlayerData<PlayerProfile>>,
    )>,
    lobby_clients: Query<(&LobbyClientPlayerUuid, Option<&PeerRtt>), With<LobbyClient>>,
    lobby_rtt: Query<Option<&PeerRtt>, (With<Lobby>, Without<Host>)>,
    lobbies: Query<Entity, With<Lobby>>,
) {
    let Ok((menu_entity, mut text, mut vis)) = menu.single_mut() else {
        return;
    };

    // Clean up previous roster spans
    for entity in roster_spans.iter() {
        commands.entity(entity).try_despawn();
    }

    let is_host = !host_lobbies.is_empty();
    let in_lobby = is_host || !client_lobbies.is_empty();

    if in_lobby {
        *vis = Visibility::Visible;
        // Clear parent text — roster is built from TextSpan children
        **text = String::new();

        let lobby_entity = lobbies.iter().next().unwrap();

        let mut players: Vec<(u128, bool, Option<&PlayerData<PlayerProfile>>)> = participants
            .iter()
            .filter(|(_, pof, _)| pof.0 == lobby_entity)
            .map(|(p, _, data)| (p.player_uuid, p.is_host, data))
            .collect();
        players.sort_by_key(|(uuid, _, _)| *uuid);

        commands.entity(menu_entity).with_children(|parent| {
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
                "\nWASD to move | Numpad 1-9 to change color | Escape to leave".to_string();
            if is_host {
                controls.push_str(" | 1-0 to kick");
            }
            parent.spawn((RosterSpan, TextSpan::new(controls)));
        });
        return;
    }

    if !pending_lobbies.is_empty() {
        *vis = Visibility::Visible;
        **text = "Connecting...".into();
        return;
    }

    // Show main menu
    *vis = Visibility::Visible;
    let mut menu_str =
        "Press H to host\nPress R to refresh lobbies\nPress 1-0 to join a lobby".to_string();

    if let Some(lobby_list) = &lobby_list {
        if lobby_list.0.is_empty() {
            menu_str.push_str("\n\nNo lobbies available");
        } else {
            menu_str.push_str("\n\nAvailable lobbies:");
            for (i, lobby) in lobby_list.0.iter().enumerate() {
                let key = if i < 9 { i + 1 } else { 0 };
                menu_str.push_str(&format!(
                    "\n  [{}] {} - {}/{} players",
                    key, lobby.host_name, lobby.player_count, lobby.max_players,
                ));
                if i >= 9 {
                    break;
                }
            }
        }
    }
    **text = menu_str;
}

// -- Lobby management --

fn handle_h_key(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    mut start_hosting: MessageWriter<StartHosting>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
    pending_lobbies: Query<(), With<PendingLobby>>,
) {
    if !keyboard_input.just_pressed(KeyCode::KeyH) {
        return;
    }
    if !host_lobbies.is_empty() || !client_lobbies.is_empty() || !pending_lobbies.is_empty() {
        return;
    }
    start_hosting.write(StartHosting);
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

/// Numpad 1-9 changes the local player's color via [`PlayerData`].
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

fn handle_escape_key(
    mut commands: Commands,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    lobbies: Query<Entity, Or<(With<Lobby>, With<PendingLobby>)>>,
    circles: Query<Entity, With<PlayerCircle>>,
) {
    if !keyboard_input.just_pressed(KeyCode::Escape) {
        return;
    }
    for entity in lobbies.iter() {
        commands.entity(entity).try_despawn();
    }
    for entity in circles.iter() {
        commands.entity(entity).try_despawn();
    }
    commands.remove_resource::<LocalMultiplayerPlayerId>();
}

// -- Circle spawning & movement --

fn spawn_local_circle(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    lobbies: Query<(), With<Lobby>>,
    existing_circles: Query<&PlayerCircle>,
) {
    let Some(local_player) = local_player else {
        return;
    };
    if lobbies.is_empty() {
        return;
    }
    if existing_circles.iter().any(|c| c.0 == local_player.0) {
        return;
    }

    commands.spawn((
        PlayerCircle(local_player.0),
        Mesh2d(meshes.add(Circle::new(CIRCLE_RADIUS))),
        MeshMaterial2d(materials.add(ColorMaterial::from_color(Color::WHITE))),
        Transform::from_translation(Vec3::ZERO),
    ));
}

fn handle_wasd_input(
    mut commands: Commands,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    client_lobbies: Query<Entity, (With<Lobby>, Without<Host>)>,
    mut circles: Query<(&PlayerCircle, &mut Transform)>,
) {
    let Some(local_player) = local_player else {
        return;
    };

    let mut direction = Vec2::ZERO;
    if keyboard_input.pressed(KeyCode::KeyW) {
        direction.y += 1.0;
    }
    if keyboard_input.pressed(KeyCode::KeyS) {
        direction.y -= 1.0;
    }
    if keyboard_input.pressed(KeyCode::KeyA) {
        direction.x -= 1.0;
    }
    if keyboard_input.pressed(KeyCode::KeyD) {
        direction.x += 1.0;
    }

    if direction == Vec2::ZERO {
        return;
    }
    let delta = direction.normalize() * MOVE_SPEED * time.delta_secs();

    let Some((_, mut transform)) = circles.iter_mut().find(|(c, _)| c.0 == local_player.0) else {
        return;
    };
    transform.translation.x += delta.x;
    transform.translation.y += delta.y;

    let pos_x = transform.translation.x;
    let pos_y = transform.translation.y;

    if let Some(host_lobby) = host_lobbies.iter().next() {
        let player_uuid = local_player.0;
        commands
            .entity(host_lobby)
            .trigger(move |entity| LobbyMessage::new_unreliable(entity, PlayerPosition {
                player_uuid,
                x: pos_x,
                y: pos_y,
            }));
    } else if let Some(client_lobby) = client_lobbies.iter().next() {
        commands
            .entity(client_lobby)
            .trigger(move |entity| LobbyMessage::new_unreliable(entity, MoveIntent {
                x: pos_x,
                y: pos_y,
            }));
    }
}

fn relay_moves_on_host(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    mut messages: MessageReader<ReceivedEnsembleMessage<MoveIntent>>,
    mut circles: Query<(&PlayerCircle, &mut Transform)>,
) {
    let Some(host_lobby) = host_lobbies.iter().next() else {
        return;
    };

    let mut spawned_this_frame = Vec::new();

    for message in messages.read() {
        let Some(sender) = message.sender else {
            continue;
        };
        let player_uuid = sender;
        let x = message.message.x;
        let y = message.message.y;

        if let Some((_, mut transform)) = circles.iter_mut().find(|(c, _)| c.0 == player_uuid) {
            transform.translation.x = x;
            transform.translation.y = y;
        } else if !spawned_this_frame.contains(&player_uuid) {
            commands.spawn((
                PlayerCircle(player_uuid),
                Mesh2d(meshes.add(Circle::new(CIRCLE_RADIUS))),
                MeshMaterial2d(materials.add(ColorMaterial::from_color(Color::WHITE))),
                Transform::from_translation(Vec3::new(x, y, 0.0)),
            ));
            spawned_this_frame.push(player_uuid);
        }

        commands
            .entity(host_lobby)
            .trigger(move |entity| LobbyMessage::new_unreliable(entity, PlayerPosition {
                player_uuid,
                x,
                y,
            }));
    }
}

fn receive_player_positions(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    mut messages: MessageReader<ReceivedEnsembleMessage<PlayerPosition>>,
    mut circles: Query<(&PlayerCircle, &mut Transform)>,
) {
    let mut spawned_this_frame = Vec::new();

    for message in messages.read() {
        let pos = &message.message;

        if let Some(ref local) = local_player {
            if pos.player_uuid == local.0 {
                continue;
            }
        }

        if let Some((_, mut transform)) = circles.iter_mut().find(|(c, _)| c.0 == pos.player_uuid)
        {
            transform.translation.x = pos.x;
            transform.translation.y = pos.y;
        } else if !spawned_this_frame.contains(&pos.player_uuid) {
            commands.spawn((
                PlayerCircle(pos.player_uuid),
                Mesh2d(meshes.add(Circle::new(CIRCLE_RADIUS))),
                MeshMaterial2d(materials.add(ColorMaterial::from_color(Color::WHITE))),
                Transform::from_translation(Vec3::new(pos.x, pos.y, 0.0)),
            ));
            spawned_this_frame.push(pos.player_uuid);
        }
    }
}

/// Updates circle colors when [`PlayerData<PlayerProfile>`] changes on a participant.
fn apply_player_colors(
    participants: Query<
        (&LobbyParticipant, &PlayerData<PlayerProfile>),
        Changed<PlayerData<PlayerProfile>>,
    >,
    circles: Query<(&PlayerCircle, &MeshMaterial2d<ColorMaterial>)>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    for (participant, profile) in participants.iter() {
        let [r, g, b] = profile.0.color;
        for (circle, material_handle) in circles.iter() {
            if circle.0 == participant.player_uuid {
                if let Some(material) = materials.get_mut(&material_handle.0) {
                    material.color = Color::srgb(r, g, b);
                }
            }
        }
    }
}

fn cleanup_circles_on_lobby_leave(
    mut commands: Commands,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
    circles: Query<(Entity, &PlayerCircle)>,
) {
    if !lobbies.is_empty() || circles.is_empty() {
        return;
    }
    for (entity, _) in circles.iter() {
        commands.entity(entity).try_despawn();
    }
}

fn cleanup_disconnected_players(
    mut commands: Commands,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    circles: Query<(Entity, &PlayerCircle)>,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
) {
    let Some(lobby_entity) = host_lobbies.iter().next() else {
        return;
    };

    let active_uuids: Vec<u128> = participants
        .iter()
        .filter(|(_, pof)| pof.0 == lobby_entity)
        .map(|(p, _)| p.player_uuid)
        .collect();

    for (entity, circle) in circles.iter() {
        if let Some(ref local) = local_player {
            if circle.0 == local.0 {
                continue;
            }
        }
        if !active_uuids.contains(&circle.0) {
            commands.entity(entity).try_despawn();
        }
    }
}
