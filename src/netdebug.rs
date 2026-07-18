//! Interactive dev overlay: live network metrics + clickable netsim preset toggles.
//!
//! Enabled by the `netdebug` feature only, so it — along with the whole [`crate::netsim`]
//! simulator — is compiled out of release builds entirely. Built on plain Bevy UI so it
//! needs no extra dependency and renders in any game that already has a camera, native or
//! WASM, on either transport backend.
//!
//! Unlike the (cheap, always-on) metrics counters, this overlay is an **opt-in plugin**
//! you add and configure yourself:
//!
//! ```rust,ignore
//! # #[cfg(feature = "netdebug")]
//! app.add_plugins(
//!     NetDebugPlugin::default()
//!         .toggle_key(KeyCode::F3)        // press to show/hide (default F3)
//!         .default_preset(NetPreset::FourG) // start the shim on this preset
//!         .hidden(),                        // start with the panel hidden
//! );
//! ```
//!
//! Because the plugin type only exists under the feature, add it behind the same
//! `#[cfg(feature = "...")]` your game uses to turn the feature on. The optional
//! `ENSEMBLE_NETSIM=4g` environment variable overrides [`default_preset`](NetDebugPlugin::default_preset)
//! at startup (handy for headless / CI runs that never touch the window).

use bevy::prelude::*;

use crate::{
    PeerLastPong, PeerRtt,
    netmetrics::NetMetrics,
    netsim::{NetPreset, NetSim, drain_netsim},
};

/// Opt-in plugin: installs the network simulator and its interactive overlay.
///
/// Construct with [`NetDebugPlugin::default`] and tweak via the builder methods. The
/// metrics counters themselves come from the always-on `netmetrics` layer (which the
/// `netdebug` feature implies), so this plugin only owns the shim and the window.
#[derive(Clone)]
pub struct NetDebugPlugin {
    /// Key that shows/hides the panel. `None` disables the hotkey entirely.
    pub toggle_key: Option<KeyCode>,
    /// Whether the panel is visible when the app starts.
    pub start_visible: bool,
    /// Preset the simulator starts on (overridden by `ENSEMBLE_NETSIM` if `env_override`).
    pub default_preset: NetPreset,
    /// Whether the `ENSEMBLE_NETSIM` environment variable may override `default_preset`.
    pub env_override: bool,
}

impl Default for NetDebugPlugin {
    fn default() -> Self {
        Self {
            toggle_key: Some(KeyCode::F3),
            start_visible: true,
            default_preset: NetPreset::Off,
            env_override: true,
        }
    }
}

impl NetDebugPlugin {
    /// Set the show/hide hotkey.
    pub fn toggle_key(mut self, key: KeyCode) -> Self {
        self.toggle_key = Some(key);
        self
    }

    /// Disable the show/hide hotkey (panel visibility is then fixed).
    pub fn no_toggle_key(mut self) -> Self {
        self.toggle_key = None;
        self
    }

    /// Start with the panel hidden (toggle it on with the hotkey).
    pub fn hidden(mut self) -> Self {
        self.start_visible = false;
        self
    }

    /// Start the simulator on the given preset instead of [`NetPreset::Off`].
    pub fn default_preset(mut self, preset: NetPreset) -> Self {
        self.default_preset = preset;
        self
    }

    /// Ignore the `ENSEMBLE_NETSIM` environment variable.
    pub fn ignore_env(mut self) -> Self {
        self.env_override = false;
        self
    }
}

impl Plugin for NetDebugPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(NetDebugConfig {
            toggle_key: self.toggle_key,
            start_visible: self.start_visible,
            default_preset: self.default_preset,
            env_override: self.env_override,
        })
        .init_resource::<NetSim>()
        .add_systems(Startup, (apply_initial_preset, spawn_overlay))
        .add_systems(
            Update,
            (
                drain_netsim,
                toggle_overlay,
                update_overlay_text,
                handle_preset_buttons,
                tint_preset_buttons,
            ),
        );
    }
}

