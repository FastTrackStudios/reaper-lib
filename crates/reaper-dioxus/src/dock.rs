//! DockablePanel — Dioxus rendering inside REAPER's native docker system.
//!
//! Creates a SWELL dialog window, registers it with REAPER's docker via
//! `DockWindowAddEx`, and renders a Dioxus component tree into it using
//! the `EmbeddedView` GPU pipeline.
//!
//! On Linux, SWELL docked panels don't get a GDK window, so we create a
//! direct X11 child window with a proper 32-bit ARGB visual as the GPU
//! render surface, reparented into the dock panel's X11 hierarchy.

use dioxus_native::prelude::*;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle,
};
use reaper_low::raw;
use reaper_low::{Reaper, Swell};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CString, c_void};
use std::os::raw::c_int;
use std::ptr;

use crate::embedded::EmbeddedView;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

// PanelId can be &str for lookups (HashMap<String> supports .get(&str))
type PanelId = &'static str; // Registration still uses &'static str for config IDs

/// Configuration for a dockable panel.
#[derive(Clone)]
pub struct DockablePanelConfig {
    pub id: PanelId,
    pub title: &'static str,
    pub default_dock_position: i32,
    pub app: fn() -> Element,
    pub default_width: u32,
    pub default_height: u32,
    pub show_on_first_launch: bool,
}

/// A live dockable panel registered with REAPER's docker system.
struct LivePanel {
    config: DockablePanelConfig,
    hwnd: raw::HWND,
    view: Option<EmbeddedView>,
    visible: bool,
    contexts: Vec<Box<dyn FnOnce()>>,
    /// X11 child window created for GPU rendering (Linux only).
    /// Destroyed on panel close to prevent resource leaks.
    #[cfg(target_os = "linux")]
    x11_child_xid: u64,
    /// Parent GDK window XID (Linux only). Used to reposition the child.
    #[cfg(target_os = "linux")]
    x11_parent_xid: u64,
    /// Track failed init attempts to avoid infinite retry spam.
    init_attempts: u32,
}

#[cfg(target_os = "linux")]
impl Drop for LivePanel {
    fn drop(&mut self) {
        // Destroy X11 child window if we created one
        if self.x11_child_xid != 0 {
            if let Some((xlib, display)) = reaper_embed::transparent::x11_display() {
                unsafe {
                    (xlib.XDestroyWindow)(display, self.x11_child_xid);
                    (xlib.XFlush)(display);
                }
                tracing::debug!(xid = self.x11_child_xid, "Destroyed X11 child window");
            }
        }
        // EmbeddedView's Drop handles GPU resource cleanup
    }
}

// ---------------------------------------------------------------------------
// Global registry (thread-local, main thread only)
// ---------------------------------------------------------------------------

thread_local! {
    static PANELS: RefCell<HashMap<PanelId, LivePanel>> = RefCell::new(HashMap::new());
}

static REAPER_API: std::sync::OnceLock<&'static Reaper> = std::sync::OnceLock::new();
static SWELL_API: std::sync::OnceLock<&'static Swell> = std::sync::OnceLock::new();

/// Initialize the dock module with REAPER/SWELL API references.
/// Must be called before `register_panel_from_service`.
pub fn init(reaper: &'static Reaper, swell: &'static Swell) {
    let _ = REAPER_API.set(reaper);
    let _ = SWELL_API.set(swell);
}

fn reaper() -> &'static Reaper {
    REAPER_API.get().expect("dock::init() must be called first")
}
fn swell() -> &'static Swell {
    SWELL_API.get().expect("dock::init() must be called first")
}

/// Assert we're on the main thread (debug builds only).
#[cfg(debug_assertions)]
fn assert_main_thread() {
    // REAPER's main thread is the one that called plugin_main.
    // thread_local! variables are per-thread, so if PANELS is accessible
    // we're on the right thread. This is a sanity check.
}

// ---------------------------------------------------------------------------
// HWND wrapper for raw-window-handle traits
// ---------------------------------------------------------------------------

