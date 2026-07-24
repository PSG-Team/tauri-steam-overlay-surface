//! The decoy swapchain: a transparent, click-through window covering the
//! main window, presenting empty frames at vsync for Steam's hook to
//! composite into.
//!
//! Safety properties (each learned from a live failure):
//! - The overlay window is NOT always-on-top: owned windows already float
//!   above their owner and only their owner — always-on-top floated above
//!   other apps and ate clicks after alt-tab while the overlay was open.
//! - After Steam's overlay activates once, its hook paints opaque frames
//!   into the swapchain forever, so the sheet is only visible WHILE the
//!   overlay is open (plus the boot window before first activation, when
//!   frames are still genuinely transparent). Anything else blacks out the
//!   game underneath.
//! - Foreground changes while the overlay is open are IGNORED: Steam's own
//!   input window takes the foreground on activation, and treating that as
//!   an alt-tab caused a show/hide fight that locked the game out.
//! - Presenting pauses while hidden, so alt-tab never wedges a Fifo present
//!   against an occluded window.
//! - The swapchain is sized/synced against the overlay window's ACTUAL size
//!   (the window is born at Tauri's 800x600 default; comparing against the
//!   config alone left Steam rendering the whole overlay into a corner).
//! - Consecutive present failures kill the surface instead of looping black.
//! - If the surface offers no transparent composite-alpha mode we abort
//!   before presenting anything (a black sheet over the game is worse than
//!   no overlay). `WGPU_BACKEND=dx12|vulkan|gl` forces a backend for
//!   experiments without a rebuild.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, Runtime, Window};

use crate::{snapshot, Config};

/// True while the decoy surface is presenting (see
/// [`crate::surface_active`]).
pub(crate) static SURFACE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mirrors Steam's GameOverlayActivated state (written by
/// [`crate::on_overlay_activated`]). Focus management must not hide the
/// surface mid-overlay.
pub(crate) static STEAM_OVERLAY_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Label of the decoy window this plugin creates.
pub const OVERLAY_LABEL: &str = "steam-overlay-surface";

#[cfg(windows)]
#[link(name = "user32")]
extern "system" {
    fn GetForegroundWindow() -> isize;
    fn GetAncestor(hwnd: isize, flags: u32) -> isize;
    fn GetWindowLongPtrW(hwnd: isize, idx: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: isize, idx: i32, value: isize) -> isize;
}

#[cfg(windows)]
const GA_ROOT: u32 = 2;
#[cfg(windows)]
const GWL_EXSTYLE: i32 = -20;
/// Tool windows are skipped by alt-tab AND by capture pickers (OBS's window
/// list) — keeps the decoy from ever being matched as a capture source.
#[cfg(windows)]
const WS_EX_TOOLWINDOW: isize = 0x80;

/// Give up on the surface after this many consecutive failed frames
/// (~2s at vsync) — protects against a stuck opaque/black window.
const MAX_CONSECUTIVE_FRAME_FAILURES: u32 = 120;

/// Re-assert WS_EX_TOOLWINDOW on the decoy. tao rewrites the full extended
/// style from its cached flags on show()/set_ignore_cursor_events(), wiping
/// externally-set bits — so this must be re-applied, not set once.
#[cfg(windows)]
fn ensure_toolwindow<R: Runtime>(overlay: &Window<R>) {
    if let Ok(hwnd) = overlay.hwnd() {
        unsafe {
            let ex = GetWindowLongPtrW(hwnd.0 as isize, GWL_EXSTYLE);
            if ex & WS_EX_TOOLWINDOW == 0 {
                SetWindowLongPtrW(hwnd.0 as isize, GWL_EXSTYLE, ex | WS_EX_TOOLWINDOW);
            }
        }
    }
}

#[cfg(not(windows))]
fn ensure_toolwindow<R: Runtime>(_overlay: &Window<R>) {}

