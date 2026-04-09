//! DockablePanel — Dioxus rendering inside REAPER's native docker system.
//!
//! Creates a SWELL dialog window, registers it with REAPER's docker via
//! `DockWindowAddEx`, and renders a Dioxus component tree into it using
//! the `EmbeddedView` GPU pipeline.
//!
//! Supports:
//! - Dockable/floating panels via REAPER's standard docker
//! - Tab integration (shows up alongside Mixer, FX Browser, etc.)
//! - State persistence via ExtState (dock position saved across sessions)
//! - Toggle show/hide via REAPER actions
//! - HiDPI scaling
//! - macOS SWELL layout workaround

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

/// Unique identifier for a dockable panel, used for REAPER dock persistence.
type PanelId = &'static str;

/// Configuration for a dockable panel.
#[derive(Clone)]
pub struct DockablePanelConfig {
    /// Unique identifier string for REAPER dock configuration persistence.
    /// Must be unique across all extensions (e.g., "FTS_INPUT_WHICHKEY").
    pub id: PanelId,
    /// Display name shown in the panel's title bar and dock tab.
    pub title: &'static str,
    /// Default dock position (0 = bottom, 1 = left, 2 = top, 3 = right, -1 = floating).
    pub default_dock_position: i32,
    /// The root Dioxus component to render.
    pub app: fn() -> Element,
    /// Initial width when floating.
    pub default_width: u32,
    /// Initial height when floating.
    pub default_height: u32,
    /// Whether to show the panel on first launch (before user has toggled it).
    pub show_on_first_launch: bool,
}

/// A live dockable panel registered with REAPER's docker system.
struct LivePanel {
    config: DockablePanelConfig,
    hwnd: raw::HWND,
    view: Option<EmbeddedView>,
    visible: bool,
    contexts: Vec<Box<dyn FnOnce()>>,
}

// ---------------------------------------------------------------------------
// Global registry (thread-local, main thread only)
// ---------------------------------------------------------------------------

thread_local! {
    static PANELS: RefCell<HashMap<PanelId, LivePanel>> = RefCell::new(HashMap::new());
}

/// Cached references to REAPER/SWELL APIs, set on first `register_panel` call.
static REAPER_API: std::sync::OnceLock<&'static Reaper> = std::sync::OnceLock::new();
static SWELL_API: std::sync::OnceLock<&'static Swell> = std::sync::OnceLock::new();

fn reaper() -> &'static Reaper {
    REAPER_API.get().expect("register_panel must be called before using dock APIs")
}

fn swell() -> &'static Swell {
    SWELL_API.get().expect("register_panel must be called before using dock APIs")
}

// ---------------------------------------------------------------------------
// HWND wrapper for raw-window-handle traits
// ---------------------------------------------------------------------------

struct SwellHwndWrapper {
    hwnd: raw::HWND,
    /// On Linux: X11 XID for the render surface (may be a child window we created).
    #[cfg(target_os = "linux")]
    xid: u64,
}

unsafe impl Send for SwellHwndWrapper {}
unsafe impl Sync for SwellHwndWrapper {}

impl SwellHwndWrapper {
    fn new(hwnd: raw::HWND, swell: &Swell) -> Self {
        #[cfg(target_os = "linux")]
        let xid = extract_x11_xid(hwnd, swell);
        Self {
            hwnd,
            #[cfg(target_os = "linux")]
            xid,
        }
    }

    /// Create a wrapper with an explicit X11 window ID (for direct X11 windows).
    #[cfg(target_os = "linux")]
    fn with_xid(hwnd: raw::HWND, xid: u64) -> Self {
        Self { hwnd, xid }
    }
}