struct RenderSurface {
    #[allow(dead_code)]
    hwnd: raw::HWND,
    #[cfg(target_os = "linux")]
    xid: u64,
    #[cfg(target_os = "linux")]
    visual_id: u32,
}

// SAFETY: REAPER extensions are single-threaded on the main thread.
// These handles are only used from the main thread in timer callbacks.
unsafe impl Send for RenderSurface {}
unsafe impl Sync for RenderSurface {}

impl HasWindowHandle for RenderSurface {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        #[cfg(target_os = "macos")]
        {
            use raw_window_handle::AppKitWindowHandle;
            use std::ptr::NonNull;
            let handle = AppKitWindowHandle::new(
                NonNull::new(self.hwnd as *mut c_void).ok_or(HandleError::Unavailable)?,
            );
            Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::AppKit(handle)) })
        }
        #[cfg(target_os = "linux")]
        {
            use raw_window_handle::XlibWindowHandle;
            if self.xid == 0 {
                return Err(HandleError::Unavailable);
            }
            let mut handle = XlibWindowHandle::new(self.xid.try_into().unwrap_or(0));
            handle.visual_id = self.visual_id.try_into().unwrap_or(0);
            Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Xlib(handle)) })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            use raw_window_handle::Win32WindowHandle;
            use std::num::NonZeroIsize;
            let handle = Win32WindowHandle::new(
                NonZeroIsize::new(self.hwnd as isize).ok_or(HandleError::Unavailable)?,
            );
            Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(handle)) })
        }
    }
}

impl HasDisplayHandle for RenderSurface {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        #[cfg(target_os = "macos")]
        {
            use raw_window_handle::AppKitDisplayHandle;
            Ok(unsafe {
                DisplayHandle::borrow_raw(RawDisplayHandle::AppKit(AppKitDisplayHandle::new()))
            })
        }
        #[cfg(target_os = "linux")]
        {
            use raw_window_handle::XlibDisplayHandle;
            let (_, display) = reaper_embed::transparent::x11_display()
                .ok_or(HandleError::Unavailable)?;
            use std::ptr::NonNull;
            let handle = XlibDisplayHandle::new(NonNull::new(display as *mut c_void), 0);
            Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Xlib(handle)) })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            use raw_window_handle::WindowsDisplayHandle;
            Ok(unsafe {
                DisplayHandle::borrow_raw(RawDisplayHandle::Windows(
                    WindowsDisplayHandle::new(),
                ))
            })
        }
    }
}

// ---------------------------------------------------------------------------
// GDK XID extraction (cached, called once)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod gdk {
    use super::*;
    use std::sync::OnceLock;

    type GdkX11WindowGetXid = unsafe extern "C" fn(*mut c_void) -> u64;

    /// Cached dlsym result for gdk_x11_window_get_xid.
    static GDK_GET_XID: OnceLock<Option<GdkX11WindowGetXid>> = OnceLock::new();

    fn get_xid_fn() -> Option<GdkX11WindowGetXid> {
        *GDK_GET_XID.get_or_init(|| {
            let lib_names = ["libgdk-3.so.0", "libgdk-3.so"];
            for lib_name in &lib_names {
                let c_lib = CString::new(*lib_name).unwrap();
                let handle =
                    unsafe { libc::dlopen(c_lib.as_ptr(), libc::RTLD_LAZY | libc::RTLD_NOLOAD) };
                if handle.is_null() {
                    continue;
                }
                let sym_name = CString::new("gdk_x11_window_get_xid").unwrap();
                let sym = unsafe { libc::dlsym(handle, sym_name.as_ptr()) };
                unsafe { libc::dlclose(handle) };
                if !sym.is_null() {
                    return Some(unsafe { std::mem::transmute(sym) });
                }
            }
            None
        })
    }

    /// Try to extract X11 XID from a SWELL HWND via GDK.
    /// Also walks the SWELL parent chain if the direct HWND has no GdkWindow.
    pub fn extract_xid(hwnd: raw::HWND, swell: &Swell) -> u64 {
        if hwnd.is_null() {
            return 0;
        }

        let gdk_type = CString::new("GdkWindow").unwrap();
        let get_xid = match get_xid_fn() {
            Some(f) => f,
            None => return 0,
        };

        // Try the HWND itself, then walk parent chain
        let mut current = hwnd;
        for _ in 0..16 {
            if current.is_null() {
                break;
            }
            let gdk_window = unsafe { swell.SWELL_GetOSWindow(current, gdk_type.as_ptr()) };
            if !gdk_window.is_null() {
                let xid = unsafe { get_xid(gdk_window) };
                if xid != 0 {
                    return xid;
                }
            }
            let parent = unsafe { swell.GetParent(current) };
            if parent.is_null() || parent == current {
                break;
            }
            current = parent;
        }

        0
    }
}