/// Create the overlay window and start the render thread. Never fatal —
/// on any failure the app simply runs without overlay support.
pub(crate) fn spawn<R: Runtime>(main: Window<R>, config: Config) {
    let app = main.app_handle().clone();

    let builder = match tauri::window::WindowBuilder::new(&app, OVERLAY_LABEL).parent(&main) {
        Ok(b) => b,
        Err(err) => {
            log::warn!("steam-overlay-surface: parent() failed ({err}); skipped");
            return;
        }
    };
    let overlay = match builder
        // Shows up in tooling that enumerates windows — default is "Tauri App".
        .title(&config.overlay_title)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .closable(false)
        .decorations(false)
        .shadow(false)
        .background_color(tauri::window::Color(0, 0, 0, 0))
        .focused(false)
        .skip_taskbar(true)
        // Hidden until the surface proves it can present transparently.
        .visible(false)
        .build()
    {
        Ok(w) => w,
        Err(err) => {
            log::warn!("steam-overlay-surface: window creation failed ({err}); skipped");
            return;
        }
    };
    let _ = overlay.set_ignore_cursor_events(true);
    // NO display affinity here, deliberately: WDA_EXCLUDEFROMCAPTURE renders
    // the decoy as an OPAQUE BLACK sheet (not removed) in the capture paths
    // OBS and screenshot tools use (DXGI duplication, WGC) — every capture
    // went black whenever the sheet was visible. Without it, captures show
    // game + overlay exactly like a native title.
    //
    // OBS re-matches sources by exe+class when a process restarts and can
    // latch onto the decoy. Tool windows are invisible to capture pickers, so
    // it can never be matched. tao rewrites the full exstyle from its cached
    // flags on show/style changes, so the render loop re-asserts this every
    // iteration (see ensure_toolwindow).
    ensure_toolwindow(&overlay);

    // Size/position BEFORE the swapchain exists — the window is born 800x600
    // and Vulkan clamps the swapchain extent to the real window size.
    if let (Ok(pos), Ok(size)) = (main.inner_position(), main.inner_size()) {
        let _ = overlay.set_size(size);
        let _ = overlay.set_position(pos);
    }

    // Focus management (runs on the main thread): hide the surface when the
    // game loses focus with the overlay closed; re-show it on focus gain only
    // while the overlay is open (Steam owns the sheet in that state).
    {
        let ov = overlay.clone();
        main.on_window_event(move |event| {
            if let tauri::WindowEvent::Focused(focused) = event {
                if *focused {
                    // Only re-show while the Steam overlay is actually open. Once
                    // Steam has activated a first time its hook keeps painting opaque
                    // frames into the swapchain, so a visible decoy with the overlay
                    // closed is a solid black sheet over the game.
                    if SURFACE_ACTIVE.load(Ordering::Relaxed)
                        && STEAM_OVERLAY_ACTIVE.load(Ordering::Relaxed)
                    {
                        let _ = ov.show();
                        // A watchdog hide leaves the sheet click-through; if it comes
                        // back while the overlay is open it must accept input again or
                        // clicks fall through to the invisible game.
                        let _ = ov.set_ignore_cursor_events(false);
                    }
                } else if !STEAM_OVERLAY_ACTIVE.load(Ordering::Relaxed) {
                    let _ = ov.hide();
                }
            }
        });
    }

    // Alt-tab detection. The decoy holds focus while the Steam overlay is
    // open, so the MAIN window never hears about a subsequent alt-tab — and
    // Focused events on the raw decoy window never fire at all (tao). So poll
    // the foreground window instead: whenever neither the game nor the decoy
    // is foreground, drop the sheet (hide + click-through) so other apps are
    // visible and clickable.
    #[cfg(windows)]
    {
        let ov = overlay.clone();
        let main_hwnd = main.hwnd().map(|h| h.0 as isize).unwrap_or(0);
        let decoy_hwnd = overlay.hwnd().map(|h| h.0 as isize).unwrap_or(0);
        let watchdog = std::thread::Builder::new()
            .name("steam-overlay-watchdog".into())
            .spawn(move || {
                let mut was_ours = true;
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let Ok(visible) = ov.is_visible() else {
                        return; // window destroyed — app is exiting
                    };
                    let fg_root = unsafe { GetAncestor(GetForegroundWindow(), GA_ROOT) };
                    let ours = fg_root == main_hwnd || fg_root == decoy_hwnd;
                    if ours != was_ours {
                        was_ours = ours;
                        let overlay_active = STEAM_OVERLAY_ACTIVE.load(Ordering::Relaxed);
                        // While the overlay is open, Steam's own input window takes the
                        // foreground — that is NOT an alt-tab, and hiding the sheet here
                        // caused a show/hide fight that locked the game out. The decoy
                        // is an owned window (floats above its owner only), so a real
                        // alt-tab simply covers it; no hide needed while active.
                        if !ours && visible && !overlay_active {
                            let _ = ov.set_ignore_cursor_events(true);
                            let _ = ov.hide();
                        }
                    }
                }
            });
        if let Err(err) = watchdog {
            log::warn!("steam-overlay-surface: watchdog thread failed to start ({err})");
        }
    }

    let spawned = std::thread::Builder::new()
        .name("steam-overlay-surface".into())
        .spawn(move || {
            if let Err(err) = run_surface(&app, &main, &overlay, &config) {
                SURFACE_ACTIVE.store(false, Ordering::Relaxed);
                let _ = overlay.hide();
                log::warn!("steam-overlay-surface: disabled — {err}");
            }
        });
    if let Err(err) = spawned {
        log::warn!("steam-overlay-surface: render thread failed to start ({err})");
    }
}

