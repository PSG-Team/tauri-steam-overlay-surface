//! Steam in-game overlay (Shift+Tab) support for Tauri apps.
//!
//! Steam's overlay works by injecting `gameoverlayrenderer64.dll` into the
//! game process and hooking the graphics API's Present call. A Tauri app
//! never creates a swapchain in its own process (WebView2/WebKit render
//! out-of-process and composite via the OS), so the hook finds nothing and
//! Shift+Tab is a no-op.
//!
//! This plugin gives Steam a target: a transparent, click-through window
//! covering your main window, with a real in-process wgpu swapchain
//! presenting empty (fully transparent) frames at vsync. Steam composites
//! its overlay UI and notification toasts into those frames at Present time.
//!
//! # Usage
//!
//! ```rust,ignore
//! // 1. In main(), SteamAPI_Init BEFORE building the Tauri app: the
//! //    injected DLL must be resident before the plugin creates its wgpu
//! //    device.
//! let steam = steamworks::Client::init_app(480).ok();
//!
//! tauri::Builder::default()
//!   // 2. Register the plugin. It spawns the decoy surface as soon as
//!   //    your main window exists.
//!   .plugin(tauri_plugin_steam_overlay_surface::init())
//!   .setup(move |app| {
//!     // 3. Forward Steam's GameOverlayActivated callback so the plugin
//!     //    can hand input to the overlay and back to your game.
//!     if let Some(client) = &steam {
//!       let handle = app.handle().clone();
//!       let cb = client.register_callback(
//!         move |ev: steamworks::GameOverlayActivated| {
//!           tauri_plugin_steam_overlay_surface::on_overlay_activated(&handle, ev.active);
//!         },
//!       );
//!       std::mem::forget(cb); // keep registered for the app's lifetime
//!     }
//!     Ok(())
//!   })
//!   .run(tauri::generate_context!())
//!   .unwrap();
//! ```
//!
//! The plugin deliberately has **no `steamworks` dependency** — your app owns
//! Steam init and its callback pump, and forwards the one callback the plugin
//! needs. This avoids version coupling between the plugin and your Steamworks
//! bindings.
//!
//! Requires tauri's `unstable` feature (raw, webview-less windows).
//! Windows-only today; on other platforms every call is a graceful no-op.

mod snapshot;
mod surface;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::plugin::{Builder as PluginBuilder, TauriPlugin};
use tauri::{AppHandle, Manager, Runtime};

pub use surface::OVERLAY_LABEL;

/// Plugin configuration. Construct via [`Builder`].
#[derive(Clone)]
struct Config {
    /// Label of the window the decoy surface covers.
    main_window_label: String,
    /// Title of the decoy window (shows up in tooling that enumerates
    /// windows, so make it recognizable).
    overlay_title: String,
    /// Capture a frozen frame of the game when the overlay opens and paint it
    /// behind Steam's UI, so Steam dims a "game frame" instead of black.
    snapshot_backdrop: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            main_window_label: "main".into(),
            overlay_title: "Steam Overlay Surface".into(),
            snapshot_backdrop: true,
        }
    }
}

/// Managed-state wrapper so [`on_overlay_activated`] can read the config.
struct ConfigState(Config);

/// Configures the plugin before registration.
#[derive(Default)]
pub struct Builder {
    config: Config,
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Label of the window the overlay surface covers (default `"main"`).
    pub fn main_window_label(mut self, label: impl Into<String>) -> Self {
        self.config.main_window_label = label.into();
        self
    }

    /// Title of the decoy window (default `"Steam Overlay Surface"`).
    pub fn overlay_title(mut self, title: impl Into<String>) -> Self {
        self.config.overlay_title = title.into();
        self
    }

    /// Enable/disable the frozen-game-frame backdrop while the overlay is
    /// open (default `true`). Disabled, Steam dims transparent black instead.
    pub fn snapshot_backdrop(mut self, enabled: bool) -> Self {
        self.config.snapshot_backdrop = enabled;
        self
    }

    pub fn build<R: Runtime>(self) -> TauriPlugin<R> {
        let config = self.config;
        let setup_config = config.clone();
        let spawned = Arc::new(AtomicBool::new(false));
        PluginBuilder::new("steam-overlay-surface")
            .setup(move |app, _api| {
                app.manage(ConfigState(setup_config));
                Ok(())
            })
            .on_window_ready(move |window| {
                // Fires for every window (including the decoy we create) — only the
                // configured main window, exactly once, spawns the surface.
                if window.label() == config.main_window_label
                    && !spawned.swap(true, Ordering::SeqCst)
                {
                    // Deadlock guard: this hook runs inside the main window's
                    // creation path on the main thread — building the decoy
                    // window re-entrantly here wedges the whole app ("Not
                    // Responding", proven live on the Spacewar example).
                    // Defer to a background thread: window creation from
                    // non-main threads proxies through the event loop once it
                    // pumps, which is the standard supported path.
                    let config = config.clone();
                    let spawn_thread = std::thread::Builder::new()
                        .name("steam-overlay-spawn".into())
                        .spawn(move || surface::spawn(window, config));
                    if let Err(err) = spawn_thread {
                        log::warn!("steam-overlay-surface: spawn thread failed to start ({err})");
                    }
                }
            })
            .build()
    }
}

/// Initialize the plugin with default configuration.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new().build()
}

/// Whether the decoy surface is up and presenting. `true` + Steam's
/// `is_overlay_enabled()` means Shift+Tab should visibly work — useful for a
/// settings/QA readout.
pub fn surface_active() -> bool {
    surface::SURFACE_ACTIVE.load(Ordering::Relaxed)
}

/// Forward Steam's `GameOverlayActivated` callback here.
///
/// Handles the full input handoff, each step learned from a live failure:
/// - overlay OPEN: freeze a game frame for the backdrop (before the sheet
///   covers it), show + focus the decoy so mouse/keyboard reach Steam's
///   in-process hooks, and accept cursor events.
/// - overlay CLOSED: hide the sheet immediately (after first activation
///   Steam paints opaque frames forever — a visible decoy blacks out the
///   game), go back to click-through, drop the backdrop, and return focus to
///   the main window (or the webview goes deaf).
pub fn on_overlay_activated<R: Runtime>(app: &AppHandle<R>, active: bool) {
    surface::STEAM_OVERLAY_ACTIVE.store(active, Ordering::Relaxed);
    log::info!(
        "steam-overlay-surface: overlay {}",
        if active { "opened" } else { "closed" }
    );
    let Some(config) = app.try_state::<ConfigState>() else {
        log::warn!("steam-overlay-surface: plugin not registered; callback ignored");
        return;
    };
    let Some(w) = app.get_window(OVERLAY_LABEL) else {
        return;
    };
    let _ = w.set_ignore_cursor_events(!active);
    if active {
        if config.0.snapshot_backdrop {
            if let Some(main) = app.get_window(&config.0.main_window_label) {
                snapshot::capture_main_snapshot(&main);
            }
        }
        let _ = w.show();
        let _ = w.set_focus();
    } else {
        let _ = w.hide();
        snapshot::clear_snapshot();
        if let Some(main) = app.get_window(&config.0.main_window_label) {
            let _ = main.set_focus();
        }
    }
}