impl HasWindowHandle for SwellHwndWrapper {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        #[cfg(target_os = "macos")]
        {
            use raw_window_handle::AppKitWindowHandle;
            use std::ptr::NonNull;
            // On macOS, SWELL HWND is an NSView*
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
            let handle = XlibWindowHandle::new(self.xid.try_into().unwrap_or(0));
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

/// Extract the X11 Window ID (XID) from a SWELL HWND on Linux.
///
/// Path: SWELL HWND → SWELL_GetOSWindow("GdkWindow") → gdk_x11_window_get_xid()
/// This matches the pattern used by ReaImGui (gdk_window.cpp).
#[cfg(target_os = "linux")]
fn extract_x11_xid(hwnd: raw::HWND, swell: &Swell) -> u64 {
    tracing::info!("extract_x11_xid: hwnd={:?}", hwnd);
    if hwnd.is_null() {
        tracing::error!("extract_x11_xid: HWND is null!");
        return 0;
    }

    // Step 1: Get GdkWindow* from SWELL HWND
    let gdk_type = std::ffi::CString::new("GdkWindow").unwrap();
    let gdk_window = unsafe { swell.SWELL_GetOSWindow(hwnd, gdk_type.as_ptr()) };
    if gdk_window.is_null() {
        tracing::error!(
            "SWELL_GetOSWindow returned null for HWND {:?} — window not realized or headless",
            hwnd
        );
        return 0;
    }

    // Step 2: Call gdk_x11_window_get_xid(gdk_window) via dlopen
    // This function is in libgdk-x11-2.0.so (GDK3 X11 backend)
    type GdkX11WindowGetXid = unsafe extern "C" fn(*mut c_void) -> u64;

    let lib_names = [
        "libgdk-3.so.0",
        "libgdk-3.so",
        "libgdk-x11-2.0.so.0",
        "libgdk-x11-2.0.so",
    ];

    for lib_name in &lib_names {
        let c_lib = std::ffi::CString::new(*lib_name).unwrap();
        let handle = unsafe { libc::dlopen(c_lib.as_ptr(), libc::RTLD_LAZY | libc::RTLD_NOLOAD) };
        if handle.is_null() {
            continue;
        }

        let sym_name = std::ffi::CString::new("gdk_x11_window_get_xid").unwrap();
        let sym = unsafe { libc::dlsym(handle, sym_name.as_ptr()) };
        unsafe { libc::dlclose(handle) };

        if !sym.is_null() {
            let func: GdkX11WindowGetXid = unsafe { std::mem::transmute(sym) };
            let xid = unsafe { func(gdk_window) };
            tracing::debug!(xid, "Extracted X11 XID from SWELL HWND via GDK");
            return xid;
        }
    }

    tracing::warn!("Could not find gdk_x11_window_get_xid — GDK X11 backend not loaded?");
    0
}

impl HasDisplayHandle for SwellHwndWrapper {
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
// Public API
// ---------------------------------------------------------------------------

/// Register a dockable panel with REAPER's docker system.
///
/// Call this during extension initialization (after SWELL and REAPER APIs are loaded).
/// The panel is created but not necessarily visible — use `toggle_panel` or
/// `show_panel` to display it.
///
/// The `contexts` vector allows injecting Dioxus context values that will be
/// available to the component tree (e.g., shared state signals).
pub fn register_panel(
    config: DockablePanelConfig,
    contexts: Vec<Box<dyn FnOnce()>>,
    reaper: &'static Reaper,
    swell: &'static Swell,
) {
    let id = config.id;

    // Cache the API references for use by other dock functions
    let _ = REAPER_API.set(reaper);
    let _ = SWELL_API.set(swell);

    // Create a SWELL dialog window for the panel
    let hwnd = create_panel_window(&config, swell);
    if hwnd.is_null() {
        tracing::error!("Failed to create dock panel window for '{}'", config.title);
        return;
    }

    // Check if REAPER has a saved dock configuration for this panel
    let ident = CString::new(config.id).unwrap();
    let saved_dock = unsafe { reaper.GetConfigWantsDock(ident.as_ptr()) };

    let dock_position = if saved_dock >= 0 {
        saved_dock
    } else {
        config.default_dock_position
    };

    // Register with REAPER's docker
    let title_c = CString::new(config.title).unwrap();
    unsafe {
        reaper.DockWindowAddEx(
            hwnd,
            title_c.as_ptr(),
            ident.as_ptr(),
            config.show_on_first_launch,
        );
    }

    // If we have a specific dock position and it's not floating, update it
    if dock_position >= 0 && saved_dock < 0 {
        unsafe {
            reaper.Dock_UpdateDockID(ident.as_ptr(), dock_position);
        }
    }

    let panel = LivePanel {
        config,
        hwnd,
        view: None, // Created lazily on first show
        visible: false,
        contexts,
    };

    PANELS.with(|panels| {
        panels.borrow_mut().insert(id, panel);
    });

    tracing::debug!(
        id,
        dock_position,
        "Registered dockable panel '{}'",
        id
    );
}

/// Toggle a panel's visibility.
pub fn toggle_panel(id: PanelId) {
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        if let Some(panel) = panels.get_mut(id) {
            if panel.visible {
                hide_panel_inner(panel);
            } else {
                show_panel_inner(panel);
            }
        }
    });
}

/// Show a panel (make it visible).
pub fn show_panel(id: PanelId) {
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        if let Some(panel) = panels.get_mut(id) {
            if !panel.visible {
                show_panel_inner(panel);
            }
        }
    });
}