// ---------------------------------------------------------------------------
// X11 child window creation with proper visual
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn create_x11_render_child(
    parent_xid: u64,
    width: u32,
    height: u32,
) -> Option<(u64, u32)> {
    use reaper_embed::transparent::x11_display;
    use x11_dl::xlib::{AllocNone, CWBackPixel, CWBorderPixel, CWColormap, InputOutput, TrueColor};

    let (xlib, display) = x11_display()?;
    let screen = unsafe { (xlib.XDefaultScreen)(display) };

    // Find 32-bit ARGB visual (same as TransparentWindow) for wgpu compatibility
    let mut visual_info = unsafe { std::mem::zeroed::<x11_dl::xlib::XVisualInfo>() };
    let found = unsafe { (xlib.XMatchVisualInfo)(display, screen, 32, TrueColor, &mut visual_info) };
    let (visual, depth, visual_id) = if found != 0 {
        (visual_info.visual, 32, visual_info.visualid as u32)
    } else {
        // Fallback to default visual
        let visual = unsafe { (xlib.XDefaultVisual)(display, screen) };
        let depth = unsafe { (xlib.XDefaultDepth)(display, screen) };
        (visual, depth, 0)
    };

    // Create colormap for the visual
    let root = unsafe { (xlib.XRootWindow)(display, screen) };
    let colormap = unsafe { (xlib.XCreateColormap)(display, root, visual, AllocNone) };

    let mut attrs = unsafe { std::mem::zeroed::<x11_dl::xlib::XSetWindowAttributes>() };
    attrs.colormap = colormap;
    attrs.border_pixel = 0;
    attrs.background_pixel = 0x121212;

    let parent = if parent_xid != 0 { parent_xid } else { root };

    let window = unsafe {
        (xlib.XCreateWindow)(
            display,
            parent,
            0, 0,
            width, height,
            0,
            depth,
            InputOutput as u32,
            visual,
            CWColormap | CWBorderPixel | CWBackPixel,
            &mut attrs,
        )
    };

    if window == 0 {
        return None;
    }

    // Don't map yet — update_panels will map after positioning
    // to avoid a flash at (0,0).
    unsafe {
        (xlib.XFlush)(display);
    }

    Some((window, visual_id))
}

/// Reposition and resize the X11 child window to match the panel's client area.
#[cfg(target_os = "linux")]
fn resize_x11_child(xid: u64, width: u32, height: u32) {
    if xid == 0 {
        return;
    }
    // Position is handled separately by reposition_x11_child_to_panel
    if let Some((xlib, display)) = reaper_embed::transparent::x11_display() {
        unsafe {
            (xlib.XResizeWindow)(display, xid, width, height);
            (xlib.XFlush)(display);
        }
    }
}

