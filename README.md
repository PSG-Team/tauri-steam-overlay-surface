# tauri-plugin-steam-overlay-surface

[![crates.io](https://img.shields.io/crates/v/tauri-plugin-steam-overlay-surface.svg)](https://crates.io/crates/tauri-plugin-steam-overlay-surface)
[![docs.rs](https://docs.rs/tauri-plugin-steam-overlay-surface/badge.svg)](https://docs.rs/tauri-plugin-steam-overlay-surface)
[![license](https://img.shields.io/crates/l/tauri-plugin-steam-overlay-surface.svg)](LICENSE)

Makes the Steam in-game overlay (Shift+Tab) and Steam screenshots (F12) work
in [Tauri](https://tauri.app) apps.

[Demo video](https://youtu.be/vc39LuDtJtM): the overlay opening over
[Spirefall](https://spirefall.com), a Steam game built with Tauri, captured
in OBS like any native title.

Works with any frontend (React, Vue, Svelte, vanilla). The plugin is pure
Rust and ships no JavaScript package. The only frontend code you need is a
small keydown listener that forwards Shift+Tab to Rust (see
[Usage](#usage)).

## Why the overlay doesn't work in Tauri

Steam shows its overlay by injecting `gameoverlayrenderer64.dll` into the
game process and hooking the graphics API's Present call (D3D/Vulkan/GL). A
Tauri app never presents frames from its own process: WebView2 renders in
separate `msedgewebview2.exe` processes and Windows composites the result.
Steam's hook finds nothing to attach to, so Shift+Tab does nothing. All
webview-based games have this problem; it's why Electron games need
`--in-process-gpu`.

## How the plugin fixes it

The plugin gives Steam something to hook: a transparent, click-through,
borderless "decoy" window that exactly covers your main window, with a real
swapchain presenting empty frames at vsync via [wgpu](https://wgpu.rs).
Steam's injected DLL draws the overlay UI, notifications, and toasts into
those frames. Every pixel Steam doesn't touch stays transparent, so your
game shows through.

```
┌────────────────────────── game process ──────────────────────────┐
│                                                                  │
│  WebView2 (out of process) ──────────────────────► your game UI  │
│                                                                  │
│  plugin: decoy window + wgpu swapchain ──► transparent frames    │
│                    ▲                             │               │
│  gameoverlayrenderer64.dll ── hooks Present ─────┤               │
│                                                  ▼               │
│                              Steam overlay composited on top     │
└──────────────────────────────────────────────────────────────────┘
```

While the overlay is open, the plugin also draws a frozen screenshot of your
game (taken with `PrintWindow` the moment the overlay opened) behind Steam's
UI. Steam then dims a game frame the way it does for native titles, instead
of dimming black.

## Installation

```toml
# Cargo.toml — the plugin needs tauri's `unstable` feature
# (windows without a webview).
tauri = { version = "2", features = ["unstable"] }

tauri-plugin-steam-overlay-surface = "0.1"
```

## Usage

```rust
fn main() {
  // 1. SteamAPI_Init BEFORE building the Tauri app: the injected DLL must
  //    be loaded before the plugin creates its wgpu device, or the Present
  //    hook misses the swapchain. Do not reorder.
  let steam = steamworks::Client::init_app(YOUR_APP_ID).ok();
  if let Some(client) = &steam {
    let pump = client.clone();
    std::thread::spawn(move || loop {
      pump.run_callbacks();
      std::thread::sleep(std::time::Duration::from_millis(100));
    });
  }

  tauri::Builder::default()
    // 2. The plugin spawns the decoy surface once your main window exists.
    .plugin(tauri_plugin_steam_overlay_surface::init())
    .setup(move |app| {
      if let Some(client) = &steam {
        // 3. Forward Steam's GameOverlayActivated callback. The plugin
        //    hands input to the overlay and back to your game.
        let handle = app.handle().clone();
        let cb = client.register_callback(move |ev: steamworks::GameOverlayActivated| {
          tauri_plugin_steam_overlay_surface::on_overlay_activated(&handle, ev.active);
        });
        std::mem::forget(cb); // keep registered for the app's lifetime

        // 4. Hook Steam screenshots. Without this, F12 captures nothing —
        //    see "Steam screenshots (F12)" below.
        client.screenshots().hook_screenshots(true);
        let handle = app.handle().clone();
        let shot_client = client.clone();
        let cb = client.register_callback(move |_: steamworks::screenshots::ScreenshotRequested| {
          if let Some(shot) = tauri_plugin_steam_overlay_surface::capture_screenshot_png(&handle) {
            let _ = shot_client.screenshots().add_screenshot_to_library(
              &shot.path, None, shot.width as i32, shot.height as i32);
          }
        });
        std::mem::forget(cb);
      }
      Ok(())
    })
    .run(tauri::generate_context!())
    .unwrap();
}
```

One piece belongs in your **frontend**: keystrokes land in the webview
process, which Steam's input hooks never see. That affects both Shift+Tab
and F12, so forward them:

```js
window.addEventListener("keydown", (e) => {
  if (e.shiftKey && e.key === "Tab") {
    e.preventDefault();
    invoke("activate_overlay");    // your command calling activate_game_overlay("")
  } else if (e.key === "F12") {
    e.preventDefault();
    invoke("trigger_screenshot");  // your command calling screenshots().trigger_screenshot()
  }
});
```

### Configuration

```rust
tauri_plugin_steam_overlay_surface::Builder::new()
  .main_window_label("main")                 // window the surface covers
  .overlay_title("MyGame Overlay Surface")   // decoy window title
  .snapshot_backdrop(true)                   // frozen game frame behind Steam's UI
  .build()
```

`surface_active()` reports whether the decoy is presenting. Combined with
Steamworks' `is_overlay_enabled()` it makes a good settings/QA readout
("Shift+Tab should visibly work").

The plugin has **no `steamworks` dependency** on purpose: your app owns
Steam init and the callback pump, and forwards the callbacks the plugin
needs. No version coupling with your Steamworks bindings.

## Steam screenshots (F12)

Out of the box, F12 does nothing while the overlay is closed, and saves a
stale frame while it's open. Steam's screenshot handler copies the hooked
swapchain's backbuffer, and here that backbuffer never contains your game:
with the overlay closed the plugin doesn't present at all, and with it open
the buffer holds the frozen backdrop snapshot. This is a consequence of the
decoy design and behaves the same on every Windows version.

The fix has two halves:

**1. Hooked screenshots** (step 4 of [Usage](#usage)):
`ISteamScreenshots::HookScreenshots(true)`. Steam then sends the game a
`ScreenshotRequested` callback instead of reading the backbuffer, and the
game supplies the image. The plugin's `capture_screenshot_png` takes a live
capture of your main window (`PrintWindow`, same mechanism as the backdrop
snapshot, safe to call from the callback pump thread), writes it to a temp
PNG, and returns the path and dimensions for `add_screenshot_to_library`.
Old temp files are cleaned up on later captures.

**2. Forwarding F12 from the frontend** (the JS snippet in
[Usage](#usage)): F12 has the same routing problem as Shift+Tab — it lands
in the webview process, so Steam's input hook only sees it while the overlay
is open (the decoy window holds keyboard focus then). With the overlay
closed, F12 must be forwarded to a command that calls
`screenshots().trigger_screenshot()`, which fires the same
`ScreenshotRequested` callback a native F12 would.

Screenshots taken this way are live in every state and, like native games,
don't include the Steam UI in the shot. One limitation: if the player
rebinds Steam's screenshot key, the frontend forward still only catches F12
(Steamworks has no API to query the binding). The rebound key keeps working
while the overlay is open.

## Try it (Spacewar example)

Anyone with the Steam client running can test against Valve's public test
app, Spacewar (AppID 480):

```powershell
cargo run -p spacewar-overlay-example
# window opens → press Shift+Tab
```

The example vendors `steam_api64.dll` (Valve's redistributable, from the
Steamworks SDK `redistributable_bin/`) and copies it next to the exe at
build time. steamworks-rs loads it dynamically at runtime.

## Implementation notes

Every rule below exists because breaking it caused a real failure. If you
fork or vendor this code, keep them:

1. **`SteamAPI_Init` before wgpu device creation.** The injected DLL hooks
   device/swapchain creation. Init Steam before `tauri::Builder::run`.
2. **Size the window before creating the swapchain, and sync against the
   overlay window's actual size.** The window is born at Tauri's 800×600
   default and Vulkan clamps the swapchain extent to the real window size.
   Get it wrong and Steam renders the entire overlay into a corner.
3. **Never `always_on_top`.** Owned windows already float above their owner
   and only their owner. Always-on-top floated above every app and ate
   clicks meant for other apps after alt-tab while the overlay was open.
4. **The sheet is visible only while the overlay is open** (plus the boot
   window before first activation). After the first activation Steam's hook
   paints opaque frames into every present, so a visible decoy with the
   overlay closed is a solid black sheet over the game.
5. **Return focus to the main window when the overlay closes**, otherwise
   the webview goes deaf (keyboard stays on the decoy window).
6. **Kill switch:** ~120 consecutive failed frames disables the surface
   (black-screen guard). If the surface offers no transparent
   composite-alpha mode, abort before presenting anything; a black sheet
   over the game is worse than no overlay.
7. **Poll `GetForegroundWindow`; don't trust decoy focus events.** `Focused`
   events on the raw decoy window never fire (tao). A watchdog thread polls
   the foreground window and hides the sheet when neither of our windows is
   foreground — but only while the overlay is closed: on activation Steam's
   own input window takes the foreground, and treating that as an alt-tab
   caused a show/hide fight that locked the game out.
8. **Recreate the swapchain on every hide→show transition.** Windows can
   keep the last composited frame (Steam's opaque overlay sheet) glued to a
   re-shown window, and transparent clears never reach the screen.
9. **Restore cursor events whenever the sheet re-shows mid-overlay.** A
   watchdog hide sets click-through; re-showing without accepting cursor
   events sends clicks through the invisible overlay into the game
   underneath.
10. **Pause presenting while hidden.** A Fifo present against an occluded
    window can block inside the driver indefinitely.
11. **Keep geometry synced even while hidden.** The hidden branch skips
    presenting but still tracks main-window size/position, so the first
    frame after Shift+Tab isn't stretched from a stale size.

## OBS / capture notes

- **Don't add display affinity to the decoy.** `WDA_EXCLUDEFROMCAPTURE`
  renders the excluded window as an opaque black sheet (not removed) in the
  capture paths OBS and screenshot tools actually use (DXGI duplication,
  WGC). Without it, captures show game + overlay like a native title.
- The decoy carries `WS_EX_TOOLWINDOW`, keeping it out of OBS's window
  picker and auto-matching (OBS re-matches sources by exe+class on process
  restart and can latch onto the decoy). tao rewrites the extended style
  from cached flags on show/style changes, so the plugin re-asserts the bit
  every render iteration; setting it once does not survive.
- OBS's capture method must be **WGC** ("Windows 10 1903 and up", or
  Automatic on OBS 28+). The legacy BitBlt method shows black for
  hardware-accelerated WebView2 content. That's true for Chrome/Electron
  apps in general, not a plugin bug.
- The snapshot backdrop uses `PrintWindow(PW_CLIENTONLY |
  PW_RENDERFULLCONTENT)`, not a screen-rect BitBlt: it renders the window's
  own composited content, unaffected by whatever overlaps the game at
  capture time. `PW_RENDERFULLCONTENT` is undocumented but required;
  without it, accelerated windows print black.

## Caveats and status

- **Windows only** for now. On other platforms every call is a graceful
  no-op. The approach should translate; PRs welcome.
- **Alpha mode `Inherit`** (what Vulkan reports on Windows): transparency
  is platform-defined per spec. Works in practice. `WGPU_BACKEND=dx12|vulkan|gl`
  forces a backend for experiments without a rebuild.
- **Semi-transparent overlay pixels** (Steam's dim layer) may blend slightly
  differently than native, since Steam renders unpremultiplied alpha.
  Cosmetic; panels are opaque and unaffected.
- **F12 screenshots need both the hooked-screenshots wiring and the
  frontend F12 forward** (step 4 and the JS snippet in [Usage](#usage)).
  Without them, Steam reads the decoy's backbuffer, which never contains
  the game: no screenshot with the overlay closed, a stale frozen frame
  with it open. See [Steam screenshots (F12)](#steam-screenshots-f12).
- **One click needed after alt-tab.** After alt-tabbing back into the game,
  the Shift+Tab forwarder is deaf until the player clicks the page once.
  Windows re-activates the native window without returning keyboard focus
  to the webview. Don't try to fix this by calling `webview.set_focus()`
  from the focus/foreground handlers: v0.1.1 shipped exactly that and it
  broke Shift+Tab entirely, even on fresh boot (reverted in v0.1.2).
- Verified on a real Steam-launched build (2026-07-24): open/close/alt-tab
  cycles, multi-resolution ladder (1920×1080 / 2560×1440 / 5120×1440, live
  switches, fullscreen↔windowed), OBS capture.
- Steam stores overlay panel positions per game in pixels, so after
  shrinking the surface (e.g. ultrawide → windowed) a panel can sit
  offscreen. That's Steam-side behavior; re-summon the panel via the
  overlay toolbar.

## Credits

- [qwook/tauri-plugin-steam-overlay](https://github.com/qwook/tauri-plugin-steam-overlay)
  validated the decoy-swapchain approach for Tauri.
- [tauri-apps discussion #11944](https://github.com/tauri-apps/tauri/discussions/11944)
  for the original problem framing.
- Built for [Spirefall](https://spirefall.com) by
  [PSG Studios](https://github.com/PSG-Team).

## License

[MIT](LICENSE)