/// Resolved configuration for the overlay, inserted by [`NetDebugPlugin`].
#[derive(Resource, Clone)]
pub struct NetDebugConfig {
    /// Key that shows/hides the panel, if any.
    pub toggle_key: Option<KeyCode>,
    /// Whether the panel starts visible.
    pub start_visible: bool,
    /// Preset the simulator starts on.
    pub default_preset: NetPreset,
    /// Whether `ENSEMBLE_NETSIM` may override `default_preset`.
    pub env_override: bool,
}

/// Marker for the panel root, so the toggle system can flip its visibility.
#[derive(Component)]
struct NetOverlayRoot;

/// Marker for the text node whose contents we rewrite each frame.
#[derive(Component)]
struct NetOverlayText;

/// Marks a preset button and records which preset it selects.
#[derive(Component, Clone, Copy)]
struct PresetButton(NetPreset);

const PANEL_BG: Color = Color::srgba(0.0, 0.0, 0.0, 0.75);
const BTN_IDLE: Color = Color::srgb(0.18, 0.18, 0.20);
const BTN_ACTIVE: Color = Color::srgb(0.15, 0.55, 0.30);

/// Parse a preset name (case-insensitive) from a string such as an env var.
fn parse_preset(value: &str) -> Option<NetPreset> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Some(NetPreset::Off),
        "cable" => Some(NetPreset::Cable),
        "4g" | "fourg" => Some(NetPreset::FourG),
        "badwifi" | "bad_wifi" | "wifi" => Some(NetPreset::BadWifi),
        "satellite" | "sat" => Some(NetPreset::Satellite),
        _ => None,
    }
}

/// Apply the starting preset from config, letting `ENSEMBLE_NETSIM` override it.
fn apply_initial_preset(config: Res<NetDebugConfig>, mut sim: ResMut<NetSim>) {
    let mut preset = config.default_preset;
    if config.env_override {
        if let Ok(value) = std::env::var("ENSEMBLE_NETSIM") {
            match parse_preset(&value) {
                Some(p) => preset = p,
                None => warn!("ENSEMBLE_NETSIM=`{value}` is not a known preset; ignoring"),
            }
        }
    }
    sim.set_preset(preset);
    if preset != NetPreset::Off {
        info!("netsim: starting with preset `{}`", preset.label());
    }
}

/// Spawn the overlay: a top-left panel with a metrics text block and a preset button row.
fn spawn_overlay(mut commands: Commands, config: Res<NetDebugConfig>) {
    let start_visibility = if config.start_visible {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };

    commands
        .spawn((
            NetOverlayRoot,
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(8.0),
                left: Val::Px(8.0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(8.0)),
                row_gap: Val::Px(6.0),
                ..default()
            },
            BackgroundColor(PANEL_BG),
            GlobalZIndex(i32::MAX),
            start_visibility,
        ))
        .with_children(|panel| {
            panel.spawn((
                NetOverlayText,
                Text::new("net: …"),
                TextFont {
                    font_size: FontSize::Px(13.0),
                    ..default()
                },
                TextColor(Color::srgb(0.85, 0.9, 0.95)),
            ));

            panel
                .spawn(Node {
                    flex_direction: FlexDirection::Row,
                    column_gap: Val::Px(4.0),
                    ..default()
                })
                .with_children(|row| {
                    for preset in NetPreset::ALL {
                        row.spawn((
                            Button,
                            PresetButton(preset),
                            Node {
                                padding: UiRect::axes(Val::Px(6.0), Val::Px(3.0)),
                                ..default()
                            },
                            BackgroundColor(BTN_IDLE),
                        ))
                        .with_children(|btn| {
                            btn.spawn((
                                Text::new(preset.label()),
                                TextFont {
                                    font_size: FontSize::Px(12.0),
                                    ..default()
                                },
                                TextColor(Color::WHITE),
                            ));
                        });
                    }
                });
        });
}

