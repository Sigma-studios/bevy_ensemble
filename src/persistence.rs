//! Keeping things across sessions: who you are, when you come back, and how you like it set up.
//!
//! A player picks a name and a look before they ever reach a lobby, and on the web "coming back"
//! means a page reload — a new process, a new world, every resource at its default. Without
//! somewhere to put it, every visit starts by typing the same name again, which is the sort of
//! friction nobody reports as a bug and everybody feels.
//!
//! # Where it goes
//!
//! [`Storage`] is the place, and a game says where that is by inserting one before its plugins
//! read from it — normally [`Storage::platform`]:
//!
//! - **On the web**, the page's local storage: a string per key, kept by the browser for this
//!   origin until the player clears site data.
//! - **Natively**, a file per key under the platform's config directory. A key is
//!   `<game>.<name>`, and it is kept at `<config>/<game>/<name>`, where `<config>` is
//!   `$XDG_CONFIG_HOME`, else `%APPDATA%`, else `~/.config`. A file each means a value this build
//!   cannot read is that one value lost rather than all of them, and every one of them is
//!   readable and editable by hand.
//!
//! With no [`Storage`] inserted nothing is kept, and that is the default on purpose: a test that
//! stands a game's plugins up must not read the developer's own settings, still less write over
//! them, and the safe way to make sure of that is for keeping things to be something only `main`
//! turns on. [`Storage::in_memory`] is the same thing with a memory, for a test about keeping.
//!
//! # Why a resource and not [`PlayerData`](crate::PlayerData)
//!
//! [`PlayerData<T>`](crate::PlayerData) is the synchronised half: a component on a participant entity, broadcast by
//! the host, and it exists only *inside* a lobby. What has to survive a reload is the local value
//! the menu edits — the one a game reads to fill its fields in before there is any lobby to join,
//! and publishes with [`SetPlayerData`](crate::SetPlayerData) once there is. That is a resource,
//! so [`PersistedResourcePlugin`] persists resources.
//!
//! ```rust,ignore
//! app.insert_resource(Storage::platform())
//!     .add_plugins(PersistedResourcePlugin::<LocalProfile>::new("mygame.profile"));
//! ```
//!
//! The resource is read back in [`PreStartup`], so anything a game does in `Startup` — reading an
//! environment variable, following a link — still wins over what was stored. JSON, not postcard,
//! and deliberately: the value is then readable in the browser's storage inspector and in the
//! file, which is where a bug report about a wrong name actually gets answered.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use serde::{Serialize, de::DeserializeOwned};

/// Where things are kept between runs: a short string per key.
///
/// Keys are `<game>.<name>`, prefixed because the web's storage is shared with everything else on
/// the origin, and natively because the prefix is the folder. Cheap to clone; clones share what
/// they keep, so a game can take one at build time and read the same place later.
#[derive(Resource, Clone, Default)]
pub struct Storage(Backend);

#[derive(Clone, Default)]
enum Backend {
    /// Nothing kept: every read is empty and every write goes nowhere.
    #[default]
    Nowhere,
    /// Kept for as long as the storage is, and no longer.
    Memory(Arc<Mutex<HashMap<String, String>>>),
    /// The page's local storage.
    #[cfg(target_arch = "wasm32")]
    Page,
    /// A file per key, under a folder per game, under this directory.
    #[cfg(not(target_arch = "wasm32"))]
    Directory(std::path::PathBuf),
}

impl Storage {
    /// The platform's own place for a player's settings: the page's local storage on the web, and
    /// the config directory natively — see the [module notes](self). Nothing at all when there is
    /// no such place, which is not an error: what was set still stands for the run.
    pub fn platform() -> Self {
        #[cfg(target_arch = "wasm32")]
        {
            Self(Backend::Page)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            config_dir().map_or_else(Self::default, Self::in_directory)
        }
    }