/// Hide a panel.
pub fn hide_panel(id: PanelId) {
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        if let Some(panel) = panels.get_mut(id) {
            if panel.visible {
                hide_panel_inner(panel);
            }
        }
    });
}

/// Check if a panel is currently visible.
pub fn is_panel_visible(id: PanelId) -> bool {
    PANELS.with(|panels| {
        panels
            .borrow()
            .get(id)
            .map_or(false, |p| p.visible)
    })
}

/// Update all visible panels (call from timer callback ~30Hz).
pub fn update_panels() {
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        for panel in panels.values_mut() {
            if panel.visible {
                // Try to initialize EmbeddedView if not yet created
                // (deferred from show_panel_inner to allow GDK window realization)
                try_init_embedded_view(panel);

                if let Some(view) = &mut panel.view {
                    // Check if the window was resized by REAPER's docker
                    let swell = swell();
                    let mut rect = raw::RECT {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0,
                    };
                    unsafe {
                        swell.GetClientRect(panel.hwnd, &mut rect);
                    }
                    let w = (rect.right - rect.left).unsigned_abs();
                    let h = (rect.bottom - rect.top).unsigned_abs();
                    let (cur_w, cur_h) = view.size();
                    if w > 0 && h > 0 && (w != cur_w || h != cur_h) {
                        view.resize(w, h);
                    }

                    view.update();
                }
            }
        }
    });
}

/// Unregister all panels (call during extension shutdown).
pub fn unregister_all_panels() {
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        let reaper = reaper();
        for (_, panel) in panels.drain() {
            if !panel.hwnd.is_null() {
                unsafe {
                    reaper.DockWindowRemove(panel.hwnd);
                }
                let swell = swell();
                unsafe {
                    swell.DestroyWindow(panel.hwnd);
                }
            }
        }
    });
}

/// Save dock state for all panels via ExtState.
pub fn save_dock_state() {
    PANELS.with(|panels| {
        let panels = panels.borrow();
        let reaper = reaper();

        for (id, panel) in panels.iter() {
            let section = CString::new(*id).unwrap();
            let vis_key = CString::new("visible").unwrap();
            let vis_val = CString::new(if panel.visible { "1" } else { "0" }).unwrap();
            unsafe {
                reaper.SetExtState(section.as_ptr(), vis_key.as_ptr(), vis_val.as_ptr(), true);
            }

            // Save dock position
            let mut is_floating = false;
            let dock_id = unsafe {
                reaper.DockIsChildOfDock(panel.hwnd, &mut is_floating)
            };
            let dock_key = CString::new("dock_id").unwrap();
            let dock_val = CString::new(format!("{}", dock_id)).unwrap();
            unsafe {
                reaper.SetExtState(section.as_ptr(), dock_key.as_ptr(), dock_val.as_ptr(), true);
            }
        }
    });
}

