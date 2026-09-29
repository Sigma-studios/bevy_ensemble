use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::protocol::{
    CAPABILITIES, CAPABILITY_TYPED_REQUESTS, ClientMessage, MAX_GAME_LEN, RequestId, ServerMessage,
    SignallingError, decode, encode,
};

use super::lobby;
use super::state::{ConnectionHandle, Limits, ServerState};

/// How far past the message limit a connection gets before it is closed rather than told.
///
/// Up to this multiple, an over-limit message is refused ([`SignallingError::RateLimited`]) and
/// dropped — a client with a bug gets to notice. Beyond it the sender is not listening to
/// answers, and every message it costs the server is one it should not be paying for.
const CLOSE_AT_MULTIPLE: f64 = 10.0;

/// How long the writer gets to flush and close cleanly before it is abandoned.
const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// A token bucket: `capacity` tokens, refilled at `per_second`, one spent per message.
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    per_second: f64,
    refilled: Instant,
}

impl TokenBucket {
    fn new(capacity: f64, per_second: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            capacity,
            per_second,
            refilled: now,
        }
    }

    /// Spend `cost` tokens if there are that many.
    fn try_take(&mut self, now: Instant, cost: f64) -> bool {
        let elapsed = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.per_second).min(self.capacity);
        self.refilled = now;
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }
}

/// What the rate limiter decided about one frame.
enum Verdict {
    Allow,
    /// Over the limit: answer with an error and drop the frame.
    Refuse,
    /// Far over the limit: stop reading from this connection at all.
    Close,
}

/// Which budget a frame is paid from.
enum Charge {
    /// Everything but signalling: one message.
    General,
    /// WebRTC signals, this many of them. See [`Limits::signals_per_second`].
    Signals(usize),
}

/// The per-connection limiter: one bucket at the limit, one at the multiple past which the
/// connection is closed, a slower one for the operations that are worth guessing at, and one for
/// signalling.
struct RateLimiter {
    soft: TokenBucket,
    hard: TokenBucket,
    lobby_ops: TokenBucket,
    signals: TokenBucket,
}

impl RateLimiter {
    fn new(limits: &Limits, now: Instant) -> Self {
        Self {
            soft: TokenBucket::new(limits.message_burst, limits.messages_per_second, now),
            hard: TokenBucket::new(
                limits.message_burst * CLOSE_AT_MULTIPLE,
                limits.messages_per_second * CLOSE_AT_MULTIPLE,
                now,
            ),
            lobby_ops: TokenBucket::new(limits.lobby_ops_burst, limits.lobby_ops_per_second, now),
            signals: TokenBucket::new(limits.signal_burst, limits.signals_per_second, now),
        }
    }

    /// Charge one frame against the connection.
    ///
    /// Every frame drains the hard bucket, whatever it carries, so a flood of anything is closed
    /// the same way. What it is refused against depends on what it is: signalling against its own
    /// budget, everything else against the general one.
    fn frame(&mut self, now: Instant, charge: Charge) -> Verdict {
        let hard = self.hard.try_take(now, 1.0);
        let soft = match charge {
            Charge::General => self.soft.try_take(now, 1.0),
            Charge::Signals(count) => self.signals.try_take(now, count.max(1) as f64),
        };
        match (hard, soft) {
            (false, _) => Verdict::Close,
            (true, false) => Verdict::Refuse,
            (true, true) => Verdict::Allow,
        }
    }

    /// Charge one `CreateLobby` or `JoinLobbyByCode`, on top of the frame it arrived in.
    fn lobby_op(&mut self, now: Instant) -> bool {
        self.lobby_ops.try_take(now, 1.0)
    }
}

/// The request id a message carries, if it is one of the typed requests.
fn request_of(message: &ClientMessage) -> Option<RequestId> {
    match message {
        ClientMessage::CreateLobbyRequest { request, .. }
        | ClientMessage::JoinLobbyRequest { request, .. }
        | ClientMessage::JoinLobbyByCodeRequest { request, .. }
        | ClientMessage::CancelRequest { request }
        | ClientMessage::Signals { request, .. } => Some(*request),
        _ => None,
    }
}

