use bevy::prelude::*;
use bevy_ensemble::{
    EnsembleAppExt, EnsemblePlugin, Host, Lobby, LobbyMessage, LobbyParticipant,
    LobbyParticipantOf, LocalMultiplayerPlayerId, PendingLobby, PublicLobbies,
    ReceivedEnsembleMessage, StartHosting,
};
use bevy_ensemble_webrtc::{BevyEnsembleWebrtcPlugin, JoinWebrtcLobby, RefreshLobbyList};
use serde::{Deserialize, Serialize};

const MOVE_SPEED: f32 = 200.0;
const CIRCLE_RADIUS: f32 = 25.0;

const PLAYER_COLORS: &[Color] = &[
    Color::srgb(0.2, 0.6, 1.0),
    Color::srgb(1.0, 0.3, 0.3),
    Color::srgb(0.3, 1.0, 0.3),
    Color::srgb(1.0, 0.8, 0.2),
    Color::srgb(0.8, 0.3, 1.0),
    Color::srgb(1.0, 0.5, 0.0),
    Color::srgb(0.0, 1.0, 0.8),
    Color::srgb(1.0, 0.4, 0.7),
];

fn player_color(player_uuid: u128) -> Color {
    PLAYER_COLORS[(player_uuid % PLAYER_COLORS.len() as u128) as usize]
}

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
                    handle_j_key,
                    handle_r_key,
                    handle_escape_key,
                    update_menu_text,
                    spawn_local_circle,
                    handle_wasd_input,
                    relay_moves_on_host,
                    receive_player_positions,
                    cleanup_circles_on_lobby_leave,
                    cleanup_disconnected_players,
                ),
            );
    }
}

// -- Messages --

/// Sent by clients to the host with their current position.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
struct MoveIntent {
    x: f32,
    y: f32,
}

/// Broadcast by the host to all clients with a player's position.
#[derive(Message, Clone, Debug, Serialize, Deserialize)]
struct PlayerPosition {
    player_uuid: u128,
    x: f32,
    y: f32,
}

// -- Components --

/// Tags a circle sprite with its owning player's UUID.
#[derive(Component)]
struct PlayerCircle(u128);

/// Marker for the menu text entity.
#[derive(Component)]
struct MenuText;

// -- Setup --

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);
    commands.spawn((
        MenuText,
        Text::new("Press H to host\nPress R to refresh lobbies\nPress J to join first lobby"),
        Node {
            align_self: AlignSelf::Center,
            justify_self: JustifySelf::Center,
            ..default()
        },
    ));
}

// -- Menu / lobby UI --