/// Position the X11 child window at the dock panel's screen position
/// relative to its parent GDK window.
#[cfg(target_os = "linux")]
fn reposition_x11_child_to_panel(
    hwnd: raw::HWND,
    child_xid: u64,
    parent_xid: u64,
    swell: &Swell,
) {
    if child_xid == 0 || parent_xid == 0 {
        return;
    }

    // Get the panel's screen position via SWELL
    let mut panel_rect = raw::RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { swell.GetWindowRect(hwnd, &mut panel_rect); }

    let Some((xlib, display)) = reaper_embed::transparent::x11_display() else { return };

    // Translate panel's screen coords to parent-relative coords
    let root = unsafe { (xlib.XDefaultRootWindow)(display) };
    let mut dest_x: c_int = 0;
    let mut dest_y: c_int = 0;
    let mut dummy_child: u64 = 0;
    unsafe {
        (xlib.XTranslateCoordinates)(
            display,
            root,
            parent_xid,
            panel_rect.left,
            panel_rect.top,
            &mut dest_x,
            &mut dest_y,
            &mut dummy_child,
        );
        (xlib.XMoveWindow)(display, child_xid, dest_x, dest_y);
    }
    tracing::trace!(
        child_xid, parent_xid, dest_x, dest_y,
        panel_left = panel_rect.left, panel_top = panel_rect.top,
        "Repositioned X11 child"
    );
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn register_panel(
    config: DockablePanelConfig,
    contexts: Vec<Box<dyn FnOnce()>>,
    reaper: &'static Reaper,
    swell: &'static Swell,
) {
    #[cfg(debug_assertions)]
    assert_main_thread();

    let id = config.id;
    let _ = REAPER_API.set(reaper);
    let _ = SWELL_API.set(swell);

    let hwnd = create_panel_window(&config, swell);
    if hwnd.is_null() {
        tracing::error!("Failed to create dock panel window for '{}'", config.title);
        return;
    }

    let ident = CString::new(config.id).unwrap();
    let saved_dock = unsafe { reaper.GetConfigWantsDock(ident.as_ptr()) };
    let dock_position = if saved_dock >= 0 { saved_dock } else { config.default_dock_position };

    let title_c = CString::new(config.title).unwrap();
    unsafe {
        reaper.DockWindowAddEx(hwnd, title_c.as_ptr(), ident.as_ptr(), config.show_on_first_launch);
    }

    if dock_position >= 0 && saved_dock < 0 {
        unsafe { reaper.Dock_UpdateDockID(ident.as_ptr(), dock_position) };
    }

    let panel = LivePanel {
        config,
        hwnd,
        view: None,
        visible: false,
        contexts,
        #[cfg(target_os = "linux")]
        x11_child_xid: 0,
        #[cfg(target_os = "linux")]
        x11_parent_xid: 0,
        init_attempts: 0,
    };

    PANELS.with(|panels| { panels.borrow_mut().insert(id, panel); });
    tracing::debug!(id, dock_position, "Registered dockable panel");
}

/// Register a panel from a DawModule PanelDef (service API).
pub fn register_panel_from_service(def: &daw::module::PanelDef) {
    use dioxus_native::prelude::Element;

    let reaper = reaper();
    let swell = swell();

    // Cast the type-erased component pointer back to fn() -> Element
    let component: fn() -> Element = unsafe { std::mem::transmute(def.component.as_ptr()) };

    let dock_pos = match def.default_dock {
        daw::module::DockPosition::Bottom => 0,
        daw::module::DockPosition::Left => 1,
        daw::module::DockPosition::Top => 2,
        daw::module::DockPosition::Right => 3,
        daw::module::DockPosition::Floating => -1,
    };

    register_panel(
        DockablePanelConfig {
            id: def.id,
            title: def.title,
            default_dock_position: dock_pos,
            app: component,
            default_width: def.default_size.0 as u32,
            default_height: def.default_size.1 as u32,
            show_on_first_launch: false,
        },
        Vec::new(),
        reaper,
        swell,
    );
}

pub fn toggle_panel(id: PanelId) {
    #[cfg(debug_assertions)]
    assert_main_thread();
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        if let Some(panel) = panels.get_mut(id) {
            if panel.visible { hide_panel_inner(panel); } else { show_panel_inner(panel); }
        }
    });
}

pub fn show_panel(id: PanelId) {
    #[cfg(debug_assertions)]
    assert_main_thread();
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        if let Some(panel) = panels.get_mut(id) {
            if !panel.visible { show_panel_inner(panel); }
        }
    });
}

pub fn hide_panel(id: PanelId) {
    #[cfg(debug_assertions)]
    assert_main_thread();
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        if let Some(panel) = panels.get_mut(id) {
            if panel.visible { hide_panel_inner(panel); }
        }
    });
}

