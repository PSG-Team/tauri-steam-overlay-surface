//! Hooked Steam screenshots (F12).
//!
//! Steam's default screenshot path grabs the hooked swapchain's backbuffer
//! at Present time. That never contains the game here: the decoy surface
//! presents nothing while the overlay is closed, and while it's open the
//! backbuffer holds the frozen backdrop snapshot — so F12 either does
//! nothing or saves a stale frame. The fix is Steamworks' own escape hatch:
//! `ISteamScreenshots::HookScreenshots(true)` makes Steam fire a
//! `ScreenshotRequested` callback instead, and the app hands Steam a live
//! frame captured here via PrintWindow (same path as the backdrop snapshot).
//!
//! F12 itself must also be forwarded from the frontend (like Shift+Tab):
//! the key lands in the webview process, so Steam's input hook only sees it
//! while the overlay is open. The app's forwarded command calls
//! `TriggerScreenshot`, which fires the same `ScreenshotRequested` callback.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager, Runtime};

use crate::{snapshot, ConfigState};

/// Files older than this are cleaned from the temp dir on each capture.
/// Generous margin: Steam copies the file into its library asynchronously
/// (`AddScreenshotToLibrary`), so a just-written file must never be touched.
const STALE_AFTER: Duration = Duration::from_secs(300);

const FILE_PREFIX: &str = "steam-overlay-surface-shot-";

/// A live game frame written to disk as PNG, ready for
/// `ISteamScreenshots::AddScreenshotToLibrary`.
pub struct CapturedScreenshot {
    /// Absolute path of the PNG in the OS temp dir. Steam copies it into
    /// the screenshot library; leftovers are cleaned up on later captures.
    pub path: PathBuf,
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
}

/// Capture the configured main window's live content and write it to a
/// temp PNG. Returns `None` on capture/encode failure or on non-Windows.
///
/// Called from the app's `ScreenshotRequested` handler (any thread — the
/// Steamworks callback pump is fine; the backdrop snapshot uses the same
/// capture from the same context).
pub fn capture_screenshot_png<R: Runtime>(app: &AppHandle<R>) -> Option<CapturedScreenshot> {
    let config = app.try_state::<ConfigState>()?;
    let main = app.get_window(&config.0.main_window_label)?;
    let snap = snapshot::capture_window_bgra(&main)?;

    // BGRA (GDI) → RGB; alpha is GDI garbage and Steam wants opaque anyway.
    let mut rgb = Vec::with_capacity((snap.width * snap.height * 3) as usize);
    for px in snap.bgra.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }

    let dir = std::env::temp_dir();
    cleanup_stale(&dir);

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = dir.join(format!("{FILE_PREFIX}{stamp}.png"));

    let file = match File::create(&path) {
        Ok(f) => f,
        Err(err) => {
            log::warn!("steam-overlay-surface: screenshot file create failed ({err})");
            return None;
        }
    };
    let mut encoder = png::Encoder::new(BufWriter::new(file), snap.width, snap.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    // Fast compression: the file is a throwaway handoff (Steam re-encodes
    // to JPG for its library), and encode time delays Steam's "screenshot
    // saved" toast — trade file size for latency.
    encoder.set_compression(png::Compression::Fast);
    let write = encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(&rgb));
    if let Err(err) = write {
        log::warn!("steam-overlay-surface: screenshot PNG encode failed ({err})");
        let _ = std::fs::remove_file(&path);
        return None;
    }

    log::info!(
        "steam-overlay-surface: screenshot captured {}x{} -> {}",
        snap.width,
        snap.height,
        path.display()
    );
    Some(CapturedScreenshot {
        path,
        width: snap.width,
        height: snap.height,
    })
}

/// Best-effort removal of our old temp PNGs (Steam has long since copied
/// them into its library).
fn cleanup_stale(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(FILE_PREFIX) || !name.ends_with(".png") {
            continue;
        }
        let old_enough = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .map(|age| age > STALE_AFTER)
            .unwrap_or(false);
        if old_enough {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
