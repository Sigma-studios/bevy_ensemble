//! Finding out at the join whether two peers speak the same protocol.
//!
//! A message travels as a type index and a postcard payload. If two builds registered
//! different sets of wire names, the same index means different types on each side, and
//! postcard will happily decode one as the other whenever the byte lengths happen to work —
//! there is no error anywhere, only a session that stops agreeing with itself in ways that look
//! like everything except what they are.
//!
//! So each side states its [`wire_hash`](crate::EnsembleMessageRegistry::wire_hash) as the first
//! thing it says. The host tells each client as its [`LobbyClient`] appears; a client tells the
//! host as its [`Lobby`] appears. A match marks the counterpart [`HandshakeVerified`], which is
//! what anything that needs a verified peer should wait for. A mismatch ends the join, on both
//! sides, with the first differing wire name in the message, so the answer is "rebuild peer B
//! with `Foo` registered" and not two 64-bit numbers.
//!
//! This lives in the core rather than in each backend so that every backend — the loopback
//! harness included — gets exactly the same check, and so that it can be tested without a
//! socket.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    EnsembleMessageRegistry, Host, HostUuid, Lobby, LobbyClient, LobbyClientPlayerUuid,
    LobbyJoinFailed, LobbyLeft, LobbyLeftReason, LocalMultiplayerPlayerId, ReceivedEnsembleMessage,
    SendMode,
    messages::{LobbyClientMessage, LobbyMessage},
    registry::{HeldUntilVerified, PROTOCOL_VERSION, decode_verified_packet},
};

/// What each peer says about its protocol, once, as the first thing it says.
///
/// The names travel alongside the hash only to make the error useful. They prove nothing the
/// hash does not, but "yours registers `Foo` and mine does not" is a sentence somebody can act
/// on, where two numbers that differ are not.
#[doc(hidden)]
#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolHandshake {
    pub version: u32,
    pub hash: u64,
    pub names: Vec<String>,
}

impl ProtocolHandshake {
    fn of(registry: &EnsembleMessageRegistry) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            hash: registry.wire_hash(),
            names: registry
                .wire_names()
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }
    }

    /// The first thing that differs between two registries, in words.
    fn difference(&self, other: &Self) -> String {
        if self.version != other.version {
            return format!(
                "protocol version {} here, {} there",
                self.version, other.version
            );
        }
        let ours: std::collections::BTreeSet<&str> =
            self.names.iter().map(String::as_str).collect();
        let theirs: std::collections::BTreeSet<&str> =
            other.names.iter().map(String::as_str).collect();
        if let Some(name) = ours.difference(&theirs).next() {
            return format!("`{name}` is registered here and not there");
        }
        if let Some(name) = theirs.difference(&ours).next() {
            return format!("`{name}` is registered there and not here");
        }
        "the registries look alike but hash differently".to_owned()
    }
}

/// On a client's lobby entity and on a host's `LobbyClient` entities: the peer on the other end
/// has stated a protocol that matches this one's. Wait for it before treating a peer as able to
/// understand anything you send.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct HandshakeVerified;

/// Peers whose protocol matched before this side had an entity to mark: a client's handshake
/// that reached the host in the frame before the host promoted it, or a host's that reached
/// the client while its lobby was still pending. Consumed by [`verify_promoted_peers`].
///
/// Without it a join could fail on timing alone. The handshake is sent once, on promotion;
/// the two promotions happen in whichever order the two ready handshakes land, and a
/// handshake that found no seat was dropped and never resent.
#[derive(Resource, Debug, Default)]
pub(crate) struct ProtocolMatched(std::collections::HashSet<u128>);

/// State our protocol to each new peer, once.
pub(crate) fn announce_protocol(
    mut commands: Commands,
    registry: Res<EnsembleMessageRegistry>,
    new_clients: Query<Entity, Added<LobbyClient>>,
    new_client_lobbies: Query<Entity, (Added<Lobby>, Without<Host>)>,
) {
    if new_clients.is_empty() && new_client_lobbies.is_empty() {
        return;
    }
    let ours = ProtocolHandshake::of(&registry);
    for client in new_clients.iter() {
        let message = ours.clone();
        commands
            .entity(client)
            .trigger(move |entity| LobbyClientMessage {
                entity,
                message,
                send_mode: SendMode::Reliable,
            });
    }
    for lobby in new_client_lobbies.iter() {
        let message = ours.clone();
        commands.entity(lobby).trigger(move |entity| LobbyMessage {
            entity,
            message,
            send_mode: SendMode::Reliable,
        });
    }
}