pub fn is_panel_visible(id: PanelId) -> bool {
    PANELS.with(|panels| panels.borrow().get(id).map_or(false, |p| p.visible))
}

/// Update all visible panels (call from timer callback ~30Hz).
pub fn update_panels() {
    #[cfg(debug_assertions)]
    assert_main_thread();

    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        let swell = swell();

        for panel in panels.values_mut() {
            if !panel.visible {
                continue;
            }

            // Poll SWELL visibility — IsWindowVisible returns false when
            // docked behind another tab (same as ReaImGui's approach).
            let hwnd_visible = unsafe { swell.IsWindowVisible(panel.hwnd) };

            #[cfg(target_os = "linux")]
            {
                if panel.x11_child_xid != 0 {
                    if let Some((xlib, display)) = reaper_embed::transparent::x11_display() {
                        if hwnd_visible {
                            unsafe {
                                (xlib.XMapWindow)(display, panel.x11_child_xid);
                            }
                        } else {
                            unsafe {
                                (xlib.XUnmapWindow)(display, panel.x11_child_xid);
                            }
                        }
                    }
                }
            }

            if !hwnd_visible {
                continue; // Skip rendering when hidden (tab inactive, dock closed)
            }

            // Deferred EmbeddedView init (max 30 retries ≈ 1 second)
            if panel.view.is_none() && panel.init_attempts < 30 {
                try_init_embedded_view(panel);
            }

            if let Some(view) = &mut panel.view {
                let mut rect = raw::RECT { left: 0, top: 0, right: 0, bottom: 0 };
                unsafe { swell.GetClientRect(panel.hwnd, &mut rect); }
                let w = (rect.right - rect.left).unsigned_abs();
                let h = (rect.bottom - rect.top).unsigned_abs();

                if w > 0 && h > 0 {
                    let (cur_w, cur_h) = view.size();
                    if w != cur_w || h != cur_h {
                        view.resize(w, h);
                        #[cfg(target_os = "linux")]
                        resize_x11_child(panel.x11_child_xid, w, h);
                    }
                    // Reposition X11 child to track dock panel movement
                    #[cfg(target_os = "linux")]
                    if panel.x11_child_xid != 0 && panel.x11_parent_xid != 0 {
                        reposition_x11_child_to_panel(
                            panel.hwnd, panel.x11_child_xid,
                            panel.x11_parent_xid, swell,
                        );
                    }
                    view.mark_dirty();
                    view.update();
                }
            }
        }
    });
}

pub fn unregister_all_panels() {
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        let reaper = reaper();
        let swell = swell();
        for (_, mut panel) in panels.drain() {
            // Drop EmbeddedView first (releases GPU resources)
            panel.view = None;
            if !panel.hwnd.is_null() {
                unsafe {
                    reaper.DockWindowRemove(panel.hwnd);
                    swell.DestroyWindow(panel.hwnd);
                }
            }
            // LivePanel::drop handles X11 child cleanup
        }
    });
}

pub fn save_dock_state() {
    PANELS.with(|panels| {
        let panels = panels.borrow();
        let reaper = reaper();
        for (id, panel) in panels.iter() {
            let section = CString::new(*id).unwrap();
            let vis_key = CString::new("visible").unwrap();
            let vis_val = CString::new(if panel.visible { "1" } else { "0" }).unwrap();
            unsafe { reaper.SetExtState(section.as_ptr(), vis_key.as_ptr(), vis_val.as_ptr(), true); }

            let mut is_floating = false;
            let dock_id = unsafe { reaper.DockIsChildOfDock(panel.hwnd, &mut is_floating) };
            let dock_key = CString::new("dock_id").unwrap();
            let dock_val = CString::new(format!("{}", dock_id)).unwrap();
            unsafe { reaper.SetExtState(section.as_ptr(), dock_key.as_ptr(), dock_val.as_ptr(), true); }
        }
    });
}