fn update_menu_text(
    mut menu: Query<(&mut Text, &mut Visibility), With<MenuText>>,
    pending_lobbies: Query<(), With<PendingLobby>>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
    lobby_list: Option<Res<PublicLobbies>>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    lobbies: Query<Entity, With<Lobby>>,
) {
    let Ok((mut text, mut vis)) = menu.single_mut() else {
        return;
    };

    let in_lobby = !host_lobbies.is_empty() || !client_lobbies.is_empty();

    if in_lobby {
        // Show participant roster in top-left
        *vis = Visibility::Visible;
        let lobby_entity = lobbies.iter().next().unwrap();
        let mut lines = vec!["Players:".to_string()];
        for (p, pof) in participants.iter() {
            if pof.0 != lobby_entity {
                continue;
            }
            let mut line = format!("  Player {}", p.player_uuid % 10000);
            if p.is_host {
                line.push_str(" (Host)");
            }
            lines.push(line);
        }
        lines.push(String::new());
        lines.push("WASD to move | Escape to leave".into());
        **text = lines.join("\n");
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
        "Press H to host\nPress R to refresh lobbies\nPress J to join first lobby".to_string();

    if let Some(lobby_list) = &lobby_list {
        if lobby_list.0.is_empty() {
            menu_str.push_str("\n\nNo lobbies available");
        } else {
            menu_str.push_str("\n\nAvailable lobbies:");
            for lobby in &lobby_list.0 {
                menu_str.push_str(&format!(
                    "\n  {} - {}/{} players",
                    lobby.host_name, lobby.player_count, lobby.max_players,
                ));
            }
        }
    }
    **text = menu_str;
}

// -- Lobby management (same pattern as minimal_lobby) --

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

fn handle_j_key(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    lobby_list: Option<Res<PublicLobbies>>,
    mut join_writer: MessageWriter<JoinWebrtcLobby>,
    existing_lobbies: Query<(), With<Lobby>>,
    pending_lobbies: Query<(), With<PendingLobby>>,
) {
    if !keyboard_input.just_pressed(KeyCode::KeyJ) {
        return;
    }
    if !existing_lobbies.is_empty() || !pending_lobbies.is_empty() {
        return;
    }
    let Some(lobby_list) = lobby_list else { return };
    let Some(first) = lobby_list.0.first() else {
        return;
    };
    join_writer.write(JoinWebrtcLobby(first.lobby_id));
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
    // Don't spawn if we already have a circle for the local player
    if existing_circles
        .iter()
        .any(|c| c.0 == local_player.0)
    {
        return;
    }

    let color = player_color(local_player.0);
    commands.spawn((
        PlayerCircle(local_player.0),
        Mesh2d(meshes.add(Circle::new(CIRCLE_RADIUS))),
        MeshMaterial2d(materials.add(ColorMaterial::from_color(color))),
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

    // Move local circle
    let Some((_, mut transform)) = circles
        .iter_mut()
        .find(|(c, _)| c.0 == local_player.0)
    else {
        return;
    };
    transform.translation.x += delta.x;
    transform.translation.y += delta.y;

    let pos_x = transform.translation.x;
    let pos_y = transform.translation.y;

    // Send position to network
    if let Some(host_lobby) = host_lobbies.iter().next() {
        // We are host: broadcast our position to all clients
        let player_uuid = local_player.0;
        commands
            .entity(host_lobby)
            .trigger(move |entity| LobbyMessage::<PlayerPosition> {
                entity,
                message: PlayerPosition {
                    player_uuid,
                    x: pos_x,
                    y: pos_y,
                },
            });
    } else if let Some(client_lobby) = client_lobbies.iter().next() {
        // We are client: send our position to host
        commands
            .entity(client_lobby)
            .trigger(move |entity| LobbyMessage::<MoveIntent> {
                entity,
                message: MoveIntent {
                    x: pos_x,
                    y: pos_y,
                },
            });
    }
}

/// Host receives MoveIntent from clients, updates their circle locally,
/// and broadcasts PlayerPosition to all clients.
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

    // Track spawns within this system run to avoid duplicates from deferred commands
    let mut spawned_this_frame = Vec::new();

    for message in messages.read() {
        let Some(sender) = message.sender else {
            continue;
        };
        let player_uuid = sender;
        let x = message.message.x;
        let y = message.message.y;

        // Update (or spawn) the client's circle on the host
        if let Some((_, mut transform)) = circles
            .iter_mut()
            .find(|(c, _)| c.0 == player_uuid)
        {
            transform.translation.x = x;
            transform.translation.y = y;
        } else if !spawned_this_frame.contains(&player_uuid) {
            let color = player_color(player_uuid);
            commands.spawn((
                PlayerCircle(player_uuid),
                Mesh2d(meshes.add(Circle::new(CIRCLE_RADIUS))),
                MeshMaterial2d(materials.add(ColorMaterial::from_color(color))),
                Transform::from_translation(Vec3::new(x, y, 0.0)),
            ));
            spawned_this_frame.push(player_uuid);
        }

        // Broadcast to all clients
        commands
            .entity(host_lobby)
            .trigger(move |entity| LobbyMessage::<PlayerPosition> {
                entity,
                message: PlayerPosition {
                    player_uuid,
                    x,
                    y,
                },
            });
    }
}

/// Receive PlayerPosition broadcasts: spawn or update remote player circles.
fn receive_player_positions(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    mut messages: MessageReader<ReceivedEnsembleMessage<PlayerPosition>>,
    mut circles: Query<(&PlayerCircle, &mut Transform)>,
) {
    // Track spawns within this system run to avoid duplicates from deferred commands
    let mut spawned_this_frame = Vec::new();

    for message in messages.read() {
        let pos = &message.message;

        // Skip our own position — we already moved locally
        if let Some(ref local) = local_player {
            if pos.player_uuid == local.0 {
                continue;
            }
        }

        // Find existing circle or spawn a new one
        if let Some((_, mut transform)) = circles
            .iter_mut()
            .find(|(c, _)| c.0 == pos.player_uuid)
        {
            transform.translation.x = pos.x;
            transform.translation.y = pos.y;
        } else if !spawned_this_frame.contains(&pos.player_uuid) {
            let color = player_color(pos.player_uuid);
            commands.spawn((
                PlayerCircle(pos.player_uuid),
                Mesh2d(meshes.add(Circle::new(CIRCLE_RADIUS))),
                MeshMaterial2d(materials.add(ColorMaterial::from_color(color))),
                Transform::from_translation(Vec3::new(pos.x, pos.y, 0.0)),
            ));
            spawned_this_frame.push(pos.player_uuid);
        }
    }
}

/// Remove all circles when we leave the lobby (Escape or host disconnect).
fn cleanup_circles_on_lobby_leave(
    mut commands: Commands,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
    circles: Query<(Entity, &PlayerCircle)>,
) {
    if !lobbies.is_empty() || circles.is_empty() {
        return;
    }
    // No lobby exists but circles remain — clean them up
    for (entity, _) in circles.iter() {
        commands.entity(entity).try_despawn();
    }
}

/// On the host, remove circles for players who disconnected.
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
        // Never remove local player's circle
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
