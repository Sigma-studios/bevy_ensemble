use std::sync::Mutex;

use bevy::prelude::*;
use bevy_ensemble_sockets::PeerSignal;
use tokio::sync::mpsc;

use crate::protocol::{
    CAPABILITY_TYPED_REQUESTS, ClientMessage, RequestId, ServerMessage, SignallingError,
};

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod wasm;

#[cfg(target_arch = "wasm32")]
pub(crate) use self::wasm::WsHandlerBuilder;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::WsHandlerBuilder;

/// Lobby-specific events forwarded to Bevy systems (not signaling events).
#[derive(Message, Debug)]
pub(crate) enum LobbyEvent {
    Welcome {
        player_uuid: u128,
    },
    /// The lobby `request` asked for exists, and this peer hosts it.
    LobbyCreated {
        request: RequestId,
        lobby_id: u64,
        code: String,
    },
    /// This peer is in the lobby `request` asked to join.
    LobbyJoined {
        request: RequestId,
        lobby_id: u64,
        /// Who runs the lobby that was joined. The one fact a client's trust rests on: it is the
        /// only peer whose offer is answered, whose packets are read, and whose loss ends the
        /// session.
        host_uuid: u128,
        /// The other members at the time of joining. Informational: a client connects to its
        /// host only, never to them.
        existing_members: Vec<u128>,
        /// The lobby's code, for the joiner to show and pass on as a host would.
        code: String,
    },
    /// The server refused `request`, or with `None`, a frame that carried no request.
    Refused {
        request: Option<RequestId>,
        error: SignallingError,
    },
    /// The server does not speak [`CAPABILITY_TYPED_REQUESTS`], so it cannot decode a single
    /// lobby request this client sends. Said once per connection.
    ServerOutdated,
    PlayerJoined {
        player_uuid: u128,
    },
    PlayerLeft {
        player_uuid: u128,
    },
    LobbyList {
        lobbies: Vec<crate::protocol::LobbyInfo>,
    },
    Disconnected {
        reason: String,
    },
    /// The lobby this peer created or joined outlives its host. See
    /// [`ServerMessage::LobbyMigratable`].
    LobbyMigratable {
        idle_timeout_secs: u32,
    },
    /// The host left and `new_host` hosts the lobby now. See [`ServerMessage::HostChanged`].
    HostChanged {
        lobby_id: u64,
        previous_host: u128,
        new_host: u128,
        code: String,
        members: Vec<u128>,
    },
    /// The WebSocket to the signalling server is gone: it closed, errored, or never opened.
    ///
    /// Not sent when this side dropped the connection itself (a rebuild after leaving a lobby)
    /// — that is a teardown already under way, not a loss.
    SignallingClosed,
}

/// Resource for lobby-level communication with the signaling server.
///
/// Lobby commands (create, join, leave, list) flow through this resource.
/// Incoming WebRTC signals arrive via `signal_rx`.
#[derive(Resource)]
pub struct LobbyConnection {
    pub command_tx: mpsc::UnboundedSender<ClientMessage>,
    pub event_rx: Mutex<mpsc::UnboundedReceiver<LobbyEvent>>,
    pub signal_rx: Mutex<mpsc::UnboundedReceiver<(u128, PeerSignal)>>,
    pub local_player_uuid: Option<u128>,
    /// Set once [`LobbyEvent::SignallingClosed`] has been seen: this connection can carry nothing
    /// more, and a fresh one has to be built before hosting or joining again.
    pub signalling_lost: bool,
    /// The name this connection authenticated with: what the server believes, on this socket, until
    /// a `SetDisplayName` changes it.
    ///
    /// Here rather than in a `Local` beside the system that publishes it, because it is a property
    /// of the connection and should die with it. A `Local` outlives the socket, and both of the
    /// ways this connection gets rebuilt — leaving a lobby, recovering a dropped signalling socket
    /// — then leave it describing a conversation that is over, so the name is never re-sent and
    /// the server keeps whatever the fresh socket authenticated with.
    pub announced_name: String,
    /// Set once [`LobbyEvent::ServerOutdated`] has been seen: every lobby request on this
    /// connection is refused here, with a reason, instead of being sent to a server that would
    /// drop it unread.
    pub server_outdated: bool,
    /// Requests sent on this connection that could still be refused for being over a rate limit,
    /// and so sent again. See [`LobbyConnection::send_request`].
    pub(crate) unanswered: bevy::platform::collections::HashMap<RequestId, Unanswered>,
    /// When this connection next tells the server it is still here, in `Time::elapsed` seconds.
    /// Zero, which is at once, for a new connection.
    pub(crate) next_keep_alive_at: f64,
}