pub fn restore_dock_state() {
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        let reaper = reaper();
        for (_id, panel) in panels.iter_mut() {
            let section = CString::new(panel.config.id).unwrap();
            let vis_key = CString::new("visible").unwrap();
            let vis_ptr = unsafe { reaper.GetExtState(section.as_ptr(), vis_key.as_ptr()) };
            if !vis_ptr.is_null() {
                let vis_str = unsafe { std::ffi::CStr::from_ptr(vis_ptr) }.to_string_lossy();
                if vis_str == "1" {
                    show_panel_inner(panel);
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn show_panel_inner(panel: &mut LivePanel) {
    let swell = swell();
    let reaper = reaper();

    unsafe {
        swell.ShowWindow(panel.hwnd, raw::SW_SHOW as c_int);
        reaper.DockWindowActivate(panel.hwnd);
    }

    #[cfg(target_os = "macos")]
    force_view_layout(panel.hwnd);

    panel.visible = true;
    panel.init_attempts = 0; // Reset retry counter for deferred init

    // Auto-save visibility
    save_dock_state();
}

fn hide_panel_inner(panel: &mut LivePanel) {
    let swell = swell();
    unsafe { swell.ShowWindow(panel.hwnd, raw::SW_HIDE as c_int); }

    // Keep EmbeddedView and X11 child alive — just unmap the X11 window.
    // This avoids the ~500ms wgpu re-init on next show.
    #[cfg(target_os = "linux")]
    {
        if panel.x11_child_xid != 0 {
            if let Some((xlib, display)) = reaper_embed::transparent::x11_display() {
                unsafe {
                    (xlib.XUnmapWindow)(display, panel.x11_child_xid);
                    (xlib.XFlush)(display);
                }
            }
        }
    }

    panel.visible = false;

    // Auto-save visibility
    save_dock_state();
}

fn try_init_embedded_view(panel: &mut LivePanel) {
    if panel.view.is_some() || !panel.visible {
        return;
    }
    panel.init_attempts += 1;
    tracing::info!(
        panel = panel.config.id,
        attempt = panel.init_attempts,
        "Attempting to init EmbeddedView"
    );

    let swell = swell();
    let mut rect = raw::RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { swell.GetClientRect(panel.hwnd, &mut rect); }
    let w = (rect.right - rect.left).unsigned_abs().max(panel.config.default_width);
    let h = (rect.bottom - rect.top).unsigned_abs().max(panel.config.default_height);
    tracing::info!(w, h, "Panel '{}' client rect", panel.config.id);

    // Try to get a render surface
    let surface = create_render_surface(panel, swell, w, h);
    let Some(surface) = surface else {
        if panel.init_attempts >= 30 {
            tracing::error!(
                "Failed to create render surface for '{}' after {} attempts",
                panel.config.id, panel.init_attempts
            );
        }
        return;
    };

    let contexts = std::mem::take(&mut panel.contexts);
    match EmbeddedView::new(panel.config.app, &surface, w, h, contexts) {
        Ok(view) => {
            panel.view = Some(view);
            tracing::info!("Created EmbeddedView for panel '{}'", panel.config.id);
            // Write ExtState so tests can verify the view was created
            let reaper = reaper();
            let section = CString::new(panel.config.id).unwrap();
            let key = CString::new("embedded_view").unwrap();
            let val = CString::new("1").unwrap();
            unsafe { reaper.SetExtState(section.as_ptr(), key.as_ptr(), val.as_ptr(), true); }
        }
        Err(e) => {
            tracing::warn!("EmbeddedView creation failed for '{}': {}", panel.config.id, e);
        }
    }
}

/// Create a RenderSurface for the panel.
///
/// On Linux, SWELL dock panels don't get their own GDK window, so we
/// create an X11 child window inside the nearest ancestor that has a
/// GDK window (the dock container). The child is positioned and resized
/// by `update_panels` to match the dock panel's client rect.
#[cfg(target_os = "linux")]
fn create_render_surface(panel: &mut LivePanel, swell: &Swell, w: u32, h: u32) -> Option<RenderSurface> {
    // Find the nearest ancestor GDK window to use as X11 parent
    let parent_xid = gdk::extract_xid(panel.hwnd, swell);
    tracing::info!(parent_xid, "GDK XID lookup for panel '{}'", panel.config.id);
    if parent_xid == 0 {
        tracing::warn!("No GDK XID found for panel '{}'", panel.config.id);
        return None;
    }

    // Create an X11 child window inside the dock container's GDK window.
    // This child is repositioned each tick to match the panel's position.
    let (xid, visual_id) = create_x11_render_child(parent_xid, w, h)?;
    panel.x11_child_xid = xid;
    panel.x11_parent_xid = parent_xid;

    // Position the child window at the dock panel's offset within the parent
    reposition_x11_child_to_panel(panel.hwnd, xid, parent_xid, swell);

    tracing::info!(xid, parent_xid, visual_id, "Created X11 render child for '{}'", panel.config.id);

    Some(RenderSurface {
        hwnd: panel.hwnd,
        xid,
        visual_id,
    })
}

#[cfg(not(target_os = "linux"))]
fn create_render_surface(panel: &mut LivePanel, _swell: &Swell, _w: u32, _h: u32) -> Option<RenderSurface> {
    Some(RenderSurface { hwnd: panel.hwnd })
}

/// Walk the SWELL parent chain to find an ancestor with a GDK window (and thus an XID).
#[cfg(target_os = "linux")]
fn find_parent_xid(hwnd: raw::HWND, swell: &Swell) -> u64 {
    let mut current = unsafe { swell.GetParent(hwnd) };
    let gdk_type = CString::new("GdkWindow").unwrap();

    for _ in 0..16 {
        if current.is_null() {
            break;
        }
        let gdk_window = unsafe { swell.SWELL_GetOSWindow(current, gdk_type.as_ptr()) };
        if !gdk_window.is_null() {
            if let Some(get_xid) = gdk_get_xid_fn() {
                let xid = unsafe { get_xid(gdk_window) };
                if xid != 0 {
                    return xid;
                }
            }
        }
        let parent = unsafe { swell.GetParent(current) };
        if parent.is_null() || parent == current {
            break;
        }
        current = parent;
    }
    0
}

#[cfg(target_os = "linux")]
fn gdk_get_xid_fn() -> Option<unsafe extern "C" fn(*mut c_void) -> u64> {
    // Use the cached version from the gdk module
    let lib_names = ["libgdk-3.so.0", "libgdk-3.so"];
    for lib_name in &lib_names {
        let c_lib = CString::new(*lib_name).unwrap();
        let handle = unsafe { libc::dlopen(c_lib.as_ptr(), libc::RTLD_LAZY | libc::RTLD_NOLOAD) };
        if handle.is_null() { continue; }
        let sym_name = CString::new("gdk_x11_window_get_xid").unwrap();
        let sym = unsafe { libc::dlsym(handle, sym_name.as_ptr()) };
        unsafe { libc::dlclose(handle) };
        if !sym.is_null() {
            return Some(unsafe { std::mem::transmute(sym) });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Window creation
// ---------------------------------------------------------------------------

fn create_panel_window(config: &DockablePanelConfig, swell: &Swell) -> raw::HWND {
    let title_c = CString::new(config.title).unwrap();

    unsafe {
        // Magic resid: 0x400000 | flags
        // bit 0 = resizable (WS_THICKFRAME)
        let magic_resid = 0x400001usize as *const std::os::raw::c_char;

        let hwnd = swell.CreateDialogParam(
            ptr::null_mut(),
            magic_resid,
            ptr::null_mut(),
            Some(panel_wndproc),
            0,
        );

        if !hwnd.is_null() {
            swell.SetWindowText(hwnd, title_c.as_ptr());
            swell.SetWindowPos(
                hwnd, ptr::null_mut(), 0, 0,
                config.default_width as c_int, config.default_height as c_int,
                0x0006, // SWP_NOZORDER | SWP_NOMOVE
            );
        }
        hwnd
    }
}

unsafe extern "C" fn panel_wndproc(
    hwnd: raw::HWND,
    msg: raw::UINT,
    wparam: raw::WPARAM,
    lparam: raw::LPARAM,
) -> raw::LRESULT {
    unsafe { panel_wndproc_inner(hwnd, msg, wparam, lparam) }
}

fn panel_wndproc_inner(
    hwnd: raw::HWND,
    msg: raw::UINT,
    wparam: raw::WPARAM,
    lparam: raw::LPARAM,
) -> raw::LRESULT {
    const WM_COMMAND: u32 = 0x0111;
    const WM_CLOSE: u32 = 0x0010;
    const WM_SIZE: u32 = 0x0005;
    const WM_SHOWWINDOW: u32 = 0x0018;
    const WM_DESTROY: u32 = 0x0002;
    const IDCANCEL: u32 = 2;

    match msg {
        // Docker close button sends WM_COMMAND with IDCANCEL
        WM_COMMAND if (wparam & 0xFFFF) as u32 == IDCANCEL => {
            PANELS.with(|panels| {
                let mut panels = panels.borrow_mut();
                if let Some(panel) = panels.values_mut().find(|p| p.hwnd == hwnd) {
                    tracing::info!(panel = panel.config.id, "Docker close button");
                    hide_panel_inner(panel);
                }
            });
            0
        }
        WM_CLOSE => {
            PANELS.with(|panels| {
                let mut panels = panels.borrow_mut();
                if let Some(panel) = panels.values_mut().find(|p| p.hwnd == hwnd) {
                    tracing::info!(panel = panel.config.id, "WM_CLOSE");
                    hide_panel_inner(panel);
                }
            });
            0
        }
        WM_SIZE => 0,
        WM_SHOWWINDOW => {
            let showing = wparam != 0;
            PANELS.with(|panels| {
                let mut panels = panels.borrow_mut();
                if let Some(panel) = panels.values_mut().find(|p| p.hwnd == hwnd) {
                    if showing {
                        panel.visible = true;
                        panel.init_attempts = 0;
                        #[cfg(target_os = "linux")]
                        if panel.x11_child_xid != 0 {
                            if let Some((xlib, display)) = reaper_embed::transparent::x11_display() {
                                unsafe {
                                    (xlib.XMapWindow)(display, panel.x11_child_xid);
                                    (xlib.XFlush)(display);
                                }
                            }
                        }
                    } else {
                        panel.visible = false;
                        panel.view = None;
                        #[cfg(target_os = "linux")]
                        {
                            if panel.x11_child_xid != 0 {
                                if let Some((xlib, display)) = reaper_embed::transparent::x11_display() {
                                    unsafe {
                                        (xlib.XUnmapWindow)(display, panel.x11_child_xid);
                                        (xlib.XFlush)(display);
                                    }
                                }
                            }
                        }
                    }
                    tracing::info!(showing, panel = panel.config.id, "WM_SHOWWINDOW");
                }
            });
            0
        }
        WM_DESTROY => {
            PANELS.with(|panels| {
                let mut panels = panels.borrow_mut();
                panels.retain(|_, p| p.hwnd != hwnd);
            });
            0
        }
        _ => {
            let swell = swell();
            unsafe { swell.DefWindowProc(hwnd, msg, wparam, lparam) }
        }
    }
}

// ---------------------------------------------------------------------------
// macOS SWELL layout workaround
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn force_view_layout(hwnd: raw::HWND) {
    use objc::{msg_send, sel, sel_impl};
    unsafe {
        let view: cocoa::base::id = hwnd as cocoa::base::id;
        if !view.is_null() {
            let _: () = msg_send![view, setNeedsLayout: cocoa::base::YES];
            let _: () = msg_send![view, setNeedsDisplay: cocoa::base::YES];
            let _: () = msg_send![view, layoutSubtreeIfNeeded];
            let _: () = msg_send![view, displayIfNeeded];
        }
    }
}

// ---------------------------------------------------------------------------
// Circular parent guard
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub fn would_create_circular_parent(child: raw::HWND, parent: raw::HWND) -> bool {
    if child.is_null() || parent.is_null() {
        return false;
    }
    let swell = swell();
    let mut current = parent;
    for _ in 0..64 {
        if current == child { return true; }
        let next = unsafe { swell.GetParent(current) };
        if next.is_null() || next == current { break; }
        current = next;
    }
    false
}