/// How `error` is said to this connection: typed to a client that declared it can read that, or
/// that asked with a typed request; as the old free text to anybody else.
fn refusal(typed: bool, request: Option<RequestId>, error: SignallingError) -> ServerMessage {
    if typed || request.is_some() {
        ServerMessage::Refused { request, error }
    } else {
        ServerMessage::LobbyError {
            reason: error.legacy_reason().into(),
        }
    }
}

pub async fn handle_socket(socket: WebSocket, state: Arc<ServerState>) {
    let (mut ws_sink, mut ws_stream) = socket.split();

    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

    let write_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let Some(bytes) = encode(&msg) else {
                error!("Failed to serialize server message: {msg:?}");
                continue;
            };
            if ws_sink.send(Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
        // Every sender is gone, which is how the reader says it is done: tell the client so, rather
        // than letting it find out from a reset.
        let _ = ws_sink.close().await;
    });

    let mut player_uuid: Option<u128> = None;
    // Kept here as well as on the handle, so a declaration that arrives before `Authenticate`
    // still applies once there is a handle to put it on.
    let mut capabilities: u64 = 0;
    // Kept here for the same reason, and read directly by `ListLobbies`.
    let mut game = String::new();
    let mut limiter = RateLimiter::new(&state.limits, Instant::now());
    let idle_timeout = state.limits.idle_timeout;

    loop {
        // A connection that says nothing for the idle timeout is gone as far as this server is
        // concerned, whatever TCP thinks. The client sends `KeepAlive` well inside it, so only a
        // dead or hung peer ever hits this -- and without it, one would hold its lobby slot for
        // as long as the kernel took to notice.
        let msg = match tokio::time::timeout(idle_timeout, ws_stream.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(_) => break,
            Err(_elapsed) => {
                info!("Closing idle connection ({player_uuid:?})");
                break;
            }
        };

        let bytes = match msg {
            Message::Binary(b) => Some(b),
            Message::Close(_) => break,
            _ => None,
        };
        let client_msg = bytes
            .as_ref()
            .and_then(|bytes| decode::<ClientMessage>(bytes).ok());
        let typed = capabilities & CAPABILITY_TYPED_REQUESTS != 0;
        let refuse = |request: Option<RequestId>, error: SignallingError| {
            let _ = tx.send(refusal(typed, request, error));
        };

        // Every frame is charged, decodable or not: the cost of reading it has been paid already.
        // Decoded first only so the charge can go to the right budget and a refusal can name the
        // request it refuses.
        let charge = match &client_msg {
            Some(ClientMessage::Signal { .. }) => Charge::Signals(1),
            Some(ClientMessage::Signals { signals, .. }) => Charge::Signals(signals.len()),
            _ => Charge::General,
        };
        match limiter.frame(Instant::now(), charge) {
            Verdict::Allow => {}
            Verdict::Refuse => {
                refuse(
                    client_msg.as_ref().and_then(request_of),
                    SignallingError::RateLimited,
                );
                continue;
            }
            Verdict::Close => {
                warn!("Closing flooding connection ({player_uuid:?})");
                break;
            }
        }

        let Some(client_msg) = client_msg else {
            if bytes.is_some() {
                error!("Failed to decode client message");
            }
            continue;
        };

        match client_msg {
            ClientMessage::Authenticate { display_name } => {
                if player_uuid.is_some() {
                    refuse(None, SignallingError::AlreadyAuthenticated);
                    continue;
                }

                let uuid = Uuid::new_v4().as_u128();
                player_uuid = Some(uuid);

                state.connections.insert(
                    uuid,
                    ConnectionHandle {
                        display_name,
                        lobby_id: None,
                        sender: tx.clone(),
                        capabilities,
                        game: game.clone(),
                        entered_by: None,
                    },
                );

                info!("Player authenticated: {uuid}");
                let _ = tx.send(ServerMessage::Welcome { player_uuid: uuid });
            }

            ClientMessage::SetDisplayName { display_name } => {
                let Some(uuid) = player_uuid else {
                    refuse(None, SignallingError::NotAuthenticated);
                    continue;
                };

                let hosted = match state.connections.get_mut(&uuid) {
                    Some(mut conn) => {
                        conn.display_name = display_name.clone();
                        conn.lobby_id
                    }
                    None => None,
                };

                // The listing keeps its own copy, taken when the lobby was created. Without this
                // the name changes everywhere except the one place it is read from.
                if let Some(lobby_id) = hosted {
                    if let Some(mut lobby) = state.lobbies.get_mut(&lobby_id) {
                        if lobby.host_uuid == uuid {
                            lobby.host_name = display_name;
                        }
                    }
                }
            }

            ClientMessage::CreateLobby { max_players } => {
                if let Err(error) = lobby_op(&mut limiter, player_uuid, |uuid| {
                    lobby::create_lobby(&state, uuid, max_players, None)
                }) {
                    refuse(None, error);
                }
            }

            ClientMessage::CreateLobbyRequest {
                request,
                max_players,
            } => {
                if let Err(error) = lobby_op(&mut limiter, player_uuid, |uuid| {
                    lobby::create_lobby(&state, uuid, max_players, Some(request))
                }) {
                    refuse(Some(request), error);
                }
            }

            // A join by id is not charged as a lobby operation: an id is a random u64, so there
            // is nothing to guess at, and it only ever comes from a listing.
            ClientMessage::JoinLobby { lobby_id } => {
                let Some(uuid) = player_uuid else {
                    refuse(None, SignallingError::NotAuthenticated);
                    continue;
                };
                if let Err(error) = lobby::join_lobby(&state, uuid, lobby_id, None) {
                    refuse(None, error);
                }
            }

            ClientMessage::JoinLobbyRequest { request, lobby_id } => {
                let Some(uuid) = player_uuid else {
                    refuse(Some(request), SignallingError::NotAuthenticated);
                    continue;
                };
                if let Err(error) = lobby::join_lobby(&state, uuid, lobby_id, Some(request)) {
                    refuse(Some(request), error);
                }
            }

            ClientMessage::JoinLobbyByCode { code } => {
                if let Err(error) = lobby_op(&mut limiter, player_uuid, |uuid| {
                    lobby::join_lobby_by_code(&state, uuid, &code, None)
                }) {
                    refuse(None, error);
                }
            }

            ClientMessage::JoinLobbyByCodeRequest { request, code } => {
                if let Err(error) = lobby_op(&mut limiter, player_uuid, |uuid| {
                    lobby::join_lobby_by_code(&state, uuid, &code, Some(request))
                }) {
                    refuse(Some(request), error);
                }
            }

            ClientMessage::CancelRequest { request } => {
                if let Some(uuid) = player_uuid {
                    lobby::cancel_request(&state, uuid, request);
                }
            }

            ClientMessage::LeaveLobby => {
                if let Some(uuid) = player_uuid {
                    lobby::leave_lobby(&state, uuid);
                }
            }

            ClientMessage::CloseLobby => {
                if let Some(uuid) = player_uuid {
                    lobby::close_lobby(&state, uuid);
                }
            }

            ClientMessage::DeclareCapabilities {
                capabilities: declared,
            } => {
                capabilities = declared;
                // Only to a client that can decode it; see `CAPABILITY_TYPED_REQUESTS` for what a
                // client makes of its absence.
                if declared & CAPABILITY_TYPED_REQUESTS != 0 {
                    let _ = tx.send(ServerMessage::ServerHello {
                        capabilities: CAPABILITIES,
                    });
                }
                if let Some(uuid) = player_uuid {
                    if let Some(mut conn) = state.connections.get_mut(&uuid) {
                        conn.capabilities = declared;
                    }
                }
            }

            ClientMessage::DeclareGame { game: declared } => {
                game = truncated(declared, MAX_GAME_LEN);
                if let Some(uuid) = player_uuid {
                    if let Some(mut conn) = state.connections.get_mut(&uuid) {
                        conn.game = game.clone();
                    }
                }
            }

            ClientMessage::ListLobbies => {
                // Authenticated like everything else. The listing is cheap to serve and not a
                // secret, but an unauthenticated socket that can ask for it is an unauthenticated
                // socket that can ask for it in a loop.
                if player_uuid.is_none() {
                    refuse(None, SignallingError::NotAuthenticated);
                    continue;
                }
                let response = lobby::list_lobbies(&state, &game);
                let _ = tx.send(response);
            }

            ClientMessage::Signal {
                receiver_uuid,
                data,
            } => {
                if let Some(from_uuid) = player_uuid {
                    relay(&state, from_uuid, receiver_uuid, vec![data]);
                }
            }

            ClientMessage::Signals {
                receiver_uuid,
                signals,
                ..
            } => {
                if let Some(from_uuid) = player_uuid {
                    relay(&state, from_uuid, receiver_uuid, signals);
                }
            }

            ClientMessage::KeepAlive => {}
        }
    }

    if let Some(uuid) = player_uuid {
        info!("Player disconnected: {uuid}");
        lobby::leave_lobby(&state, uuid);
        state.connections.remove(&uuid);
    }

    // With the connection's handle gone this is the last sender; dropping it lets the writer
    // finish what is queued and send a close frame. A client that has stopped reading could keep
    // that from ever completing, so it gets a deadline.
    drop(tx);
    if tokio::time::timeout(CLOSE_GRACE, write_task).await.is_err() {
        warn!("Writer did not close in time; abandoning it");
    }
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn truncated(mut text: String, max: usize) -> String {
    if text.len() > max {
        let end = (0..=max)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        text.truncate(end);
    }
    text
}

