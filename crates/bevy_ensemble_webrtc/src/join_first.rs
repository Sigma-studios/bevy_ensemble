//! Joining whatever lobby is listed first, for as long as the game asks to.
//!
//! See [`JoinFirstLobby`].

use std::time::Duration;

use bevy::prelude::*;
use bevy_ensemble::{Instant, Lobby, PendingLobby, PublicLobbies};

use crate::{JoinWebrtcLobby, RefreshLobbyList};

/// The shortest gap between two requests for the listing.
///
/// An empty listing is answered within a round trip, which on a signalling server on this machine
/// is well under a millisecond; asking again the moment one arrives would ask hundreds of times a
/// second and be refused for its rate within the first frame or two. Half a second is quick
/// enough that a joiner started alongside its host joins as soon as the host has a lobby to join,
/// and slow enough to be nothing to the server.
const LISTING_INTERVAL: Duration = Duration::from_millis(500);

/// How long a request for the listing is left unanswered before it is sent again anyway.
///
/// A listing is not a typed request, so there is no answer to wait on by id and no refusal that
/// names it: a `ListLobbies` refused for its rate, or sent into a connection that was being
/// rebuilt, is simply never answered. The connection asks for the listing itself whenever it is
/// rebuilt, so this is only the backstop for the cases that do not rebuild it.
const LISTING_UNANSWERED: Duration = Duration::from_secs(3);

/// Join the first lobby the signalling server lists for this game, and keep at it until there is
/// one — the development "start a second window and have it join the first" in one resource.
///
/// Insert it, and while there is no [`Lobby`] and no [`PendingLobby`] on this peer, the crate asks
/// the signalling server for the listing and joins the first lobby in it. That is the whole of
/// the behaviour, and every game that had a hand-written version of it had the same one: a
/// joiner started by the same script as its host has nothing to join for the first second or
/// two, and cannot know when that stops being true, so it has to keep asking.
///
/// ```rust,ignore
/// if std::env::var("MYGAME_AUTOSTART").as_deref() == Ok("join") {
///     app.insert_resource(JoinFirstLobby::default());
/// }
/// ```
///
/// *When* to ask is the game's, and deliberately so: an environment variable, a command-line
/// flag, a page's URL, a debug key — each game already has its own, and nothing here reads any of
/// them.
///
/// # Which lobby is "first"
///
/// The first entry of [`PublicLobbies`], exactly as the server sent it. The listing only ever
/// holds lobbies of the game this client declared with
/// [`BevyEnsembleWebrtcPlugin::game`](crate::BevyEnsembleWebrtcPlugin::game), so on a shared
/// server this does not wander into somebody else's game. There is no choosing beyond that, and
/// that is the point: this exists for a signalling server with one host on it, started moments
/// ago by the same script, where "the first" is unambiguous. A join by code, which picks a
/// particular lobby, is a single [`JoinWebrtcLobbyByCode`](crate::JoinWebrtcLobbyByCode) and needs
/// none of this.
///
/// # One request at a time
///
/// The hand-written versions throttled everything on a one-second timer, because a join sent
/// again before the first one was answered was refused as "already in a lobby". This keeps to
/// one request in flight instead of guessing at how long an answer takes:
///
/// - **A join** is in flight for as long as its [`PendingLobby`] exists, and nothing more is sent
///   while it does. The pending lobby is spawned by the frame the join is sent, so there is no
///   window in which a second one could go out. A join refused for its rate is sent again by the
///   crate itself and never leaves the pending lobby.
/// - **A failed join** — refused, timed out, its connection failed — ends in
///   [`LobbyJoinFailed`](bevy_ensemble::LobbyJoinFailed) with the pending lobby gone, and this
///   starts again. Not by joining the same entry again: the listing it came from is what was
///   wrong (the lobby closed, or filled up), so the next join waits for a listing that arrived
///   after the failure.
/// - **The listing** has no request id to wait on, so it is asked for at most every half a
///   second, and not again while an answer is outstanding unless that answer is several seconds
///   late.
///
/// # How long it lasts
///
/// For as long as the resource is there. A lobby that ends — the host quit, the connection was
/// lost — leaves this peer with no lobby, and it goes looking for the next one; for a script that
/// restarts its host, that is a joiner that follows it. A game that wants one attempt and no
/// more, or that leaves a lobby on purpose to go back to a menu, removes the resource once it has
/// what it wanted.
///
/// A peer that is *hosting* has a [`Lobby`], so this waits quietly behind it too; it never leaves
/// a lobby to join another.
///
/// # What the game does on joining
///
/// Nothing is written for it: the join is the ordinary one, so the ordinary signs of it are what
/// to watch. A [`PendingLobby`] without [`Host`](bevy_ensemble::Host) appears the frame the join
/// is sent — the place to show a "connecting" screen — and the [`Lobby`] that replaces it carries
/// the lobby's code as a [`LobbyWebrtcCode`](crate::LobbyWebrtcCode) from the moment the server
/// confirms it.
#[derive(Resource, Debug, Default)]
pub struct JoinFirstLobby {
    /// When the listing was last asked for, if it has been.
    listing_asked_at: Option<Instant>,
    /// Whether that request is still waiting for its answer.
    listing_outstanding: bool,
    /// Whether the listing held now arrived since the last join this sent — whether its first
    /// entry is worth trying.
    listing_fresh: bool,
    /// Whether it has been said that the server is too old to join through.
    said_outdated: bool,
}

pub(crate) fn join_first_lobby(
    mut request: ResMut<JoinFirstLobby>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
    listing: Option<Res<PublicLobbies>>,
    mut refresh: MessageWriter<RefreshLobbyList>,
    mut join: MessageWriter<JoinWebrtcLobby>,
    lobby_conn: Res<crate::connection::LobbyConnection>,
) {
    // Every join through a server without typed requests fails the frame it is made, so trying
    // again at each listing would only be a failure every half a second. Said once, and then
    // nothing until a connection to a server that can be joined through replaces this one.
    if lobby_conn.server_outdated {
        if !request.said_outdated {
            request.said_outdated = true;
            warn!(
                "not joining the first listed lobby: the signalling server is too old to join through"
            );
        }
        return;
    }
    request.said_outdated = false;

    // Read whether or not anything is done with it this frame: a listing that arrives while this
    // peer is in a lobby is still the latest there is once it is not.
    if listing.as_ref().is_some_and(|listing| listing.is_changed()) {
        request.listing_outstanding = false;
        request.listing_fresh = true;
    }
    if !lobbies.is_empty() {
        return;
    }

    if request.listing_fresh
        && let Some(first) = listing.as_ref().and_then(|listing| listing.0.first())
    {
        info!(
            "joining the first listed lobby, {} ({})",
            first.code, first.host_name
        );
        join.write(JoinWebrtcLobby(first.lobby_id));
        // Should this join fail, the next is from a listing asked for after it.
        request.listing_fresh = false;
        return;
    }

    let now = Instant::now();
    let wait = if request.listing_outstanding {
        LISTING_UNANSWERED
    } else {
        LISTING_INTERVAL
    };
    let due = request
        .listing_asked_at
        .is_none_or(|asked| now.duration_since(asked) >= wait);
    if due {
        refresh.write(RefreshLobbyList);
        request.listing_asked_at = Some(now);
        request.listing_outstanding = true;
    }
}
