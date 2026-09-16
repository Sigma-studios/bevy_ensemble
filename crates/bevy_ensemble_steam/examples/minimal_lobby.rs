use bevy::prelude::*;
use bevy_ensemble::{
    AwaitingHost, BroadcastLobbyMessage, CloseLobby, EnsembleAppExt, EnsemblePlugin, Host,
    HostChanged, LeaveLobby, Lobby, LobbyBroadcastAppExt, LobbyBroadcastPlugin, LobbyClient,
    LobbyMessage, LobbyParticipant, LobbyParticipantOf, LocalMultiplayerPlayerId, PendingLobby,
    ReceivedEnsembleMessage, StartHosting,
};
use bevy_ensemble_steam::{BevyEnsembleSteamPlugin, LobbyClientSteamId, LobbySteamId};
use bevy_immediate::{BevyImmediatePlugin, ImmCtx, ui::CapsUi};
use bevy_steamworks::{Client as SteamClient, SteamId};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(EnsemblePlugin)
        .add_plugins(LobbyBroadcastPlugin)
        .add_plugins(BevyEnsembleSteamPlugin::default())
        .add_plugins(BevyImmediatePlugin::<CapsUi>::new())
        .add_plugins(MinimalLobbyExamplePlugin)
        .run();
}

struct MinimalLobbyExamplePlugin;

impl Plugin for MinimalLobbyExamplePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ChatLog>()
            .register_broadcast_message::<ChatMessage>("ChatMessage")
            .register_ensemble_message_type::<WaveAction>("WaveAction")
            .add_systems(Startup, setup_camera)
            .add_systems(
                Update,
                (
                    render_ui,
                    handle_h_key,
                    handle_t_key,
                    receive_chat_messages,
                    receive_wave_actions,
                    handle_escape_key,
                    handle_c_key,
                    receive_host_changes,
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
    lobby_clients: Query<
        (&bevy_ensemble::LobbyClientPlayerUuid, &LobbyClientSteamId),
        With<LobbyClient>,
    >,
    client_lobby_ids: Query<&LobbySteamId, (With<Lobby>, Without<Host>)>,
    awaiting: Query<&AwaitingHost>,
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
        root.ch_id("menu").on_change_insert(true, || {
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
        format!(
            "Messages:\n{}",
            chat_log.0.iter().cloned().collect::<Vec<_>>().join("\n")
        )
    };

    // The host is gone and Steam has not named a new owner yet, or the new one is being reached.
    let status = match awaiting.get(lobby_entity) {
        Ok(awaiting) if awaiting.successor.is_none() => format!(
            "Host left - waiting for Steam to name a new host ({:.0}s)\n\n",
            awaiting.waited.as_secs_f64()
        ),
        Ok(awaiting) => format!(
            "Host left - reaching the new host ({:.0}s)\n\n",
            awaiting.waited.as_secs_f64()
        ),
        Err(_) => String::new(),
    };
    let lobby_info_text = format!(
        "{status}Players:\n{}\n\nPress H to send hello (broadcast)\nPress Escape to exit the lobby\n{}",
        roster_text,
        if host_lobbies.get(lobby_entity).is_ok() {
            "Press T to wave (host-only action)\nPress C to close the lobby for everyone"
        } else {
            ""
        }
    );
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
    steam_client: Res<SteamClient>,
    mut start_hosting: MessageWriter<StartHosting>,
    lobbies: Query<Entity, With<Lobby>>,
    pending_lobbies: Query<(), With<PendingLobby>>,
) {
    if !keyboard_input.just_pressed(KeyCode::KeyH) {
        return;
    }

    if let Some(lobby) = lobbies.iter().next() {
        commands.entity(lobby).trigger(|entity| {
            BroadcastLobbyMessage::new(
                entity,
                ChatMessage {
                    sender_name: steam_client.friends().name(),
                    text: "Hello".to_string(),
                },
            )
        });
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
    steam_client: Res<SteamClient>,
    lobby_clients: Query<
        (&bevy_ensemble::LobbyClientPlayerUuid, &LobbyClientSteamId),
        With<LobbyClient>,
    >,
    client_lobby_ids: Query<&LobbySteamId, (With<Lobby>, Without<Host>)>,
    mut chat_log: ResMut<ChatLog>,
) {
    for message in messages.read() {
        let sender_name = message
            .sender
            .and_then(|sender| {
                steam_name_for_player_uuid(&steam_client, sender, &lobby_clients, &client_lobby_ids)
            })
            .unwrap_or_else(|| "Unknown".to_string());
        push_chat_message(&mut chat_log, &sender_name, "waves");
    }
}

fn handle_escape_key(
    mut commands: Commands,
    keyboard_input: Res<ButtonInput<KeyCode>>,
    mut leave: MessageWriter<LeaveLobby>,
    mut chat_log: ResMut<ChatLog>,
) {
    if !keyboard_input.just_pressed(KeyCode::Escape) {
        return;
    }

    // Closing the sessions and leaving the Steam lobby is the backend's to do, in its order.
    leave.write(LeaveLobby);
    commands.remove_resource::<LocalMultiplayerPlayerId>();
    chat_log.0.clear();
}

/// Ends the lobby for everyone rather than handing it to the next owner Steam picks.
fn handle_c_key(
    keyboard_input: Res<ButtonInput<KeyCode>>,
    hosting: Query<(), (With<Lobby>, With<Host>)>,
    mut close: MessageWriter<CloseLobby>,
    mut chat_log: ResMut<ChatLog>,
) {
    if !keyboard_input.just_pressed(KeyCode::KeyC) || hosting.is_empty() {
        return;
    }
    close.write(CloseLobby);
    chat_log.0.clear();
}

/// The lobby is the same one under the owner Steam picked; the roster carries over.
fn receive_host_changes(
    steam_client: Res<SteamClient>,
    mut changes: MessageReader<HostChanged>,
    mut chat_log: ResMut<ChatLog>,
) {
    for change in changes.read() {
        let name = |uuid: u128| {
            u64::try_from(uuid)
                .map(|raw| {
                    steam_client
                        .friends()
                        .get_friend(SteamId::from_raw(raw))
                        .name()
                })
                .unwrap_or_else(|_| uuid.to_string())
        };
        let text = if change.promoted {
            format!("{} left; you host the lobby now", name(change.previous))
        } else {
            format!(
                "{} left; {} hosts the lobby now",
                name(change.previous),
                name(change.new)
            )
        };
        push_chat_message(&mut chat_log, "lobby", &text);
    }
}

fn build_roster_text(
    steam_client: &SteamClient,
    lobby_entity: Entity,
    participants: &Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    lobby_clients: &Query<
        (&bevy_ensemble::LobbyClientPlayerUuid, &LobbyClientSteamId),
        With<LobbyClient>,
    >,
    client_lobby_ids: &Query<&LobbySteamId, (With<Lobby>, Without<Host>)>,
) -> String {
    let mut players = Vec::new();

    for (participant, participant_of) in participants.iter() {
        if participant_of.0 != lobby_entity {
            continue;
        }

        let mut line = steam_name_for_player_uuid(
            steam_client,
            participant.player_uuid,
            lobby_clients,
            client_lobby_ids,
        )
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
        (&bevy_ensemble::LobbyClientPlayerUuid, &LobbyClientSteamId),
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
