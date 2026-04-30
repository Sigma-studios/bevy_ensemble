use bevy::prelude::*;
use bevy_immediate::{BevyImmediatePlugin, ImmCtx, ui::CapsUi};
use bevy_ensemble::{
    Host, Lobby, LobbyClient, LobbyMessage, LobbyParticipant, LobbyParticipantOf,
    LocalMultiplayerPlayerId, EnsembleAppExt, EnsemblePlugin, PendingLobby,
    ReceivedEnsembleMessage, StartHosting,
};
use bevy_ensemble_steam::{
    BevyEnsembleSteamPlugin, LobbyClientSteamId, LobbySteamId,
};
use bevy_steamworks::{Client as SteamClient, SteamId};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(EnsemblePlugin)
        .add_plugins(BevyEnsembleSteamPlugin::default())
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
    steam_client: Res<SteamClient>,
    pending_lobbies: Query<(), With<PendingLobby>>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    client_lobbies: Query<Entity, (With<Lobby>, Without<Host>)>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    lobby_clients: Query<(&bevy_ensemble::crate_steam_bridge::LobbyClientPlayerUuid, &LobbyClientSteamId), With<LobbyClient>>,
    client_lobby_ids: Query<&LobbySteamId, (With<Lobby>, Without<Host>)>,
    chat_log: Res<ChatLog>,
) {
    let mut root = ctx.build_immediate_root("minimal_lobby");

    if !pending_lobbies.is_empty() {
        root.ch_id("loading")
            .on_change_insert(true, || {
                (
                    Node {
                        align_self: AlignSelf::Center,
                        justify_self: JustifySelf::Center,
                        ..default()
                    },
                    Text::new("Loading..."),
                )
            });
        return;
    }

    let ready_lobby = host_lobbies
        .iter()
        .next()
        .or_else(|| client_lobbies.iter().next());

    let Some(lobby_entity) = ready_lobby else {
        root.ch_id("menu")
            .on_change_insert(true, || {
                (
                    Node {
                        align_self: AlignSelf::Center,
                        justify_self: JustifySelf::Center,
                        ..default()
                    },
                    Text::new("Host: Press H\nJoin: Use the steam overlay"),
                )
            });
        return;
    };

    let roster_text = build_roster_text(
        &steam_client,
        lobby_entity,
        &participants,
        &lobby_clients,
        &client_lobby_ids,
    );
    let messages_text = if chat_log.0.is_empty() {
        "Messages:\n".to_string()
    } else {
        format!("Messages:\n{}", chat_log.0.iter().cloned().collect::<Vec<_>>().join("\n"))
    };

    let lobby_info_text = format!(
        "Players:\n{}\n\nPress H to send hello\nPress Escape to exit the lobby",
        roster_text
    );
    root.ch_id("lobby_info")
        .on_change_insert(true, move || {
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
    root.ch_id("messages")
        .on_change_insert(true, move || {
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
    steam_client: Res<SteamClient>,
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
        let message = DisplayChatMessage {
            sender_name: steam_client.friends().name(),
            text: "Hello".to_string(),
        };
        push_chat_message(&mut chat_log, &message.sender_name, &message.text);
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

fn relay_hello_messages_on_host(
    mut commands: Commands,
    steam_client: Res<SteamClient>,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    lobby_clients: Query<
        (&bevy_ensemble::crate_steam_bridge::LobbyClientPlayerUuid, &LobbyClientSteamId),
        With<LobbyClient>,
    >,
    client_lobby_ids: Query<&LobbySteamId, (With<Lobby>, Without<Host>)>,
    mut messages: MessageReader<ReceivedEnsembleMessage<HelloIntent>>,
    mut chat_log: ResMut<ChatLog>,
) {
    let Some(host_lobby) = host_lobbies.iter().next() else {
        return;
    };

    for message in messages.read() {
        let sender_name = message
            .sender
            .and_then(|sender| {
                steam_name_for_player_uuid(&steam_client, sender, &lobby_clients, &client_lobby_ids)
            })
            .unwrap_or_else(|| "Unknown".to_string());
        let display = DisplayChatMessage {
            sender_name,
            text: "Hello".to_string(),
        };
        push_chat_message(&mut chat_log, &display.sender_name, &display.text);
        commands.entity(host_lobby).trigger(move |entity| {
            LobbyMessage::<DisplayChatMessage> {
                entity,
                message: display,
            }
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
    steam_client: Res<SteamClient>,
    host_lobbies: Query<(Entity, &LobbySteamId), (With<Lobby>, With<Host>)>,
    client_lobbies: Query<(Entity, &LobbySteamId), (With<Lobby>, Without<Host>)>,
    pending_lobbies: Query<Entity, With<PendingLobby>>,
    lobby_clients: Query<(Entity, &LobbyClientSteamId), With<LobbyClient>>,
    mut chat_log: ResMut<ChatLog>,
) {
    if !keyboard_input.just_pressed(KeyCode::Escape) {
        return;
    }

    for entity in pending_lobbies.iter() {
        commands.entity(entity).try_despawn();
    }

    if let Some((host_entity, lobby_id)) = host_lobbies.iter().next() {
        for (client_entity, client_steam_id) in lobby_clients.iter() {
            steam_client.networking().close_p2p_session(client_steam_id.0);
            commands.entity(client_entity).try_despawn();
        }
        steam_client.matchmaking().leave_lobby(lobby_id.0);
        commands.entity(host_entity).try_despawn();
    }

    if let Some((client_entity, lobby_id)) = client_lobbies.iter().next() {
        let host = steam_client.matchmaking().lobby_owner(lobby_id.0);
        steam_client.networking().close_p2p_session(host);
        steam_client.matchmaking().leave_lobby(lobby_id.0);
        commands.entity(client_entity).try_despawn();
    }

    commands.remove_resource::<LocalMultiplayerPlayerId>();
    chat_log.0.clear();
}

fn build_roster_text(
    steam_client: &SteamClient,
    lobby_entity: Entity,
    participants: &Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    lobby_clients: &Query<
        (&bevy_ensemble::crate_steam_bridge::LobbyClientPlayerUuid, &LobbyClientSteamId),
        With<LobbyClient>,
    >,
    client_lobby_ids: &Query<&LobbySteamId, (With<Lobby>, Without<Host>)>,
) -> String {
    let mut players = Vec::new();

    for (participant, participant_of) in participants.iter() {
        if participant_of.0 != lobby_entity {
            continue;
        }

        let mut line =
            steam_name_for_player_uuid(steam_client, participant.player_uuid, lobby_clients, client_lobby_ids)
                .unwrap_or_else(|| participant.player_uuid.to_string());
        if participant.is_host {
            line.push_str(" (Host)");
        }
        players.push(line);
    }

    if players.is_empty() {
        "(No players)".to_string()
    } else {
        players.join("\n")
    }
}

fn steam_name_for_player_uuid(
    steam_client: &SteamClient,
    player_uuid: u128,
    lobby_clients: &Query<
        (&bevy_ensemble::crate_steam_bridge::LobbyClientPlayerUuid, &LobbyClientSteamId),
        With<LobbyClient>,
    >,
    client_lobby_ids: &Query<&LobbySteamId, (With<Lobby>, Without<Host>)>,
) -> Option<String> {
    if let Some((_, steam_id)) = lobby_clients
        .iter()
        .find(|(client_player_uuid, _)| client_player_uuid.0 == player_uuid)
    {
        return Some(steam_client.friends().get_friend(steam_id.0).name());
    }

    if let Some(lobby_steam_id) = client_lobby_ids.iter().next() {
        let host_steam_id = steam_client.matchmaking().lobby_owner(lobby_steam_id.0);
        if u128::from(host_steam_id.raw()) == player_uuid {
            return Some(steam_client.friends().get_friend(host_steam_id).name());
        }
    }

    let steam_id = SteamId::from_raw(u64::try_from(player_uuid).ok()?);
    Some(steam_client.friends().get_friend(steam_id).name())
}

fn push_chat_message(chat_log: &mut ChatLog, sender_name: &str, text: &str) {
    while chat_log.0.len() >= 8 {
        chat_log.0.pop_front();
    }
    chat_log.0.push_back(format!("{sender_name}: {text}"));
}