/// Show/hide the panel when the configured toggle key is pressed.
fn toggle_overlay(
    keys: Res<ButtonInput<KeyCode>>,
    config: Res<NetDebugConfig>,
    mut root: Query<&mut Visibility, With<NetOverlayRoot>>,
) {
    let Some(key) = config.toggle_key else {
        return;
    };
    if !keys.just_pressed(key) {
        return;
    }
    for mut visibility in root.iter_mut() {
        *visibility = match *visibility {
            Visibility::Hidden => Visibility::Inherited,
            _ => Visibility::Hidden,
        };
    }
}

/// Format a bytes/sec value compactly.
fn fmt_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1_000_000.0 {
        format!("{:.1} MB/s", bytes_per_sec / 1_000_000.0)
    } else if bytes_per_sec >= 1_000.0 {
        format!("{:.1} kB/s", bytes_per_sec / 1_000.0)
    } else {
        format!("{bytes_per_sec:.0} B/s")
    }
}

/// Rewrite the metrics text each frame from the current counters, RTT and netsim state.
fn update_overlay_text(
    metrics: Res<NetMetrics>,
    sim: Res<NetSim>,
    peers: Query<(&PeerRtt, &PeerLastPong)>,
    mut text: Single<&mut Text, With<NetOverlayText>>,
) {
    let peer_count = peers.iter().count();
    let (avg_rtt_ms, worst_silence) = if peer_count > 0 {
        let mut rtt_sum = 0.0;
        let mut worst = 0.0_f64;
        for (rtt, last_pong) in peers.iter() {
            rtt_sum += rtt.0;
            worst = worst.max(last_pong.0);
        }
        (rtt_sum / peer_count as f64 * 1000.0, worst)
    } else {
        (0.0, 0.0)
    };

    let cfg = sim.config();
    let mut out = String::new();
    out.push_str(&format!("netsim: {}", sim.preset.label()));
    if sim.is_active() {
        out.push_str(&format!(
            "  (+{:.0}ms ±{:.0}ms, loss {:.1}%, dup {:.1}%)",
            cfg.delay_ms,
            cfg.jitter_ms,
            cfg.loss * 100.0,
            cfg.duplicate * 100.0,
        ));
    }
    out.push('\n');
    out.push_str(&format!(
        "peers: {peer_count}   rtt: {avg_rtt_ms:.0}ms   silence: {worst_silence:.1}s\n"
    ));
    out.push_str(&format!(
        "rx: {}  {:.0} pkt/s\n",
        fmt_rate(metrics.rx_bytes_per_sec),
        metrics.rx_packets_per_sec,
    ));
    out.push_str(&format!(
        "tx: {}  {:.0} pkt/s\n",
        fmt_rate(metrics.tx_bytes_per_sec),
        metrics.tx_packets_per_sec,
    ));
    out.push_str(&format!(
        "sim dropped: {}   duplicated: {}",
        metrics.sim_dropped, metrics.sim_duplicated,
    ));

    ***text = out;
}

/// Apply a preset when its button is clicked.
fn handle_preset_buttons(
    buttons: Query<(&Interaction, &PresetButton), Changed<Interaction>>,
    mut sim: ResMut<NetSim>,
) {
    for (interaction, button) in buttons.iter() {
        if *interaction == Interaction::Pressed {
            sim.set_preset(button.0);
            info!("netsim: preset set to `{}`", button.0.label());
        }
    }
}

/// Highlight whichever preset button is currently active.
fn tint_preset_buttons(sim: Res<NetSim>, mut buttons: Query<(&PresetButton, &mut BackgroundColor)>) {
    if !sim.is_changed() {
        return;
    }
    for (button, mut bg) in buttons.iter_mut() {
        bg.0 = if button.0 == sim.preset {
            BTN_ACTIVE
        } else {
            BTN_IDLE
        };
    }
}