/// A request sent and not yet answered, kept so that a [`SignallingError::RateLimited`] refusal
/// of it can be answered by sending it again rather than by giving up.
pub(crate) struct Unanswered {
    pub message: ClientMessage,
    pub sent_at: bevy_ensemble::Instant,
    /// When it is due to be sent again, once refused for its rate.
    pub retry_at: Option<bevy_ensemble::Instant>,
    pub retries: u8,
}

/// The next request id, across every connection this process opens.
///
/// Process-wide rather than per connection because a request outlives the connection it was sent
/// on, as far as the entity waiting on it is concerned: an id reused by the rebuilt connection
/// could be taken for an answer to an attempt that was already over.
static NEXT_REQUEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

impl LobbyConnection {
    /// Send a request, under a fresh id that is returned, and remember it until it is answered
    /// or forgotten.
    pub(crate) fn send_request(
        &mut self,
        build: impl FnOnce(RequestId) -> ClientMessage,
    ) -> RequestId {
        let request = NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let message = build(request);
        let _ = self.command_tx.send(message.clone());
        self.unanswered.insert(
            request,
            Unanswered {
                message,
                sent_at: bevy_ensemble::Instant::now(),
                retry_at: None,
                retries: 0,
            },
        );
        request
    }

    /// Schedule `request`, just refused for its rate, to be sent again. False when it is not
    /// being kept, or has been sent again as often as it will be: then the refusal stands.
    pub(crate) fn retry_later(&mut self, request: RequestId) -> bool {
        let Some(unanswered) = self.unanswered.get_mut(&request) else {
            return false;
        };
        if unanswered.retries >= crate::systems::RATE_LIMITED_RETRIES {
            return false;
        }
        unanswered.retries += 1;
        unanswered.retry_at =
            Some(bevy_ensemble::Instant::now() + crate::systems::RATE_LIMITED_RETRY_DELAY);
        true
    }

    /// `request` is settled, one way or another: it will never be sent again.
    pub(crate) fn forget(&mut self, request: RequestId) {
        self.unanswered.remove(&request);
    }

    /// Take back `request`, which this side has given up on. See
    /// [`ClientMessage::CancelRequest`].
    ///
    /// Kept like a request, under the id it cancels — which is the id a refusal of it names — so
    /// that a cancel refused for its rate is sent again. Lost, it would leave the server holding
    /// this connection in the lobby it was meant to take back.
    pub(crate) fn cancel(&mut self, request: RequestId) {
        let message = ClientMessage::CancelRequest { request };
        let _ = self.command_tx.send(message.clone());
        self.unanswered.insert(
            request,
            Unanswered {
                message,
                sent_at: bevy_ensemble::Instant::now(),
                retry_at: None,
                retries: 0,
            },
        );
    }

