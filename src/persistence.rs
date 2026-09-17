//! Keeping a resource across sessions: who you are, when you come back.
//!
//! A player picks a name and a look before they ever reach a lobby, and on the web "coming back"
//! means a page reload — a new process, a new world, every resource at its default. Without
//! somewhere to put it, every visit starts by typing the same name again, which is the sort of
//! friction nobody reports as a bug and everybody feels.
//!
//! # Why a resource and not [`PlayerData`](crate::PlayerData)
//!
//! [`PlayerData<T>`] is the synchronised half: a component on a participant entity, broadcast by
//! the host, and it exists only *inside* a lobby. What has to survive a reload is the local value
//! the menu edits — the one a game reads to fill its fields in before there is any lobby to join,
//! and publishes with [`SetPlayerData`](crate::SetPlayerData) once there is. That is a resource,
//! so this persists resources.
//!
//! ```rust,ignore
//! app.add_plugins(PersistedResourcePlugin::<LocalProfile>::new("mygame.profile"));
//! ```
//!
//! The resource is read back in [`PreStartup`], so anything a game does in `Startup` — reading an
//! environment variable, following a link — still wins over what was stored.
//!
//! # Where it goes
//!
//! On the web, the page's local storage: a string per key, kept by the browser for this origin
//! until the player clears site data. Everywhere else there is no page, and both halves do
//! nothing — a native build keeps its value for the run and forgets it, which is the honest
//! behaviour until somebody needs a config file badly enough to ask for one.
//!
//! JSON, not postcard, and deliberately: the value is then readable in the browser's storage
//! inspector, which is where a bug report about a wrong name actually gets answered.

use std::marker::PhantomData;

use bevy::prelude::*;
use serde::{Serialize, de::DeserializeOwned};

/// A resource that is written to the page's storage whenever it changes, and read back when the
/// page loads.
///
/// `key` is the storage key. Storage is shared with everything else on the origin, so prefix it
/// with something that belongs to the game (`"mygame.profile"`, not `"profile"`).
///
/// A stored value that no longer decodes — an older build wrote a different shape — is ignored
/// and the resource keeps its default. That is on purpose: a half-read profile silently carrying
/// one stale field is worse than a fresh one the player sets again.
pub struct PersistedResourcePlugin<T> {
    key: &'static str,
    marker: PhantomData<T>,
}

impl<T> PersistedResourcePlugin<T> {
    pub fn new(key: &'static str) -> Self {
        Self {
            key,
            marker: PhantomData,
        }
    }
}

/// The storage key for `T`, so the systems below can find it without a second generic.
#[derive(Resource)]
struct PersistedUnder<T> {
    key: &'static str,
    marker: PhantomData<T>,
}

impl<T> Plugin for PersistedResourcePlugin<T>
where
    T: Resource + Serialize + DeserializeOwned + Default,
{
    fn build(&self, app: &mut App) {
        app.insert_resource(PersistedUnder::<T> {
            key: self.key,
            marker: PhantomData,
        })
        .init_resource::<T>()
        // Before `Startup`, so a game that overrides the value there — from an environment
        // variable, from the page URL — is overriding the stored one rather than racing it.
        .add_systems(PreStartup, load::<T>)
        // `resource_changed` is true on the frame it is inserted, so the load below would write
        // straight back what it just read. Harmless, and one storage write.
        .add_systems(Update, store::<T>.run_if(resource_changed::<T>));
    }
}

fn load<T: Resource + DeserializeOwned>(mut commands: Commands, under: Res<PersistedUnder<T>>) {
    let Some(text) = storage::read(under.key) else {
        return;
    };
    match serde_json::from_str::<T>(&text) {
        Ok(value) => commands.insert_resource(value),
        Err(error) => {
            // Loud, because the alternative is a player whose name quietly stops sticking.
            warn!(
                "stored `{}` could not be read back and was ignored: {error}",
                under.key
            );
        }
    }
}

fn store<T: Resource + Serialize>(value: Res<T>, under: Res<PersistedUnder<T>>) {
    match serde_json::to_string(value.as_ref()) {
        Ok(text) => storage::write(under.key, &text),
        Err(error) => warn!("`{}` could not be written out: {error}", under.key),
    }
}

/// The page's local storage on the web, and nothing at all anywhere else.
#[cfg(target_arch = "wasm32")]
mod storage {
    fn local_storage() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }

    pub fn read(key: &str) -> Option<String> {
        local_storage()?.get_item(key).ok().flatten()
    }

    pub fn write(key: &str, value: &str) {
        if let Some(storage) = local_storage()
            && storage.set_item(key, value).is_err()
        {
            // Full, or storage disabled for the page. The value still stands for this run.
            bevy::log::warn!("could not store `{key}`");
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod storage {
    pub fn read(_key: &str) -> Option<String> {
        None
    }

    pub fn write(_key: &str, _value: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Resource, Serialize, Deserialize, Default, PartialEq, Debug)]
    struct Profile {
        name: String,
        hair: u8,
    }

    /// Off the web there is nowhere to write, and the plugin must still leave a usable default
    /// rather than refusing to start or clearing the resource.
    #[test]
    fn a_peer_with_no_page_keeps_its_default() {
        let mut app = App::new();
        app.add_plugins(PersistedResourcePlugin::<Profile>::new("test.profile"));
        app.update();
        assert_eq!(*app.world().resource::<Profile>(), Profile::default());
    }

    /// The shape that actually travels. A value that round-trips through the same encoding the
    /// storage uses is the whole promise, and it is testable without a browser.
    #[test]
    fn a_value_survives_the_encoding_the_storage_uses() {
        let profile = Profile {
            name: "SIGMA".into(),
            hair: 7,
        };
        let text = serde_json::to_string(&profile).unwrap();
        assert!(
            text.contains("SIGMA"),
            "readable in a storage inspector: {text}"
        );
        assert_eq!(serde_json::from_str::<Profile>(&text).unwrap(), profile);
    }

    /// What an older build left behind. Ignored, not half-applied.
    #[test]
    fn a_stored_value_of_the_wrong_shape_is_ignored() {
        assert!(serde_json::from_str::<Profile>(r#"{"name":"SIGMA","hue":3}"#).is_err());
    }
}