/// Init wgpu on the overlay window and present transparent frames until the
/// main window closes. Returns `Err` for failures (caller hides the window).
fn run_surface<R: Runtime>(
    app: &AppHandle<R>,
    main: &Window<R>,
    overlay: &Window<R>,
    config_in: &Config,
) -> Result<(), String> {
    // Instance::default() honors WGPU_BACKEND for no-rebuild experiments.
    let instance = wgpu::Instance::default();
    let surface = instance
        .create_surface(overlay.clone())
        .map_err(|e| format!("create_surface: {e}"))?;

    let adapter =
        tauri::async_runtime::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .ok_or("no compatible GPU adapter")?;
    let info = adapter.get_info();
    log::info!(
        "steam-overlay-surface: adapter '{}' backend {:?}",
        info.name,
        info.backend
    );

    let (device, queue) = tauri::async_runtime::block_on(
        adapter.request_device(&wgpu::DeviceDescriptor::default(), None),
    )
    .map_err(|e| format!("request_device: {e}"))?;

    let caps = surface.get_capabilities(&adapter);
    log::info!(
        "steam-overlay-surface: alpha modes {:?}, formats {:?}",
        caps.alpha_modes,
        caps.formats
    );

    let alpha_mode = caps
        .alpha_modes
        .iter()
        .copied()
        .find(|m| {
            matches!(
                m,
                wgpu::CompositeAlphaMode::PostMultiplied
                    | wgpu::CompositeAlphaMode::PreMultiplied
                    | wgpu::CompositeAlphaMode::Inherit
            )
        })
        .ok_or_else(|| {
            format!(
                "no transparent composite-alpha mode (got {:?}); try WGPU_BACKEND=dx12|vulkan|gl",
                caps.alpha_modes
            )
        })?;
    if alpha_mode == wgpu::CompositeAlphaMode::Inherit {
        log::warn!(
      "steam-overlay-surface: alpha mode Inherit — transparency is platform-defined, verify visually"
    );
    }

    // Configure against the overlay window's ACTUAL size, not the main
    // window's: if set_size hasn't landed yet the swapchain extent gets
    // clamped and Steam would render the overlay into a corner.
    let size = overlay
        .inner_size()
        .map_err(|e| format!("overlay inner_size: {e}"))?;
    let mut config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: *caps.formats.first().ok_or("no surface formats")?,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);
    log::info!(
        "steam-overlay-surface: presenting ({alpha_mode:?}, {}x{})",
        config.width,
        config.height
    );

    // Snapshot backdrop: while the overlay is open, paint the frozen game
    // frame (captured at overlay-open) instead of clearing transparent. Steam
    // then dims a "game frame" exactly like a native game, instead of
    // compositing onto black. These frames are opaque BY DESIGN — per-pixel
    // alpha stops mattering in the only state where the sheet is on screen.
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("overlay-backdrop"),
        source: wgpu::ShaderSource::Wgsl(
            r#"
      @group(0) @binding(0) var snap_tex: texture_2d<f32>;
      @group(0) @binding(1) var snap_sampler: sampler;

      struct VOut {
        @builtin(position) pos: vec4<f32>,
        @location(0) uv: vec2<f32>,
      };

      @vertex
      fn vs_main(@builtin(vertex_index) i: u32) -> VOut {
        // Fullscreen triangle.
        let corner = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
        var out: VOut;
        out.pos = vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
        out.uv = vec2<f32>(corner.x, 1.0 - corner.y);
        return out;
      }

      @fragment
      fn fs_main(in: VOut) -> @location(0) vec4<f32> {
        // Alpha forced to 1: GDI leaves garbage in the alpha channel, and
        // the backdrop must be opaque regardless.
        return vec4<f32>(textureSample(snap_tex, snap_sampler, in.uv).rgb, 1.0);
      }
      "#
            .into(),
        ),
    });
    let backdrop_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("overlay-backdrop"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let backdrop_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("overlay-backdrop"),
        bind_group_layouts: &[&backdrop_bgl],
        push_constant_ranges: &[],
    });
    let backdrop_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("overlay-backdrop"),
        layout: Some(&backdrop_pl),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_main",
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format: config.format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
    });
    let backdrop_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("overlay-backdrop"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    // Screen pixels are sRGB-encoded; match the surface's sRGB-ness so
    // sampling/writing round-trips the colors unchanged.
    let backdrop_format = if config.format.is_srgb() {
        wgpu::TextureFormat::Bgra8UnormSrgb
    } else {
        wgpu::TextureFormat::Bgra8Unorm
    };
    let mut backdrop: Option<wgpu::BindGroup> = None;

    SURFACE_ACTIVE.store(true, Ordering::Relaxed);
    let _ = overlay.show();

    let main_label = config_in.main_window_label.as_str();
    let mut last_pos = PhysicalPosition::new(i32::MIN, i32::MIN);
    let mut consecutive_failures: u32 = 0;
    let mut was_visible = true;
    loop {
        // Main window gone → close the overlay too, or the app never exits
        // (Tauri quits when the *last* window closes).
        if app.get_window(main_label).is_none() {
            SURFACE_ACTIVE.store(false, Ordering::Relaxed);
            let _ = overlay.close();
            return Ok(());
        }

        // tao may have rewritten the exstyle since last iteration (any
        // show/hide/cursor-events call) — keep the decoy out of capture pickers.
        ensure_toolwindow(overlay);

        // Hidden (game unfocused / overlay closed): don't present — but keep
        // geometry synced so the first frame after Shift+Tab isn't stretched
        // from a stale size (the window can be moved/resized while hidden).
        if !overlay.is_visible().unwrap_or(false) {
            was_visible = false;
            if let (Ok(pos), Ok(size)) = (main.inner_position(), main.inner_size()) {
                if size.width > 0 && size.height > 0 {
                    if overlay.inner_size().map(|s| s != size).unwrap_or(false) {
                        let _ = overlay.set_size(size);
                    }
                    let _ = overlay.set_position(pos);
                }
            }
            std::thread::sleep(Duration::from_millis(150));
            continue;
        }
        if !was_visible {
            was_visible = true;
            // Fresh swapchain after hide→show: Windows can keep the last
            // composited frame (e.g. Steam's opaque overlay sheet) glued to a
            // re-shown window, leaving a stale black screen our clears never
            // replace. Reconfiguring drops the old chain.
            surface.configure(&device, &config);
            log::info!("steam-overlay-surface: reshown, swapchain recreated");
        }

        let (Ok(main_pos), Ok(main_size)) = (main.inner_position(), main.inner_size()) else {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        };
        if main_size.width == 0 || main_size.height == 0 {
            // Minimized: don't present into a zero-sized surface.
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        if main_pos != last_pos {
            last_pos = main_pos;
            let _ = overlay.set_position(main_pos);
        }

        // Geometry sync against the overlay's REAL size. set_size is async-ish
        // (proxied to the main thread), so re-issue until it lands, and only
        // reconfigure the swapchain to sizes the window actually has.
        let actual = overlay
            .inner_size()
            .unwrap_or(PhysicalSize::new(config.width, config.height));
        if actual != main_size {
            let _ = overlay.set_size(main_size);
        }
        if actual.width > 0
            && actual.height > 0
            && (actual.width != config.width || actual.height != config.height)
        {
            config.width = actual.width;
            config.height = actual.height;
            surface.configure(&device, &config);
            log::info!(
                "steam-overlay-surface: resized to {}x{}",
                config.width,
                config.height
            );
        }

        // Consume snapshot handoffs from the Steam callback: Some = upload a
        // fresh backdrop texture, None = overlay closed, drop it.
        if let Some(change) = snapshot::take_pending() {
            match change {
                Some(snap) => {
                    let tight_bpr = snap.width * 4;
                    // wgpu requires 256-byte-aligned rows; repack when the width
                    // doesn't already comply (windowed sizes usually don't).
                    let padded_bpr = (tight_bpr + 255) & !255;
                    let pixels = if padded_bpr == tight_bpr {
                        snap.bgra
                    } else {
                        let mut padded = vec![0u8; (padded_bpr as usize) * (snap.height as usize)];
                        for row in 0..snap.height as usize {
                            let src = row * tight_bpr as usize;
                            let dst = row * padded_bpr as usize;
                            padded[dst..dst + tight_bpr as usize]
                                .copy_from_slice(&snap.bgra[src..src + tight_bpr as usize]);
                        }
                        padded
                    };
                    let tex = device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("overlay-backdrop"),
                        size: wgpu::Extent3d {
                            width: snap.width,
                            height: snap.height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: backdrop_format,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    queue.write_texture(
                        wgpu::ImageCopyTexture {
                            texture: &tex,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &pixels,
                        wgpu::ImageDataLayout {
                            offset: 0,
                            bytes_per_row: Some(padded_bpr),
                            rows_per_image: None,
                        },
                        wgpu::Extent3d {
                            width: snap.width,
                            height: snap.height,
                            depth_or_array_layers: 1,
                        },
                    );
                    let tex_view = tex.create_view(&wgpu::TextureViewDescriptor::default());
                    backdrop = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("overlay-backdrop"),
                        layout: &backdrop_bgl,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&tex_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&backdrop_sampler),
                            },
                        ],
                    }));
                    log::info!(
                        "steam-overlay-surface: backdrop snapshot {}x{}",
                        snap.width,
                        snap.height
                    );
                }
                None => backdrop = None,
            }
        }

        match surface.get_current_texture() {
            Ok(frame) => {
                consecutive_failures = 0;
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("overlay-frame"),
                });
                // Transparent clear is the product when idle (Steam draws into it
                // at Present); with the overlay open and a snapshot captured, paint
                // the frozen game frame so Steam dims that instead of black.
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("overlay-frame"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    });
                    if STEAM_OVERLAY_ACTIVE.load(Ordering::Relaxed) {
                        if let Some(bg) = &backdrop {
                            pass.set_pipeline(&backdrop_pipeline);
                            pass.set_bind_group(0, bg, &[]);
                            pass.draw(0..3, 0..1);
                        }
                    }
                }
                queue.submit(std::iter::once(encoder.finish()));
                // Fifo present blocks until vsync — this is the loop's pacing. The
                // foreground watchdog hides the window before occlusion can wedge
                // the present.
                frame.present();
            }
            Err(err) => {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FRAME_FAILURES {
                    return Err(format!(
            "{consecutive_failures} consecutive frame failures (last: {err}); surface killed as black-screen guard"
          ));
                }
                match err {
                    wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                        surface.configure(&device, &config);
                    }
                    _ => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }
    }
}