    /// Send `signals` for `peer`, behind any batch for the same peer that is waiting to be sent
    /// again after a refusal for its rate.
    ///
    /// Behind it, not beside it: the waiting batch is the older, and may hold the offer that the
    /// newer candidates mean nothing without — a candidate that reaches a peer before its offer is
    /// dropped. Sent on its own, a small new batch could fit the budget the big refused one did
    /// not, and overtake it. So new signals join the waiting batch and go out with it, in order.
    ///
    /// What this cannot catch is a batch sent in the round trip before the refusal of the one
    /// ahead of it has arrived; with signalling on its own budget, sized for a whole lobby at
    /// once, that takes a client already far over it.
    pub(crate) fn send_signals(&mut self, peer: u128, signals: Vec<String>) {
        let waiting = self
            .unanswered
            .iter_mut()
            .filter(|(_, unanswered)| {
                unanswered.retry_at.is_some()
                    && matches!(
                        unanswered.message,
                        ClientMessage::Signals { receiver_uuid, .. } if receiver_uuid == peer
                    )
            })
            .max_by_key(|(request, _)| **request);
        if let Some((_, unanswered)) = waiting
            && let ClientMessage::Signals { signals: held, .. } = &mut unanswered.message
        {
            held.extend(signals);
            return;
        }
        self.send_request(|request| ClientMessage::Signals {
            request,
            receiver_uuid: peer,
            signals,
        });
    }
}

/// What the WebSocket task has learnt about the server's side of the protocol, on this connection.
#[derive(Default)]
pub(crate) struct ServerKnowledge {
    /// A [`ServerMessage::ServerHello`] offering [`CAPABILITY_TYPED_REQUESTS`] has arrived.
    hello: bool,
    /// [`LobbyEvent::ServerOutdated`] has been sent; it is said once.
    outdated: bool,
}

/// Dispatch a decoded server message to the lobby event and/or signal channels.
///
/// `knowledge` is what this connection has learnt of the server so far, and is how an older server
/// is noticed: the client's first queued command is always `ListLobbies`, sent after its
/// capabilities, so a server that has the typed requests says hello before it answers with the
/// listing. A listing with no hello before it is a server that does not.
pub(crate) fn dispatch_server_message(
    server_msg: ServerMessage,
    signal_tx: &mpsc::UnboundedSender<(u128, PeerSignal)>,
    lobby_event_tx: &mpsc::UnboundedSender<LobbyEvent>,
    knowledge: &mut ServerKnowledge,
) {
    let outdated = |knowledge: &mut ServerKnowledge| {
        if !knowledge.outdated {
            knowledge.outdated = true;
            let _ = lobby_event_tx.send(LobbyEvent::ServerOutdated);
        }
    };
    let signal = |sender_uuid: u128, data: &str| match serde_json::from_str::<PeerSignal>(data) {
        Ok(peer_signal) => {
            let _ = signal_tx.send((sender_uuid, peer_signal));
        }
        Err(e) => {
            warn!("Failed to parse PeerSignal from {sender_uuid}: {e}");
        }
    };
    match server_msg {
        ServerMessage::Welcome { player_uuid } => {
            let _ = lobby_event_tx.send(LobbyEvent::Welcome { player_uuid });
        }
        ServerMessage::PlayerJoined { player_uuid } => {
            let _ = lobby_event_tx.send(LobbyEvent::PlayerJoined { player_uuid });
        }
        ServerMessage::PlayerLeft { player_uuid } => {
            let _ = lobby_event_tx.send(LobbyEvent::PlayerLeft { player_uuid });
        }

        ServerMessage::Signal { sender_uuid, data } => signal(sender_uuid, &data),
        ServerMessage::Signals {
            sender_uuid,
            signals,
        } => {
            for data in &signals {
                signal(sender_uuid, data);
            }
        }

        ServerMessage::ServerHello { capabilities } => {
            if capabilities & CAPABILITY_TYPED_REQUESTS != 0 {
                knowledge.hello = true;
            } else {
                outdated(knowledge);
            }
        }
        ServerMessage::LobbyCreatedFor {
            request,
            lobby_id,
            code,
        } => {
            let _ = lobby_event_tx.send(LobbyEvent::LobbyCreated {
                request,
                lobby_id,
                code,
            });
        }
        ServerMessage::LobbyJoinedFor {
            request,
            lobby_id,
            host_uuid,
            existing_members,
            code,
        } => {
            let _ = lobby_event_tx.send(LobbyEvent::LobbyJoined {
                request,
                lobby_id,
                host_uuid,
                existing_members,
                code,
            });
        }
        ServerMessage::Refused { request, error } => {
            let _ = lobby_event_tx.send(LobbyEvent::Refused { request, error });
        }
        // This client never sends the requests these answer; a server that sends them anyway
        // has not read what this client declared.
        ServerMessage::LobbyCreated { lobby_id, .. }
        | ServerMessage::LobbyJoined { lobby_id, .. } => {
            warn!(
                "ignoring an untyped answer about lobby {lobby_id}: this client sent no untyped \
                 request"
            );
        }
        ServerMessage::LobbyError { reason } => {
            warn!("the signalling server refused something: {reason}");
        }
        ServerMessage::LobbyList { lobbies } => {
            // The reply to the `ListLobbies` queued behind the capabilities: a server that has
            // typed requests has said hello by now.
            if !knowledge.hello {
                outdated(knowledge);
            }
            let _ = lobby_event_tx.send(LobbyEvent::LobbyList { lobbies });
        }
        ServerMessage::Disconnected { reason } => {
            let _ = lobby_event_tx.send(LobbyEvent::Disconnected { reason });
        }
        ServerMessage::LobbyMigratable {
            idle_timeout_secs, ..
        } => {
            let _ = lobby_event_tx.send(LobbyEvent::LobbyMigratable { idle_timeout_secs });
        }
        ServerMessage::HostChanged {
            lobby_id,
            previous_host,
            new_host,
            code,
            members,
        } => {
            let _ = lobby_event_tx.send(LobbyEvent::HostChanged {
                lobby_id,
                previous_host,
                new_host,
                code,
                members,
            });
        }
    }
}

