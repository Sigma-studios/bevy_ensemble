//! An in-process signalling server, for tests that want a real one on a port nobody else has.
//!
//! Not test-only in the `cfg(test)` sense: integration tests in `tests/` build the library
//! without `cfg(test)`, so this is simply part of the `server` feature. Nothing in a deployment
//! calls it.

use std::sync::Arc;

use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use super::{Limits, ServerState, serve};

/// A signalling server on `127.0.0.1`, on its own runtime, stopped when dropped.
pub struct SignallingServer {
    url: String,
    /// `Option` only so `Drop` can take it: a runtime is shut down by value.
    runtime: Option<Runtime>,
    handle: JoinHandle<std::io::Result<()>>,
    state: Arc<ServerState>,
}

impl SignallingServer {
    /// Start one with the deployment defaults.
    pub fn start() -> Self {
        Self::start_with(Limits::default())
    }

    /// Start one with these limits — a one-second idle timeout for a test of the idle timeout,
    /// say, rather than the sixty a deployment gets.
    pub fn start_with(limits: Limits) -> Self {
        // Bound synchronously so the port is known before anything is spawned; the runtime
        // adopts the socket once it exists.
        let std_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        std_listener
            .set_nonblocking(true)
            .expect("set the listener non-blocking");
        let port = std_listener
            .local_addr()
            .expect("read the bound port")
            .port();

        // Its own runtime rather than the caller's: a test that runs on `#[tokio::test]` may drop
        // this from inside its own runtime, and a server task on that runtime would outlive the
        // test that spawned it. This one goes away with the value.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build the server runtime");

        let state = Arc::new(ServerState::with_limits(limits));
        let handle = {
            let state = Arc::clone(&state);
            runtime.spawn(async move {
                let listener = tokio::net::TcpListener::from_std(std_listener)?;
                serve(listener, state).await
            })
        };

        Self {
            url: format!("ws://127.0.0.1:{port}"),
            runtime: Some(runtime),
            handle,
            state,
        }
    }

    /// `ws://127.0.0.1:<port>` — the origin. The signalling endpoint is at `/ws` under it, which
    /// is what [`ws_url`](Self::ws_url) gives.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The URL a client connects to.
    pub fn ws_url(&self) -> String {
        format!("{}/ws", self.url)
    }

    /// The server's state, for a test that wants to look rather than ask.
    pub fn state(&self) -> &Arc<ServerState> {
        &self.state
    }
}

impl Drop for SignallingServer {
    fn drop(&mut self) {
        self.handle.abort();
        if let Some(runtime) = self.runtime.take() {
            // `shutdown_background` rather than dropping: dropping a runtime from inside another
            // runtime's async context panics, and that is exactly where a test drops this.
            runtime.shutdown_background();
        }
    }
}