/// A lobby operation: authenticated, within the lobby-operation budget, and refused with the
/// reason `op` gives if it fails.
fn lobby_op(
    limiter: &mut RateLimiter,
    player_uuid: Option<u128>,
    op: impl FnOnce(u128) -> Result<(), SignallingError>,
) -> Result<(), SignallingError> {
    let uuid = player_uuid.ok_or(SignallingError::NotAuthenticated)?;
    if !limiter.lobby_op(Instant::now()) {
        return Err(SignallingError::RateLimited);
    }
    op(uuid)
}

/// Carry `signals` from `from` to `to`, if this server carries signals between them at all: as one
/// frame to a client that reads batches, one frame each to a client that does not.
fn relay(state: &ServerState, from: u128, to: u128, signals: Vec<String>) {
    if !relay_allowed(state, from, to) {
        warn!("Rejecting signal relay from {from} to {to}: not host and member of one lobby");
        return;
    }
    info!("Relaying {} signal(s) from {from} to {to}", signals.len());
    if state.declares(to, CAPABILITY_TYPED_REQUESTS) {
        state.send_to(
            to,
            ServerMessage::Signals {
                sender_uuid: from,
                signals,
            },
        );
    } else {
        for data in signals {
            state.send_to(
                to,
                ServerMessage::Signal {
                    sender_uuid: from,
                    data,
                },
            );
        }
    }
}

/// Whether a signal from `from` to `to` is one this server carries.
///
/// Both must be in the same lobby, and one of them must be its host. Sessions are a star around
/// the host — every member's one connection is to it — so a member has no reason to signal
/// another member, and a relay that would do it anyway is a way for anyone who joined a lobby to
/// push arbitrary data at everyone else in it.
fn relay_allowed(state: &ServerState, from: u128, to: u128) -> bool {
    if from == to {
        return false;
    }
    let lobby_of = |uuid: u128| state.connections.get(&uuid).and_then(|c| c.lobby_id);
    let (Some(from_lobby), Some(to_lobby)) = (lobby_of(from), lobby_of(to)) else {
        return false;
    };
    if from_lobby != to_lobby {
        return false;
    }
    // The connection says which lobby; the lobby says who hosts it. Read from the lobby rather
    // than trusting either end's claim.
    let Some(lobby) = state.lobbies.get(&from_lobby) else {
        return false;
    };
    lobby.host_uuid == from || lobby.host_uuid == to
}