    /// Files under `dir` instead of the config directory, laid out the same way.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn in_directory(dir: impl Into<std::path::PathBuf>) -> Self {
        Self(Backend::Directory(dir.into()))
    }

    /// Kept in memory, for as long as this storage or a clone of it lives.
    pub fn in_memory() -> Self {
        Self(Backend::Memory(Arc::default()))
    }

    /// The [`Storage`] in `app`, inserting one that keeps nothing if the game has not chosen.
    ///
    /// For a plugin that reads what was kept while it is being built, so that the value is there
    /// before the first frame rather than one frame into it.
    pub fn of(app: &mut App) -> Self {
        app.init_resource::<Self>();
        app.world().resource::<Self>().clone()
    }

    /// What is kept under `key`, if anything.
    pub fn read(&self, key: &str) -> Option<String> {
        match &self.0 {
            Backend::Nowhere => None,
            Backend::Memory(kept) => kept.lock().ok()?.get(key).cloned(),
            #[cfg(target_arch = "wasm32")]
            Backend::Page => page::read(key),
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Directory(dir) => std::fs::read_to_string(file_for(dir, key)?).ok(),
        }
    }

    /// Keep `value` under `key`, over whatever was there.
    ///
    /// Failing to is a warning and not an error: the value still stands for this run, exactly as
    /// it would with nowhere to keep it.
    pub fn write(&self, key: &str, value: &str) {
        let kept = match &self.0 {
            Backend::Nowhere => true,
            Backend::Memory(kept) => kept
                .lock()
                .map(|mut kept| kept.insert(key.to_owned(), value.to_owned()))
                .is_ok(),
            #[cfg(target_arch = "wasm32")]
            Backend::Page => page::write(key, value),
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Directory(dir) => write_file(dir, key, value),
        };
        if !kept {
            warn!("could not keep `{key}`");
        }
    }
}

/// The platform's config directory, found from the environment rather than from a crate: it is
/// three variables, and none of them needs a dependency.
#[cfg(not(target_arch = "wasm32"))]
fn config_dir() -> Option<std::path::PathBuf> {
    use std::{env::var_os, path::PathBuf};

    var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| var_os("APPDATA").map(PathBuf::from))
        .or_else(|| var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

/// `<dir>/<game>/<name>` for `<game>.<name>`, or `None` for a key that is not one.
///
/// Both halves are held to letters, digits, `-` and `_` — and the name to `.` as well, but not to
/// start with one — so no key is ever a path this never meant to write to.
#[cfg(not(target_arch = "wasm32"))]
fn file_for(dir: &std::path::Path, key: &str) -> Option<std::path::PathBuf> {
    let (game, name) = key.split_once('.')?;
    let plain = |part: &str, dots: bool| {
        !part.is_empty()
            && !part.starts_with('.')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || (dots && c == '.'))
    };
    (plain(game, false) && plain(name, true)).then(|| dir.join(game).join(name))
}

/// Write to a file beside the real one and move it into place, so a run that dies halfway leaves
/// the old value rather than half of the new one.
#[cfg(not(target_arch = "wasm32"))]
fn write_file(dir: &std::path::Path, key: &str, value: &str) -> bool {
    let Some(path) = file_for(dir, key) else {
        return false;
    };
    let mut partial = path.clone().into_os_string();
    partial.push(".partial");
    path.parent()
        .is_some_and(|folder| std::fs::create_dir_all(folder).is_ok())
        && std::fs::write(&partial, value).is_ok()
        && std::fs::rename(&partial, &path).is_ok()
}

#[cfg(target_arch = "wasm32")]
mod page {
    fn local_storage() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }

    pub fn read(key: &str) -> Option<String> {
        local_storage()?.get_item(key).ok().flatten()
    }

    /// Full, or storage disabled for the page, is a value not kept.
    pub fn write(key: &str, value: &str) -> bool {
        local_storage().is_some_and(|storage| storage.set_item(key, value).is_ok())
    }
}

