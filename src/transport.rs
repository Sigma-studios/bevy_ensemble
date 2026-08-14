//! Which transport a session sends over, and the guarantee that there is only one.
//!
//! Every backend observes the same outbound event, [`SerializedLobbyPacket`]: the core crate
//! encodes a message once and triggers it, and whichever backend is installed picks it up and
//! puts the bytes on its wire. That is what makes a backend a drop-in — but it also means an app
//! holding two backends sends every packet twice, once down each. Nothing about that fails to
//! compile, and nothing about it fails loudly at runtime either; the session merely behaves
//! strangely, in a way that looks like a bug in whatever is on top of it.
//!
//! So a backend claims the transport as it is built, and a second claim panics naming both. The
//! cost of being wrong is a session that misbehaves in the field; the cost of the check is a
//! startup panic on a machine where somebody just wrote the mistake.
//!
//! [`SerializedLobbyPacket`]: crate::SerializedLobbyPacket

use bevy::prelude::*;

/// The transport backend this app sends over — the name of the crate that claimed it.
///
/// Inserted by [`EnsembleTransportAppExt::claim_transport`]. Present in any app with a backend
/// installed, so game code that wants to know what it is running over can read it rather than
/// re-deriving it from its own cargo features.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportBackend(pub &'static str);

/// Installs the one transport an app is allowed to have.
pub trait EnsembleTransportAppExt {
    /// Claim the outbound-packet path for `name`, panicking if another backend already holds it.
    ///
    /// Call this from a backend plugin's `build`, before it observes
    /// [`SerializedLobbyPacket`](crate::SerializedLobbyPacket). `name` is what the panic message
    /// calls this backend, so it should be the crate name.
    fn claim_transport(&mut self, name: &'static str) -> &mut Self;
}

impl EnsembleTransportAppExt for App {
    fn claim_transport(&mut self, name: &'static str) -> &mut Self {
        if let Some(TransportBackend(held)) = self.world().get_resource::<TransportBackend>() {
            panic!(
                "two transport backends in one app: {held} already claimed it, and {name} wants \
                 it too.\n\nBoth observe `SerializedLobbyPacket`, so every packet this session \
                 sends would go out down both of them — twice to any peer that can hear both, \
                 and the duplicate is not detected anywhere downstream.\n\nInstall one. If both \
                 are reachable from the dependency graph, that is usually a pair of cargo \
                 features that need to be mutually exclusive rather than additive."
            );
        }
        self.insert_resource(TransportBackend(name));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_backend_claims_the_transport() {
        let mut app = App::new();
        app.claim_transport("bevy_ensemble_loopback");

        assert_eq!(
            app.world().get_resource::<TransportBackend>(),
            Some(&TransportBackend("bevy_ensemble_loopback")),
            "the claim names the backend that holds the outbound path"
        );
    }

    #[test]
    #[should_panic(expected = "two transport backends in one app")]
    fn a_second_backend_is_a_startup_panic() {
        let mut app = App::new();
        app.claim_transport("bevy_ensemble_steam");
        app.claim_transport("bevy_ensemble_webrtc");
    }
}
