//! Minimal Tauri app demonstrating the Steam overlay surface plugin against
//! Spacewar (AppID 480), Valve's public test app — anyone with the Steam
//! client running can launch this and press Shift+Tab.

use std::time::Duration;

/// `None` when Steam isn't running or init failed — the app still works,
/// just without an overlay (mirrors how a shipped game should degrade).
struct SteamState(Option<steamworks::Client>);

#[derive(serde::Serialize)]
struct OverlayStatus {
    steam_available: bool,
    overlay_enabled: bool,
    surface_active: bool,
}

#[tauri::command]
fn overlay_status(state: tauri::State<'_, SteamState>) -> OverlayStatus {
    OverlayStatus {
        steam_available: state.0.is_some(),
        overlay_enabled: state
            .0
            .as_ref()
            .map(|c| c.utils().is_overlay_enabled())
            .unwrap_or(false),
        surface_active: tauri_plugin_steam_overlay_surface::surface_active(),
    }
}

/// Shift+Tab lands in the webview process, which Steam's input hooks never
/// see — the page forwards the chord here.
#[tauri::command]
fn activate_overlay(state: tauri::State<'_, SteamState>) -> bool {
    let Some(client) = state.0.as_ref() else {
        return false;
    };
    if !client.utils().is_overlay_enabled() {
        return false;
    }
    client.friends().activate_game_overlay("");
    true
}

/// F12 has the same routing problem as Shift+Tab: it lands in the webview
/// process, so Steam's hook only sees it while the overlay is open (the
/// decoy window holds focus then). The page forwards it here;
/// TriggerScreenshot fires the hooked ScreenshotRequested callback.
#[tauri::command]
fn trigger_screenshot(state: tauri::State<'_, SteamState>) -> bool {
    let Some(client) = state.0.as_ref() else {
        return false;
    };
    client.screenshots().trigger_screenshot();
    true
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // SteamAPI_Init BEFORE the Tauri app is built: the injected overlay DLL
    // must be resident before the plugin creates its wgpu device, or the
    // Present hook misses the swapchain.
    let steam = match steamworks::Client::init_app(480) {
        Ok(client) => {
            log::info!("steamworks initialized (Spacewar, app 480)");
            // Overlay + async Steamworks events need run_callbacks serviced.
            let pump = client.clone();
            std::thread::spawn(move || loop {
                pump.run_callbacks();
                std::thread::sleep(Duration::from_millis(100));
            });
            Some(client)
        }
        Err(err) => {
            log::warn!("steamworks unavailable ({err}) — running without Steam");
            None
        }
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_steam_overlay_surface::init())
        .manage(SteamState(steam))
        .invoke_handler(tauri::generate_handler![
            overlay_status,
            activate_overlay,
            trigger_screenshot
        ])
        .setup(|app| {
            use tauri::Manager;
            let state = app.state::<SteamState>();
            if let Some(client) = state.0.as_ref() {
                // Forward GameOverlayActivated so the plugin can hand input to the
                // overlay and back to the game.
                let handle = app.handle().clone();
                let cb = client.register_callback(move |ev: steamworks::GameOverlayActivated| {
                    tauri_plugin_steam_overlay_surface::on_overlay_activated(&handle, ev.active);
                });
                // Registered for the app's entire lifetime (dropping unregisters).
                std::mem::forget(cb);

                // Hooked screenshots: Steam's default F12 grabs the hooked
                // swapchain's backbuffer, which never contains the game (the
                // decoy presents nothing while the overlay is closed). Hook
                // them and hand Steam a live PrintWindow frame instead.
                client.screenshots().hook_screenshots(true);
                let handle = app.handle().clone();
                let shot_client = client.clone();
                let cb = client.register_callback(
                    move |_: steamworks::screenshots::ScreenshotRequested| {
                        let Some(shot) =
                            tauri_plugin_steam_overlay_surface::capture_screenshot_png(&handle)
                        else {
                            log::warn!("screenshot capture failed; F12 dropped");
                            return;
                        };
                        if let Err(err) = shot_client.screenshots().add_screenshot_to_library(
                            &shot.path,
                            None,
                            shot.width as i32,
                            shot.height as i32,
                        ) {
                            log::warn!("add_screenshot_to_library failed ({err})");
                        }
                    },
                );
                std::mem::forget(cb);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running example app");
}
