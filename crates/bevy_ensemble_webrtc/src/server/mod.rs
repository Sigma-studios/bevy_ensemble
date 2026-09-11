mod lobby;
mod relay;
mod state;
pub mod test_support;
mod ws_handler;

use std::sync::Arc;

use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::net::TcpListener;

pub use lobby::{MAX_PLAYERS, clamp_max_players};
pub use relay::{
    DEFAULT_LISTEN_PORT, DEFAULT_RELAY_PORTS, Relay, RelayConfig, RelayCredentials, RelayError,
    start_relay,
};
pub use state::{Limits, ServerState};
pub use ws_handler::handle_socket;

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServerState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// The signalling service: `/ws` on `listener`, every socket handled by [`handle_socket`] against
/// `state`. Runs until the listener fails.
///
/// This is all the binary does with the listener, factored out so a test can run the same server
/// on a port of its choosing. The limits — idle timeout, rates — come with `state`, from
/// [`ServerState::with_limits`].
pub async fn serve(listener: TcpListener, state: Arc<ServerState>) -> std::io::Result<()> {
    let app = Router::new()
        .route("/ws", get(ws_upgrade))
        .with_state(state);
    axum::serve(listener, app).await
}
