//! What to do when ICE loses the path to a peer, decided the same way on every platform.
//!
//! The backends differ in how they *ask* for a restart (`RTCOfferOptions` on native,
//! `RtcOfferOptions` in a browser) and in nothing else: who restarts, how many times, and when a
//! lost path is a lost session are policy, and policy that lives in two files drifts. So it lives
//! here, as a small state machine with no I/O, and each backend asks it what to do.

/// Which side of the offer/answer exchange a peer connection is on.
///
/// Recorded when the connection is made — [`connect_peer`] makes an offerer, an offer received
/// makes an answerer — because it decides who restarts ICE. Only the offerer can: an ICE restart
/// is a new offer, and in this protocol the answerer's offers are refused (a client that offered
/// its host would be trying to become the host). The answerer notices the loss the same way and
/// reports it the same way; it just waits for the offer instead of making one.
///
/// [`connect_peer`]: crate::EnsembleSocket::connect_peer
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Offerer,
    Answerer,
}

/// What a backend should do about ICE having lost its path.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Recover {
    /// Report `Reconnecting`, then send an offer with `ice_restart`. `attempt` counts from one.
    Restart { attempt: u32 },
    /// Report `Reconnecting` and wait: the offerer will notice the same loss and send a restart
    /// offer, which is answered like any other.
    Wait,
    /// Report `Failed`: there is no restart left to try, or nothing to restore.
    GiveUp,
}

/// One peer connection's restart budget and where it stands.
pub(crate) struct Recovery {
    role: Role,
    /// Restart offers made so far, manual ones included.
    restarts: u32,
    /// Between a loss (or a request) and ICE reporting a path again.
    in_progress: bool,
    /// Whether ICE ever had a path at all.
    ///
    /// A restart restores a path; it cannot invent one. ICE failing on a connection that never
    /// connected means no candidate pair works, and gathering the same candidates again will
    /// find the same thing three more times — slowly, on a screen that is waiting for an
    /// answer. That is `Failed`, as it was before restarts existed.
    connected_once: bool,
}

impl Recovery {
    pub(crate) fn new(role: Role) -> Self {
        Self {
            role,
            restarts: 0,
            in_progress: false,
            connected_once: false,
        }
    }

    pub(crate) fn role(&self) -> Role {
        self.role
    }

    /// Whether a recovery is under way, i.e. `Reconnecting` has been reported and neither
    /// `Connected` nor `Failed` has followed it yet.
    pub(crate) fn in_progress(&self) -> bool {
        self.in_progress
    }

    /// ICE reported `Disconnected` (`failed == false`) or `Failed` (`failed == true`).
    pub(crate) fn ice_lost(&mut self, failed: bool) -> Recover {
        if !self.connected_once {
            return Recover::GiveUp;
        }
        match self.role {
            // `Failed` on the answerer is final. Either the offerer's restart already ran and
            // found nothing, or ICE gave up before a restart offer arrived — and the answerer
            // cannot make one itself.
            Role::Answerer if failed => Recover::GiveUp,
            Role::Answerer => {
                self.in_progress = true;
                Recover::Wait
            }
            Role::Offerer => self.request_restart(),
        }
    }

    /// A restart asked for explicitly, or by a lost path on the offerer.
    ///
    /// Counted against [`MAX_ICE_RESTARTS`](crate::MAX_ICE_RESTARTS) either way: a "reconnect"
    /// button that can be pressed for ever on a path that is gone is a button that lies.
    pub(crate) fn request_restart(&mut self) -> Recover {
        debug_assert_eq!(self.role, Role::Offerer, "only the offerer restarts ICE");
        if self.restarts >= crate::MAX_ICE_RESTARTS {
            return Recover::GiveUp;
        }
        self.restarts += 1;
        self.in_progress = true;
        Recover::Restart {
            attempt: self.restarts,
        }
    }

    /// ICE reported `Connected` or `Completed`. `true` when that ends a recovery, in which case
    /// the backend reports `Connected` — the data channel never closed, so its `on_open` will
    /// not say so.
    pub(crate) fn ice_connected(&mut self) -> bool {
        self.connected_once = true;
        std::mem::take(&mut self.in_progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_that_never_existed_is_not_restored() {
        let mut offerer = Recovery::new(Role::Offerer);
        assert_eq!(offerer.ice_lost(true), Recover::GiveUp);
        assert_eq!(offerer.ice_lost(false), Recover::GiveUp);
        let mut answerer = Recovery::new(Role::Answerer);
        assert_eq!(answerer.ice_lost(false), Recover::GiveUp);
    }

    #[test]
    fn the_offerer_restarts_a_bounded_number_of_times() {
        let mut offerer = Recovery::new(Role::Offerer);
        assert!(!offerer.ice_connected());
        for attempt in 1..=crate::MAX_ICE_RESTARTS {
            assert_eq!(offerer.ice_lost(false), Recover::Restart { attempt });
            assert!(offerer.in_progress());
            assert!(
                offerer.ice_connected(),
                "a restart that worked ends the recovery"
            );
            assert!(!offerer.in_progress());
        }
        assert_eq!(offerer.ice_lost(true), Recover::GiveUp);
        assert_eq!(offerer.request_restart(), Recover::GiveUp);
    }

    #[test]
    fn a_failed_restart_is_followed_by_another_while_the_budget_lasts() {
        let mut offerer = Recovery::new(Role::Offerer);
        offerer.ice_connected();
        assert_eq!(offerer.ice_lost(false), Recover::Restart { attempt: 1 });
        // The restart's own checks found nothing: ICE reports `Failed` without a `Connected`
        // in between. Try again rather than reporting a loss the budget could still cover.
        assert_eq!(offerer.ice_lost(true), Recover::Restart { attempt: 2 });
        assert!(offerer.in_progress());
    }

    #[test]
    fn the_answerer_waits_on_disconnected_and_gives_up_on_failed() {
        let mut answerer = Recovery::new(Role::Answerer);
        answerer.ice_connected();
        assert_eq!(answerer.ice_lost(false), Recover::Wait);
        assert!(answerer.in_progress());
        assert!(answerer.ice_connected());
        assert_eq!(answerer.ice_lost(true), Recover::GiveUp);
    }
}