/// Load and restore dock state from ExtState.
pub fn restore_dock_state() {
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        let reaper = reaper();

        for (id, panel) in panels.iter_mut() {
            let section = CString::new(*id).unwrap();
            let vis_key = CString::new("visible").unwrap();

            let vis_ptr = unsafe { reaper.GetExtState(section.as_ptr(), vis_key.as_ptr()) };
            if !vis_ptr.is_null() {
                let vis_str = unsafe { std::ffi::CStr::from_ptr(vis_ptr) }
                    .to_string_lossy();
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

    // Show the window FIRST — SWELL/GDK needs to realize the window
    // before we can extract the X11 XID for GPU surface creation.
    unsafe {
        swell.ShowWindow(panel.hwnd, raw::SW_SHOW as c_int);
        reaper.DockWindowActivate(panel.hwnd);
    }

    // macOS SWELL workaround: force layout after showing
    #[cfg(target_os = "macos")]
    force_view_layout(panel.hwnd);

    panel.visible = true;
    // Note: EmbeddedView creation is deferred to update_panels() —
    // the GDK window needs a tick to realize after ShowWindow.
}

/// Try to initialize the EmbeddedView for a panel if not yet created.
/// Called from update_panels() after the window has been shown and realized.
fn try_init_embedded_view(panel: &mut LivePanel) {
    if panel.view.is_some() || !panel.visible {
        return;
    }

    let swell = swell();
    let mut rect = raw::RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe {
        swell.GetClientRect(panel.hwnd, &mut rect);
    }
    let w = (rect.right - rect.left).unsigned_abs().max(panel.config.default_width);
    let h = (rect.bottom - rect.top).unsigned_abs().max(panel.config.default_height);

    let wrapper = SwellHwndWrapper::new(panel.hwnd, swell);

    // Check if the window handle is valid before consuming contexts
    #[cfg(target_os = "linux")]
    let wrapper = if wrapper.window_handle().is_err() {
        // SWELL GDK path failed — create a direct X11 child window as render target
        match create_x11_child_for_panel(panel, w, h) {
            Some(xid) => {
                tracing::info!(xid, "Created X11 child window for panel '{}'", panel.config.id);
                SwellHwndWrapper::with_xid(panel.hwnd, xid)
            }
            None => {
                tracing::debug!(
                    "X11 child window not ready for '{}' (will retry)",
                    panel.config.id
                );
                return;
            }
        }
    } else {
        wrapper
    };

    #[cfg(not(target_os = "linux"))]
    if wrapper.window_handle().is_err() {
        tracing::debug!(
            "Window handle not ready for '{}' (will retry next tick)",
            panel.config.id
        );
        return;
    }

    let contexts = std::mem::take(&mut panel.contexts);
    match EmbeddedView::new(panel.config.app, &wrapper, w, h, contexts) {
        Ok(view) => {
            panel.view = Some(view);
            tracing::info!("Created EmbeddedView for panel '{}'", panel.config.id);
        }
        Err(e) => {
            tracing::debug!(
                "EmbeddedView not ready for '{}': {} (will retry)",
                panel.config.id,
                e
            );
        }
    }
}

fn hide_panel_inner(panel: &mut LivePanel) {
    let swell = swell();
    unsafe {
        swell.ShowWindow(panel.hwnd, raw::SW_HIDE as c_int);
    }
    panel.visible = false;
}

// ---------------------------------------------------------------------------
// Window creation
// ---------------------------------------------------------------------------

/// Create a SWELL dialog window for the dockable panel.
///
/// On Linux, SWELL uses GDK for window management. The dialog must be
/// created with proper style flags so SWELL allocates a real GdkWindow
/// (which we later extract via SWELL_GetOSWindow for GPU rendering).
///
/// Uses SWELL's magic resid `0x400000 + flags` to bypass resource lookup
/// and create a window with WS_CAPTION + WS_THICKFRAME directly.
fn create_panel_window(config: &DockablePanelConfig, swell: &Swell) -> raw::HWND {
    let title_c = CString::new(config.title).unwrap();

    unsafe {
        // Magic resid: 0x400000 + flags
        // bit 0 = resizable (WS_THICKFRAME)
        // This tells SWELL_CreateDialog to create a top-level window with
        // WS_CAPTION | WS_THICKFRAME without needing a dialog resource.
        let magic_resid = (0x400000usize | 0x1) as *const std::os::raw::c_char; // resizable

        let hwnd = swell.CreateDialogParam(
            ptr::null_mut(),       // hInstance — not used on SWELL
            magic_resid,           // magic resid → creates real window
            ptr::null_mut(),       // parent — null, REAPER reparents via DockWindowAddEx
            Some(panel_wndproc),
            0,
        );

        if !hwnd.is_null() {
            swell.SetWindowText(hwnd, title_c.as_ptr());
            swell.SetWindowPos(
                hwnd,
                ptr::null_mut(),
                0,
                0,
                config.default_width as c_int,
                config.default_height as c_int,
                0x0006, // SWP_NOZORDER | SWP_NOMOVE
            );
        }

        hwnd
    }
}

/// Create a direct X11 child window for GPU rendering inside a docked panel.
///
/// When SWELL's GDK path doesn't produce a GdkWindow (common for docker-managed
/// windows), we create a raw X11 window as the render target. This window is
/// a child of the root window and gets its position/size managed by our update loop.
#[cfg(target_os = "linux")]
fn create_x11_child_for_panel(_panel: &LivePanel, width: u32, height: u32) -> Option<u64> {
    use reaper_embed::transparent::x11_display;

    let (xlib, display) = x11_display()?;
    let screen = unsafe { (xlib.XDefaultScreen)(display) };
    let root = unsafe { (xlib.XRootWindow)(display, screen) };

    let window = unsafe {
        (xlib.XCreateSimpleWindow)(
            display,
            root,
            0, 0,
            width, height,
            0,  // border width
            0,  // border color
            0x121212, // dark background
        )
    };

    if window == 0 {
        return None;
    }

    // Map the window (make it visible)
    unsafe {
        (xlib.XMapWindow)(display, window);
        (xlib.XFlush)(display);
    }

    Some(window)
}

/// Alternative window creation using platform-native APIs.
/// Used as a fallback if SWELL magic resid doesn't produce a real window.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
fn create_panel_window_x11(config: &DockablePanelConfig) -> raw::HWND {
    // On Linux, if SWELL's dialog creation doesn't produce a GdkWindow,
    // we can create an X11 window directly and use it as the render target.
    // This bypasses SWELL entirely for the panel content window.
    use reaper_embed::transparent::x11_display;

    let Some((xlib, display)) = x11_display() else {
        tracing::error!("No X11 display — cannot create panel window");
        return ptr::null_mut();
    };

    unsafe {
        let screen = (xlib.XDefaultScreen)(display);
        let root = (xlib.XRootWindow)(display, screen);

        let window = (xlib.XCreateSimpleWindow)(
            display,
            root,
            0, 0,
            config.default_width, config.default_height,
            0,  // border
            0,  // border color
            0,  // background
        );

        if window == 0 {
            return ptr::null_mut();
        }

        (xlib.XFlush)(display);
        window as raw::HWND
    }
}

/// Window procedure for dockable panel windows.
///
/// Handles resize events and forwards them to the EmbeddedView.
unsafe extern "C" fn panel_wndproc(
    hwnd: raw::HWND,
    msg: raw::UINT,
    wparam: raw::WPARAM,
    lparam: raw::LPARAM,
) -> raw::LRESULT {
    // WM_SIZE = 0x0005
    const WM_SIZE: u32 = 0x0005;
    // WM_DESTROY = 0x0002
    const WM_DESTROY: u32 = 0x0002;

    match msg {
        WM_SIZE => {
            // REAPER's docker resized this window — the EmbeddedView will
            // pick up the new size on the next update() call via GetClientRect.
            0
        }
        WM_DESTROY => {
            // Panel is being destroyed — clean up
            PANELS.with(|panels| {
                let mut panels = panels.borrow_mut();
                // Find and remove the panel by hwnd
                panels.retain(|_, p| p.hwnd != hwnd);
            });
            0
        }
        _ => {
            // Default window procedure
            let swell = swell();
            unsafe { swell.DefWindowProc(hwnd, msg, wparam, lparam) }
        }
    }
}

// ---------------------------------------------------------------------------
// macOS SWELL layout workaround
// ---------------------------------------------------------------------------

/// Force Cocoa layout + display after reparenting or showing a SWELL window.
///
/// SWELL's SetParent/ShowWindow don't always trigger NSView's
/// `setNeedsLayout:` and `setNeedsDisplay:`, which can leave child views
/// incorrectly positioned. This function forces the layout pass.
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

/// Check if `child` is an ancestor of `parent`, preventing circular reparenting.
///
/// Walks the parent chain from `parent` upward. If we encounter `child`,
/// reparenting would create a cycle — return true to abort.
#[allow(dead_code)]
pub fn would_create_circular_parent(child: raw::HWND, parent: raw::HWND) -> bool {
    if child.is_null() || parent.is_null() {
        return false;
    }

    let swell = swell();
    let mut current = parent;

    // Walk up the parent chain (max 64 levels to prevent infinite loops)
    for _ in 0..64 {
        if current == child {
            return true; // Circular!
        }
        let next = unsafe { swell.GetParent(current) };
        if next.is_null() || next == current {
            break;
        }
        current = next;
    }

    false
}
