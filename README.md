# tauri-plugin-steam-overlay-surface

Make the Steam in-game overlay (Shift+Tab) work in [Tauri](https://tauri.app) apps.

[Demo video](https://youtu.be/vc39LuDtJtM) — the overlay opening over
[Spirefall](https://spirefall.com), a Tauri-shipped Steam game, being captured
in OBS like any native title.

## The problem

Steam's overlay works by injecting `gameoverlayrenderer64.dll` into the game
process and hooking the graphics API's Present call (D3D/Vulkan/GL). A Tauri
app never creates a swapchain in its own process — WebView2 renders in
separate `msedgewebview2.exe` processes and composites via DWM — so the hook
finds nothing and Shift+Tab is a no-op. This is the norm for webview-shell
games, and the reason Electron games need `--in-process-gpu`.

## The mechanism

This plugin gives Steam a render target: a **decoy swapchain** on a
transparent, click-through, borderless window that exactly covers your main
window. A dedicated thread presents empty (fully transparent) frames at vsync
via [wgpu](https://wgpu.rs). Steam's injected layer composites the overlay UI,
notifications, and toasts into those frames at Present time — the game stays
visible through every untouched pixel.

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

While the overlay is open, the plugin also paints a frozen snapshot of your
game (captured via `PrintWindow` the instant the overlay activated) behind
Steam's UI, so Steam dims a "game frame" exactly like a native title instead
of compositing onto black.

## Usage

```toml
# Cargo.toml — the plugin needs tauri's `unstable` feature (raw,
# webview-less windows).
tauri = { version = "2", features = ["unstable"] }
tauri-plugin-steam-overlay-surface = "0.1"
```

```rust
fn main() {
  // 1. SteamAPI_Init BEFORE building the Tauri app: the injected DLL must
  //    be resident before the plugin creates its wgpu device, or the
  //    Present hook misses the swapchain. Do not reorder.
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
      // 3. Forward Steam's GameOverlayActivated callback — the plugin
      //    hands input to the overlay and back to your game.
      if let Some(client) = &steam {
        let handle = app.handle().clone();
        let cb = client.register_callback(move |ev: steamworks::GameOverlayActivated| {
          tauri_plugin_steam_overlay_surface::on_overlay_activated(&handle, ev.active);
        });
        std::mem::forget(cb); // keep registered for the app's lifetime
      }
      Ok(())
    })
    .run(tauri::generate_context!())
    .unwrap();
}
```

One more piece belongs in your **frontend**: the Shift+Tab chord lands in the
webview process, which Steam's input hooks never see. Forward it:

```js
window.addEventListener("keydown", (e) => {
  if (e.shiftKey && e.key === "Tab") {
    e.preventDefault();
    invoke("activate_overlay"); // your command calling activate_game_overlay("")
  }
});
```

Configuration via the builder:

```rust
tauri_plugin_steam_overlay_surface::Builder::new()
  .main_window_label("main")                 // window the surface covers
  .overlay_title("MyGame Overlay Surface")   // decoy window title
  .snapshot_backdrop(true)                   // frozen game frame behind Steam's UI
  .build()
```

`surface_active()` reports whether the decoy is presenting — combined with
Steamworks' `is_overlay_enabled()` it makes a good settings/QA readout
("Shift+Tab should visibly work").

The plugin deliberately has **no `steamworks` dependency**: your app owns
Steam init and the callback pump, and forwards the one callback the plugin
needs. No version coupling with your Steamworks bindings.

## Try it (Spacewar example)

Anyone with the Steam client running can test against Valve's public test
app, Spacewar (AppID 480):

```powershell
cargo run -p spacewar-overlay-example
# window opens → press Shift+Tab
```

The example vendors `steam_api64.dll` (Valve's redistributable, from the
Steamworks SDK `redistributable_bin/`) and copies it next to the exe at build
time — steamworks-rs loads it dynamically at runtime.

## Hard-won invariants

Every one of these was learned from a live failure. If you fork or vendor
this code, keep them:

1. **`SteamAPI_Init` before wgpu device creation** — the injected DLL hooks
   device/swapchain creation. Init Steam before `tauri::Builder::run`.
2. **Size the window before creating the swapchain, and sync against the
   overlay's ACTUAL size** — the window is born at Tauri's 800×600 default;
   Vulkan clamps the swapchain extent to the real window size. Get it wrong
   and Steam renders the entire overlay into a corner.
3. **Never `always_on_top`** — owned windows already float above their owner
   and *only* their owner. Always-on-top floated above every app and ate
   clicks into other apps after alt-tab while the overlay was open.
4. **The sheet is visible ONLY while the overlay is open** (plus the boot
   window before first activation). After the first activation Steam's hook
   paints opaque frames into every present, so a visible decoy with the
   overlay closed is a solid black sheet over the game.
5. **Return focus to the main window when the overlay closes** — otherwise
   the webview goes deaf (keyboard stays on the decoy window).
6. **Kill switch** — ~120 consecutive failed frames disables the surface
   (black-screen guard). No transparent composite-alpha mode ⇒ abort before
   presenting anything (a black sheet over the game is worse than no
   overlay).
7. **Poll `GetForegroundWindow`; never trust decoy focus events** — raw
   decoy `Focused` events simply never fire (tao). A watchdog thread polls
   the foreground window and hides the sheet when neither of our windows is
   foreground — but ONLY while the overlay is closed: on activation Steam's
   own input window takes the foreground, and treating that as an alt-tab
   caused a show/hide fight that locked the game out.
8. **Recreate the swapchain on every hide→show transition** — Windows can
   keep the last composited frame (Steam's opaque overlay sheet) glued to a
   re-shown window; transparent clears never reached the screen.
9. **Restore cursor events whenever the sheet re-shows mid-overlay** — a
   watchdog hide sets click-through; re-showing without accepting cursor
   events sends clicks through the invisible overlay into the game
   underneath.
10. **Pause presenting while hidden** — a Fifo present against an occluded
    window can block inside the driver indefinitely.
11. **Keep geometry synced even while hidden** — the hidden branch skips
    presenting but still tracks main-window size/position, so the first
    frame after Shift+Tab isn't stretched from a stale size.
12. **Refocus the WEBVIEW, not just the window, when the game comes back** —
    alt-tab back can re-activate the decoy (it held focus while the overlay
    was open), and even when the main window is activated WebView2 doesn't
    reliably regain keyboard focus with it. Either way, keys (including the
    Shift+Tab forwarder) go nowhere until the user clicks the page. On focus
    regain with the overlay closed, and on overlay close, the plugin calls
    `webview.set_focus()` explicitly.

## Capture / streaming notes (OBS)

- **No display affinity on the decoy — don't add it.**
  `WDA_EXCLUDEFROMCAPTURE` renders the excluded window as an OPAQUE BLACK
  sheet (not removed) in the capture paths OBS and screenshot tools actually
  use (DXGI duplication, WGC). Without it, captures show game + overlay
  exactly like a native title.
- The decoy carries `WS_EX_TOOLWINDOW`, keeping it out of OBS's window picker
  and auto-matching (OBS re-matches sources by exe+class on process restart
  and can latch onto the decoy). tao rewrites the extended style from cached
  flags on show/style changes, so the plugin re-asserts the bit every render
  iteration — a one-shot set does not survive.
- OBS's capture method must be **WGC** ("Windows 10 1903 and up", or
  Automatic on OBS 28+). The legacy BitBlt method shows black for
  hardware-accelerated WebView2 content — true for Chrome/Electron apps
  generally, not a plugin bug.
- The snapshot backdrop uses `PrintWindow(PW_CLIENTONLY |
  PW_RENDERFULLCONTENT)`, not a screen-rect BitBlt — it renders the window's
  own composited content, immune to whatever overlaps the game at capture
  time. `PW_RENDERFULLCONTENT` is undocumented but load-bearing: without it,
  accelerated windows print black.

## Caveats and status

- **Windows only** for now (the demo shipped Windows-only). On other
  platforms every call is a graceful no-op. The approach should translate —
  PRs welcome.
- **Alpha mode `Inherit`** (what Vulkan reports on Windows): transparency is
  platform-defined per spec. Works in practice; `WGPU_BACKEND=dx12|vulkan|gl`
  forces a backend for experiments without a rebuild.
- **Semi-transparent overlay pixels** (Steam's dim layer) may blend slightly
  differently than native: Steam renders unpremultiplied alpha. Cosmetic;
  panels are opaque and unaffected.
- Verified on a real Steam-launched build (2026-07-24): open/close/alt-tab
  cycles, multi-resolution ladder (1920×1080 / 2560×1440 / 5120×1440, live
  switches, fullscreen↔windowed), OBS capture.
- Steam persists overlay panel positions per-game in pixels — after
  shrinking the surface (e.g. ultrawide → windowed) a panel can sit
  offscreen. Steam-side behavior; re-summon it via the overlay toolbar.

## Credits

- [qwook/tauri-plugin-steam-overlay](https://github.com/qwook/tauri-plugin-steam-overlay)
  validated the decoy-swapchain approach for Tauri.
- [tauri-apps discussion #11944](https://github.com/tauri-apps/tauri/discussions/11944)
  for the original problem framing.
- Built for [Spirefall](https://spirefall.com) by
  [PSG Studios](https://github.com/PSG-Team).

## License

[MIT](LICENSE)