#[cfg(test)]
mod retry_tests {
    use tokio::sync::mpsc;

    use super::LobbyConnection;
    use crate::protocol::ClientMessage;

    fn connection() -> (LobbyConnection, mpsc::UnboundedReceiver<ClientMessage>) {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        let (_signal_tx, signal_rx) = mpsc::unbounded_channel();
        let connection = LobbyConnection {
            command_tx,
            event_rx: std::sync::Mutex::new(event_rx),
            signal_rx: std::sync::Mutex::new(signal_rx),
            local_player_uuid: None,
            signalling_lost: false,
            announced_name: String::new(),
            server_outdated: false,
            unanswered: Default::default(),
            next_keep_alive_at: 0.0,
        };
        (connection, command_rx)
    }

    const PEER: u128 = 0xA;
    const OTHER: u128 = 0xB;

    /// New signals for a peer whose earlier batch waits to be sent again go out with it, after
    /// it, rather than overtaking it; another peer's are not held.
    #[test]
    fn signals_behind_a_refused_batch_wait_for_it() {
        let (mut connection, mut sent) = connection();
        connection.send_signals(PEER, vec!["offer".into()]);
        let Ok(ClientMessage::Signals { request, .. }) = sent.try_recv() else {
            panic!("the first batch was not sent");
        };
        assert!(connection.retry_later(request));

        connection.send_signals(PEER, vec!["candidate".into()]);
        assert!(
            sent.try_recv().is_err(),
            "the newer batch overtook the refused one"
        );
        connection.send_signals(OTHER, vec!["offer".into()]);
        assert!(matches!(
            sent.try_recv(),
            Ok(ClientMessage::Signals {
                receiver_uuid: OTHER,
                ..
            })
        ));

        match &connection.unanswered[&request].message {
            ClientMessage::Signals { signals, .. } => assert_eq!(signals, &["offer", "candidate"]),
            other => panic!("{other:?}"),
        }
    }

    /// A cancel is kept under the id it cancels, so a refusal of it for its rate is retried.
    #[test]
    fn a_cancel_refused_for_its_rate_is_sent_again() {
        let (mut connection, mut sent) = connection();
        connection.cancel(7);
        assert!(matches!(
            sent.try_recv(),
            Ok(ClientMessage::CancelRequest { request: 7 })
        ));
        assert!(connection.retry_later(7), "the cancel was not kept");
        assert!(matches!(
            connection.unanswered[&7].message,
            ClientMessage::CancelRequest { request: 7 }
        ));
    }
}
