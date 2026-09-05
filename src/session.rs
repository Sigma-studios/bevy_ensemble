//! Hosting, joining and leaving, said once, in terms no backend owns.
//!
//! A game's session code — start a lobby, list what is joinable, join one, leave, go back to the
//! menu when the lobby is lost — is the same code whichever wire it runs over. Written against a
//! backend's own types it is not, and a game supporting two transports ends up with two
//! near-identical files that must be edited together and drift when they are not.
//!
//! So the requests live here and the backend that is installed services them:
//!
//! | request | what a backend does with it |
//! | --- | --- |
//! | [`StartHosting`](crate::StartHosting) | open a lobby on the platform |
//! | [`RefreshLobbies`] | rediscover joinable lobbies and publish [`PublicLobbies`] |
//! | [`JoinLobby`] | join the lobby with that id |
//! | [`LeaveLobby`] | close platform connections and despawn the lobby entities |
//!
//! and [`PublicLobbies`] is the answer to "what can I join", in the same shape from every
//! backend. Hosting already had a neutral request; these are the other three.
//!
//! Exactly one transport is compiled into an app (see [`claim_transport`]), so a game writes its
//! session once against these and picks the backend with a cargo feature rather than with a `cfg`
//! through the middle of its own logic.
//!
//! # Lobby ids are u64 and mean nothing
//!
//! [`JoinLobby`] carries the same `u64` a backend put in [`PublicLobbyInfo::lobby_id`]. It is
//! opaque: a server-assigned id on WebRTC, a `LobbyId`'s raw bits on Steam. Menus should pass
//! back what discovery handed them rather than construct one.
//!
//! [`claim_transport`]: crate::EnsembleTransportAppExt::claim_transport
//! [`PublicLobbies`]: crate::PublicLobbies
//! [`PublicLobbyInfo::lobby_id`]: crate::PublicLobbyInfo::lobby_id

use bevy::prelude::*;

/// Ask the installed backend to rediscover joinable lobbies.
///
/// The answer arrives as a change to [`PublicLobbies`](crate::PublicLobbies) — asynchronously,
/// because discovery is a network round trip on every backend that has one at all. A backend with
/// no concept of lobby discovery may ignore this.
#[derive(Message, Debug, Clone, Copy)]
pub struct RefreshLobbies;

/// Join the lobby with this id, as discovered in [`PublicLobbies`](crate::PublicLobbies).
#[derive(Message, Debug, Clone, Copy)]
pub struct JoinLobby(pub u64);

/// A session that was being opened will not be opening.
///
/// Written by the installed backend when a host or a join is given up on: the peer connection
/// failed, or the attempt ran past its deadline with nothing to show for it. The backend has
/// already despawned the lobby entity by the time this arrives -- see below -- so a game reading
/// this is being told *why*, not being asked what to do.
///
/// # The backend acts, and this says why
///
/// Unlike [`RegistryMismatch`], which leaves a promoted lobby standing because a game might
/// reasonably choose what to do with one, a lobby that never opened has nothing worth keeping:
/// no roster, no code, no data channel, nothing a screen could show. Worse, leaving it standing
/// soft-locks the peer -- the backends refuse a fresh join while one is pending, so a consumer
/// that did not know to clean up could never retry.
///
/// The correct action is also not uniform, which is the other reason it is not the game's. A
/// client whose connection fails has lost its whole session; a host whose incoming client fails
/// has lost one player and must keep hosting for everybody else. The backend knows which role it
/// holds, and every consumer working that out again is how the difference gets got wrong.
///
/// [`RegistryMismatch`]: https://docs.rs/bevy_ticked_networking_ensemble
#[derive(Message, Debug, Clone)]
pub struct LobbyJoinFailed {
    /// Why, in words a game can put in front of a player.
    pub reason: String,
}

/// Leave the current session, hosted or joined.
///
/// A backend handling this closes its platform-level connections and despawns the lobby and
/// `LobbyClient` entities. Game-side teardown — state, local identity — belongs to the game and
/// is not this message's business.
#[derive(Message, Debug, Clone, Copy)]
pub struct LeaveLobby;

/// Registers the backend-neutral session requests.
///
/// Added by [`EnsemblePlugin`](crate::EnsemblePlugin), so a game and a backend can both assume
/// the messages exist.
pub(crate) fn register_session_messages(app: &mut App) {
    app.add_message::<RefreshLobbies>()
        .add_message::<JoinLobby>()
        .add_message::<LeaveLobby>()
        .add_message::<LobbyJoinFailed>();
}
