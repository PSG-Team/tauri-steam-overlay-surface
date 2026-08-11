//! Frozen game-frame backdrop.
//!
//! When the overlay opens we capture the main window's client area and the
//! render thread paints it behind Steam's UI, so Steam dims a "game frame"
//! exactly like a native title instead of compositing onto black.

use std::sync::Mutex;

use tauri::{Runtime, Window};

#[cfg(windows)]
#[link(name = "user32")]
extern "system" {
    fn GetDC(hwnd: isize) -> isize;
    fn ReleaseDC(hwnd: isize, hdc: isize) -> i32;
    fn PrintWindow(hwnd: isize, hdc: isize, flags: u32) -> i32;
}

#[cfg(windows)]
#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(hdc: isize) -> isize;
    fn CreateCompatibleBitmap(hdc: isize, w: i32, h: i32) -> isize;
    fn SelectObject(hdc: isize, obj: isize) -> isize;
    fn GetDIBits(
        hdc: isize,
        bmp: isize,
        start: u32,
        lines: u32,
        bits: *mut u8,
        info: *mut BitmapInfo,
        usage: u32,
    ) -> i32;
    fn DeleteDC(hdc: isize) -> i32;
    fn DeleteObject(obj: isize) -> i32;
}

/// PrintWindow: render only the client area (no title bar / frame).
#[cfg(windows)]
const PW_CLIENTONLY: u32 = 0x1;
/// PrintWindow: render DWM-composited content (WebView2 output) — without
/// this, accelerated windows print black (undocumented but load-bearing).
#[cfg(windows)]
const PW_RENDERFULLCONTENT: u32 = 0x2;

#[cfg(windows)]
#[repr(C)]
struct BitmapInfo {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_ppm: i32,
    bi_y_ppm: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
    // BITMAPINFO color table space (unused for 32bpp BI_RGB).
    bmi_colors: [u32; 3],
}

/// A frozen frame of the game, captured the instant the overlay opened.
/// BGRA, tightly packed rows.
pub(crate) struct Snapshot {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Handoff slot between the Steam callback (writer) and the render thread
/// (reader). `dirty` marks a state change the render thread must consume:
/// Some = new snapshot to upload, None = drop the texture (overlay closed).
struct SnapshotSlot {
    data: Option<Snapshot>,
    dirty: bool,
}

static SNAPSHOT: Mutex<SnapshotSlot> = Mutex::new(SnapshotSlot {
    data: None,
    dirty: false,
});

/// Render-thread side: consume a pending state change, if any.
/// `Some(Some(snap))` = upload a fresh backdrop, `Some(None)` = drop it,
/// `None` = nothing changed since last poll.
pub(crate) fn take_pending() -> Option<Option<Snapshot>> {
    let mut slot = SNAPSHOT.lock().unwrap();
    if slot.dirty {
        slot.dirty = false;
        Some(slot.data.take())
    } else {
        None
    }
}

/// Grab a window's client area (physical pixels) via PrintWindow with
/// PW_RENDERFULLCONTENT — renders the window's own DWM-composited content
/// (WebView2 output included), so the capture is immune to whatever overlaps
/// the window at that instant (a screen-rect BitBlt would capture the decoy
/// sheet instead of the game). Returns tightly packed BGRA rows, top-down.
#[cfg(windows)]
pub(crate) fn capture_window_bgra<R: Runtime>(window: &Window<R>) -> Option<Snapshot> {
    let (Ok(hwnd), Ok(size)) = (window.hwnd(), window.inner_size()) else {
        return None;
    };
    if size.width == 0 || size.height == 0 {
        return None;
    }
    let (w, h) = (size.width as i32, size.height as i32);

    unsafe {
        let screen = GetDC(0);
        if screen == 0 {
            return None;
        }
        let memdc = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let mut bgra = Vec::new();
        let mut ok = false;
        if memdc != 0 && bmp != 0 {
            let old = SelectObject(memdc, bmp);
            if PrintWindow(hwnd.0 as isize, memdc, PW_CLIENTONLY | PW_RENDERFULLCONTENT) != 0 {
                let mut info = BitmapInfo {
                    bi_size: 40, // sizeof(BITMAPINFOHEADER)
                    bi_width: w,
                    bi_height: -h, // negative = top-down rows
                    bi_planes: 1,
                    bi_bit_count: 32,
                    bi_compression: 0, // BI_RGB
                    bi_size_image: 0,
                    bi_x_ppm: 0,
                    bi_y_ppm: 0,
                    bi_clr_used: 0,
                    bi_clr_important: 0,
                    bmi_colors: [0; 3],
                };
                bgra = vec![0u8; (w as usize) * (h as usize) * 4];
                ok = GetDIBits(memdc, bmp, 0, h as u32, bgra.as_mut_ptr(), &mut info, 0) == h;
            }
            SelectObject(memdc, old);
        }
        if bmp != 0 {
            DeleteObject(bmp);
        }
        if memdc != 0 {
            DeleteDC(memdc);
        }
        ReleaseDC(0, screen);

        ok.then_some(Snapshot {
            width: size.width,
            height: size.height,
            bgra,
        })
    }
}

#[cfg(not(windows))]
pub(crate) fn capture_window_bgra<R: Runtime>(_window: &Window<R>) -> Option<Snapshot> {
    None
}

/// Capture the main window for the frozen backdrop. Called from
/// GameOverlayActivated(true) BEFORE the sheet is shown; the render thread
/// paints the result so Steam dims a "game frame" instead of transparent
/// black.
pub(crate) fn capture_main_snapshot<R: Runtime>(main: &Window<R>) {
    match capture_window_bgra(main) {
        Some(snap) => {
            let mut slot = SNAPSHOT.lock().unwrap();
            slot.data = Some(snap);
            slot.dirty = true;
        }
        None => log::warn!(
            "steam-overlay-surface: snapshot capture failed; backdrop stays transparent"
        ),
    }
}

/// Drop the frozen backdrop (overlay closed) — the sheet goes back to
/// transparent clears next time it's shown.
pub(crate) fn clear_snapshot() {
    let mut slot = SNAPSHOT.lock().unwrap();
    slot.data = None;
    slot.dirty = true;
}