/// A resource that is written to the [`Storage`] whenever it changes, and read back at startup.
///
/// `key` is the storage key, `<game>.<name>` — see [`Storage`].
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
        .init_resource::<Storage>()
        .init_resource::<T>()
        // Before `Startup`, so a game that overrides the value there — from an environment
        // variable, from the page URL — is overriding the stored one rather than racing it.
        .add_systems(PreStartup, load::<T>)
        // `resource_changed` is true on the frame it is inserted, so the load below would write
        // straight back what it just read. Harmless, and one storage write.
        .add_systems(Update, store::<T>.run_if(resource_changed::<T>));
    }
}

fn load<T: Resource + DeserializeOwned>(
    mut commands: Commands,
    storage: Res<Storage>,
    under: Res<PersistedUnder<T>>,
) {
    let Some(text) = storage.read(under.key) else {
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

fn store<T: Resource + Serialize>(
    value: Res<T>,
    storage: Res<Storage>,
    under: Res<PersistedUnder<T>>,
) {
    match serde_json::to_string(value.as_ref()) {
        Ok(text) => storage.write(under.key, &text),
        Err(error) => warn!("`{}` could not be written out: {error}", under.key),
    }
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

    /// With nowhere chosen nothing is kept, and the plugin must still leave a usable default
    /// rather than refusing to start or clearing the resource.
    #[test]
    fn a_game_that_chose_nowhere_keeps_its_default() {
        let mut app = App::new();
        app.add_plugins(PersistedResourcePlugin::<Profile>::new("test.profile"));
        app.update();
        assert_eq!(*app.world().resource::<Profile>(), Profile::default());
        assert_eq!(app.world().resource::<Storage>().read("test.profile"), None);
    }

    /// What one run keeps, the next reads back — the whole promise, through the plugin.
    #[test]
    fn a_value_kept_by_one_run_is_there_in_the_next() {
        let storage = Storage::in_memory();
        let mut first = App::new();
        first
            .insert_resource(storage.clone())
            .add_plugins(PersistedResourcePlugin::<Profile>::new("test.profile"));
        first.update();
        first.world_mut().resource_mut::<Profile>().name = "SIGMA".into();
        first.update();
        let text = storage.read("test.profile").unwrap();
        assert!(
            text.contains("SIGMA"),
            "readable in a storage inspector: {text}"
        );

        let mut second = App::new();
        second
            .insert_resource(storage)
            .add_plugins(PersistedResourcePlugin::<Profile>::new("test.profile"));
        second.update();
        assert_eq!(second.world().resource::<Profile>().name, "SIGMA");
    }

    /// What an older build left behind. Ignored, not half-applied.
    #[test]
    fn a_stored_value_of_the_wrong_shape_is_ignored() {
        let storage = Storage::in_memory();
        storage.write("test.profile", r#"{"name":"SIGMA","hue":3}"#);
        let mut app = App::new();
        app.insert_resource(storage)
            .add_plugins(PersistedResourcePlugin::<Profile>::new("test.profile"));
        app.update();
        assert_eq!(*app.world().resource::<Profile>(), Profile::default());
    }

    /// A file per key, under a folder per game, and nothing written for a key that would be a path.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_key_is_a_file_under_its_game() {
        let dir = std::env::temp_dir().join(format!("ensemble-storage-{}", std::process::id()));
        let storage = Storage::in_directory(&dir);
        storage.write("game.volume.music", "0.5");
        assert_eq!(
            std::fs::read_to_string(dir.join("game").join("volume.music")).unwrap(),
            "0.5"
        );
        assert_eq!(storage.read("game.volume.music").as_deref(), Some("0.5"));

        for key in [
            "game",
            "game.",
            ".name",
            "game..name",
            "game./etc",
            "ga/me.x",
        ] {
            storage.write(key, "no");
            assert_eq!(storage.read(key), None, "{key}");
        }
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "only the game's own folder"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
