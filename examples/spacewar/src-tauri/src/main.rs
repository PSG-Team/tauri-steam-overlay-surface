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
/// Capture the game live and hand it to Steam's screenshot library. Shared
/// by both screenshot triggers: Steam's ScreenshotRequested callback
/// (overlay open — Steam's hook sees F12) and the frontend F12 forward
/// (overlay closed — only the page sees F12).
fn capture_and_add_screenshot(client: &steamworks::Client, app: &tauri::AppHandle) -> bool {
    let Some(shot) = tauri_plugin_steam_overlay_surface::capture_screenshot_png(app) else {
        log::warn!("screenshot capture failed");
        return false;
    };
    match client.screenshots().add_screenshot_to_library(
        &shot.path,
        None,
        shot.width as i32,
        shot.height as i32,
    ) {
        Ok(handle) => {
            log::info!("screenshot added to Steam library (handle {handle:?})");
            true
        }
        Err(err) => {
            log::warn!("add_screenshot_to_library failed ({err})");
            false
        }
    }
}

/// F12 has the same routing problem as Shift+Tab: it lands in the webview
/// process, so Steam's hook only sees it while the overlay is open. The
/// page forwards it here. NOTE: this captures directly instead of calling
/// TriggerScreenshot — with hooking enabled, TriggerScreenshot never
/// delivered a ScreenshotRequested callback (verified live 2026-08-11).
#[tauri::command]
fn trigger_screenshot(app: tauri::AppHandle, state: tauri::State<'_, SteamState>) -> bool {
    let Some(client) = state.0.as_ref() else {
        return false;
    };
    capture_and_add_screenshot(client, &app)
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
                // ScreenshotRequested fires when Steam's input hook sees F12,
                // which only happens while the overlay is open (the decoy
                // holds focus); the overlay-closed case is the frontend
                // forward → trigger_screenshot command.
                client.screenshots().hook_screenshots(true);
                let handle = app.handle().clone();
                let shot_client = client.clone();
                let cb = client.register_callback(
                    move |_: steamworks::screenshots::ScreenshotRequested| {
                        capture_and_add_screenshot(&shot_client, &handle);
                    },
                );
                std::mem::forget(cb);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running example app");
}
