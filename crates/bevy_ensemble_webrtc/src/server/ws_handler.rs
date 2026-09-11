use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::protocol::{ClientMessage, ServerMessage, decode, encode};

use super::lobby;
use super::state::{ConnectionHandle, Limits, ServerState};

/// How far past the message limit a connection gets before it is closed rather than told.
///
/// Up to this multiple, an over-limit message is answered with `LobbyError { "rate limited" }`
/// and dropped — a client with a bug gets to notice. Beyond it the sender is not listening to
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

    /// Spend one token if there is one.
    fn try_take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.per_second).min(self.capacity);
        self.refilled = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
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

/// The per-connection limiter: one bucket at the limit, one at the multiple past which the
/// connection is closed, and a slower one for the operations that are worth guessing at.
struct RateLimiter {
    soft: TokenBucket,
    hard: TokenBucket,
    lobby_ops: TokenBucket,
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
        }
    }

    /// Charge one frame, of any kind, against the connection.
    fn frame(&mut self, now: Instant) -> Verdict {
        // Both buckets drain on every frame, so the hard one measures the same flood the soft one
        // does — just with ten times the room before it acts.
        let hard = self.hard.try_take(now);
        let soft = self.soft.try_take(now);
        match (hard, soft) {
            (false, _) => Verdict::Close,
            (true, false) => Verdict::Refuse,
            (true, true) => Verdict::Allow,
        }
    }

    /// Charge one `CreateLobby` or `JoinLobbyByCode`, on top of the frame it arrived in.
    fn lobby_op(&mut self, now: Instant) -> bool {
        self.lobby_ops.try_take(now)
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

    let rate_limited = || ServerMessage::LobbyError {
        reason: "rate limited".into(),
    };
    let not_authenticated = || ServerMessage::LobbyError {
        reason: "Not authenticated".into(),
    };

    let mut player_uuid: Option<u128> = None;
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

        // Every frame is charged, decodable or not: the cost of reading it has been paid already.
        match limiter.frame(Instant::now()) {
            Verdict::Allow => {}
            Verdict::Refuse => {
                let _ = tx.send(rate_limited());
                continue;
            }
            Verdict::Close => {
                warn!("Closing flooding connection ({player_uuid:?})");
                break;
            }
        }

        let bytes = match msg {
            Message::Binary(b) => b,
            Message::Close(_) => break,
            _ => continue,
        };

        let Ok(client_msg) = decode::<ClientMessage>(&bytes) else {
            error!("Failed to decode client message");
            continue;
        };

        match client_msg {
            ClientMessage::Authenticate { display_name } => {
                if player_uuid.is_some() {
                    let _ = tx.send(ServerMessage::LobbyError {
                        reason: "Already authenticated".into(),
                    });
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
                    },
                );

                info!("Player authenticated: {uuid}");
                let _ = tx.send(ServerMessage::Welcome { player_uuid: uuid });
            }

            ClientMessage::SetDisplayName { display_name } => {
                let Some(uuid) = player_uuid else {
                    let _ = tx.send(not_authenticated());
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
                let Some(uuid) = player_uuid else {
                    let _ = tx.send(not_authenticated());
                    continue;
                };
                if !limiter.lobby_op(Instant::now()) {
                    let _ = tx.send(rate_limited());
                    continue;
                }

                let response = lobby::create_lobby(&state, uuid, max_players);
                let _ = tx.send(response);
            }

            ClientMessage::JoinLobby { lobby_id } => {
                let Some(uuid) = player_uuid else {
                    let _ = tx.send(not_authenticated());
                    continue;
                };

                let response = lobby::join_lobby(&state, uuid, lobby_id);
                let _ = tx.send(response);
            }

            ClientMessage::JoinLobbyByCode { code } => {
                let Some(uuid) = player_uuid else {
                    let _ = tx.send(not_authenticated());
                    continue;
                };
                if !limiter.lobby_op(Instant::now()) {
                    let _ = tx.send(rate_limited());
                    continue;
                }

                let response = lobby::join_lobby_by_code(&state, uuid, &code);
                let _ = tx.send(response);
            }

            ClientMessage::LeaveLobby => {
                if let Some(uuid) = player_uuid {
                    lobby::leave_lobby(&state, uuid);
                }
            }

            ClientMessage::ListLobbies => {
                // Authenticated like everything else. The listing is cheap to serve and not a
                // secret, but an unauthenticated socket that can ask for it is an unauthenticated
                // socket that can ask for it in a loop.
                if player_uuid.is_none() {
                    let _ = tx.send(not_authenticated());
                    continue;
                }
                let response = lobby::list_lobbies(&state);
                let _ = tx.send(response);
            }

            ClientMessage::Signal {
                receiver_uuid,
                data,
            } => {
                let Some(from_uuid) = player_uuid else {
                    continue;
                };

                if relay_allowed(&state, from_uuid, receiver_uuid) {
                    info!("Relaying signal from {from_uuid} to {receiver_uuid}");
                    state.send_to(
                        receiver_uuid,
                        ServerMessage::Signal {
                            sender_uuid: from_uuid,
                            data,
                        },
                    );
                } else {
                    warn!(
                        "Rejecting signal relay from {from_uuid} to {receiver_uuid}: not host and member of one lobby"
                    );
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