/// Everything a peer said before its handshake was compared is decoded now that it has been.
///
/// An observer on the marker rather than part of `verify_protocol`, because the marker is
/// inserted through commands and the decoder checks for it on the entity: replaying before the
/// insert has applied would hold every packet again.
pub(crate) fn replay_held_packets(
    verified: On<Add, HandshakeVerified>,
    clients: Query<&LobbyClientPlayerUuid, With<LobbyClient>>,
    host: Option<Res<HostUuid>>,
    mut commands: Commands,
) {
    let sender = match clients.get(verified.entity) {
        Ok(uuid) => uuid.0,
        Err(_) => match host {
            Some(host) => host.0,
            None => return,
        },
    };
    commands.queue(move |world: &mut World| {
        let held = world
            .get_resource_mut::<HeldUntilVerified>()
            .map(|mut held| held.take(sender))
            .unwrap_or_default();
        for (packet, received_at) in held {
            decode_verified_packet(world, Some(sender), &packet, received_at);
        }
    });
}

/// A seat that appears after its peer's handshake already matched is verified now.
pub(crate) fn verify_promoted_peers(
    mut commands: Commands,
    mut matched: ResMut<ProtocolMatched>,
    promoted_clients: Query<(Entity, &LobbyClientPlayerUuid), Added<LobbyClient>>,
    promoted_lobbies: Query<Entity, (Added<Lobby>, Without<Host>)>,
    host: Option<Res<HostUuid>>,
) {
    if matched.0.is_empty() {
        return;
    }
    for (client, uuid) in promoted_clients.iter() {
        if matched.0.remove(&uuid.0) {
            commands.entity(client).try_insert(HandshakeVerified);
        }
    }
    if let Some(host) = host {
        for lobby in promoted_lobbies.iter() {
            if matched.0.remove(&host.0) {
                commands.entity(lobby).try_insert(HandshakeVerified);
            }
        }
    }
}

/// Compare what arrived against what we hold; verify on a match, end the join otherwise.
pub(crate) fn verify_protocol(
    mut commands: Commands,
    registry: Res<EnsembleMessageRegistry>,
    mut messages: MessageReader<ReceivedEnsembleMessage<ProtocolHandshake>>,
    mut held: ResMut<HeldUntilVerified>,
    mut matched: ResMut<ProtocolMatched>,
    host_lobby: Option<Single<Entity, (With<Lobby>, With<Host>)>>,
    client_lobby: Option<Single<Entity, (With<Lobby>, Without<Host>)>>,
    lobby_clients: Query<(Entity, &LobbyClientPlayerUuid), With<LobbyClient>>,
    mut join_failed: MessageWriter<LobbyJoinFailed>,
    mut left: MessageWriter<LobbyLeft>,
) {
    let mut ours = None;
    for message in messages.read() {
        let Some(sender) = message.sender else {
            continue;
        };
        let ours = ours.get_or_insert_with(|| ProtocolHandshake::of(&registry));
        let theirs = &message.message;

        if theirs.version == ours.version && theirs.hash == ours.hash {
            if host_lobby.is_some() {
                match lobby_clients.iter().find(|(_, uuid)| uuid.0 == sender) {
                    Some((client, _)) => {
                        commands.entity(client).try_insert(HandshakeVerified);
                    }
                    None => {
                        matched.0.insert(sender);
                    }
                }
            } else if let Some(lobby) = client_lobby.as_ref() {
                commands.entity(**lobby).try_insert(HandshakeVerified);
            } else {
                matched.0.insert(sender);
            }
            continue;
        }

        let difference = ours.difference(theirs);
        held.discard(sender);
        if host_lobby.is_some() {
            error!(
                "refusing client {sender:#x}: its protocol does not match ({difference}). Build \
                 both peers from the same commit."
            );
            if let Some((client, _)) = lobby_clients.iter().find(|(_, uuid)| uuid.0 == sender) {
                commands.entity(client).try_despawn();
            }
            join_failed.write(LobbyJoinFailed {
                reason: format!("A player's build does not match this one: {difference}"),
            });
        } else if let Some(lobby) = client_lobby.as_ref() {
            error!(
                "leaving: the host's protocol does not match ({difference}). Build both peers \
                 from the same commit."
            );
            commands.entity(**lobby).try_despawn();
            commands.remove_resource::<LocalMultiplayerPlayerId>();
            join_failed.write(LobbyJoinFailed {
                reason: format!("Your build does not match the host's: {difference}"),
            });
            left.write(LobbyLeft {
                reason: LobbyLeftReason::ProtocolMismatch(difference),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake(names: &[&str]) -> ProtocolHandshake {
        ProtocolHandshake {
            version: PROTOCOL_VERSION,
            hash: names.len() as u64,
            names: names.iter().map(|n| (*n).to_owned()).collect(),
        }
    }

    #[test]
    fn the_difference_names_the_first_registration_that_differs() {
        let ours = handshake(&["A", "B", "C"]);
        let theirs = handshake(&["A", "C"]);
        assert_eq!(
            ours.difference(&theirs),
            "`B` is registered here and not there"
        );
        assert_eq!(
            theirs.difference(&ours),
            "`B` is registered there and not here"
        );
    }

    #[test]
    fn a_version_difference_is_named_first() {
        let ours = handshake(&["A"]);
        let mut theirs = handshake(&["A"]);
        theirs.version += 1;
        assert!(ours.difference(&theirs).starts_with("protocol version"));
    }
}
