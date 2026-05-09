//! Transparent overlay window.
//!
//! A borderless transparent window with GPU rendering via Vello. Click-through
//! by default — designed for HUD-style overlays that don't intercept input.

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle,
};
use vello::Scene;

use crate::gpu::{GpuError, GpuState};
use crate::platform::WindowRect;

/// A raw native window handle (`NSView*` on macOS, `HWND` on Windows).
pub type RawHwnd = *mut std::ffi::c_void;

/// A transparent overlay window with GPU-accelerated Vello rendering.
pub struct TransparentWindow {
    hwnd: Option<RawHwnd>,
    handle_wrapper: Option<NativeHandleWrapper>,
    gpu: Option<GpuState>,
    bounds: WindowRect,
    visible: bool,
}

// ---------------------------------------------------------------------------
// NativeHandleWrapper — platform-specific raw-window-handle bridge
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
struct NativeHandleWrapper {
    ns_view: *mut std::ffi::c_void,
}

#[cfg(target_os = "linux")]
struct NativeHandleWrapper {
    window: u64,
    display: *mut std::ffi::c_void,
    visual_id: u32,
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
struct NativeHandleWrapper {
    _dummy: (),
}

// SAFETY: REAPER extensions run single-threaded on the main thread.
unsafe impl Send for NativeHandleWrapper {}
unsafe impl Sync for NativeHandleWrapper {}

// --- macOS HasWindowHandle / HasDisplayHandle ---

#[cfg(target_os = "macos")]
impl HasWindowHandle for NativeHandleWrapper {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        use raw_window_handle::AppKitWindowHandle;
        use std::ptr::NonNull;
        let handle =
            AppKitWindowHandle::new(NonNull::new(self.ns_view).ok_or(HandleError::Unavailable)?);
        let raw = RawWindowHandle::AppKit(handle);
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

#[cfg(target_os = "macos")]
impl HasDisplayHandle for NativeHandleWrapper {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        use raw_window_handle::AppKitDisplayHandle;
        let raw = RawDisplayHandle::AppKit(AppKitDisplayHandle::new());
        Ok(unsafe { DisplayHandle::borrow_raw(raw) })
    }
}

// --- Linux HasWindowHandle / HasDisplayHandle ---

#[cfg(target_os = "linux")]
impl HasWindowHandle for NativeHandleWrapper {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        use raw_window_handle::XlibWindowHandle;
        let mut handle = XlibWindowHandle::new(self.window.try_into().unwrap_or(0));
        handle.visual_id = self.visual_id.try_into().unwrap_or(0);
        let raw = RawWindowHandle::Xlib(handle);
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

#[cfg(target_os = "linux")]
impl HasDisplayHandle for NativeHandleWrapper {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        use raw_window_handle::XlibDisplayHandle;
        use std::ptr::NonNull;
        let display_ptr = NonNull::new(self.display).ok_or(HandleError::Unavailable)?;
        let handle = XlibDisplayHandle::new(Some(display_ptr), 0);
        let raw = RawDisplayHandle::Xlib(handle);
        Ok(unsafe { DisplayHandle::borrow_raw(raw) })
    }
}

// --- Fallback (Windows, etc.) ---

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl HasWindowHandle for NativeHandleWrapper {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl HasDisplayHandle for NativeHandleWrapper {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

/// Returns a reference to the shared Xlib function table and display pointer.
/// Returns `None` if running headless (no X11 display available).
/// Used by `platform.rs` for screen queries and by `reaper-dioxus` for window handles.
#[cfg(target_os = "linux")]
pub fn x11_display() -> Option<(&'static x11_dl::xlib::Xlib, *mut x11_dl::xlib::Display)> {
    Some((x11_state::xlib()?, x11_state::display()?))
}

// ---------------------------------------------------------------------------
// TransparentWindow implementation
// ---------------------------------------------------------------------------

impl TransparentWindow {
    /// Open a new transparent click-through overlay window.
    pub fn open(x: i32, y: i32, width: u32, height: u32) -> Result<Self, GpuError> {
        let bounds = WindowRect::new(x, y, width, height);

        let (hwnd, handle_wrapper) = create_native_transparent_window(x, y, width, height)
            .ok_or(GpuError::InvalidWindowHandle)?;

        // Make click-through by default
        set_native_click_through(hwnd, true);

        let gpu = GpuState::new(&handle_wrapper, width, height)?;

        Ok(Self {
            hwnd: Some(hwnd),
            handle_wrapper: Some(handle_wrapper),
            gpu: Some(gpu),
            bounds,
            visible: false,
        })
    }

    pub fn show(&mut self) {
        if let Some(hwnd) = self.hwnd {
            show_native_window(hwnd, true);
            self.visible = true;
        }
    }

    pub fn hide(&mut self) {
        if let Some(hwnd) = self.hwnd {
            show_native_window(hwnd, false);
            self.visible = false;
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Move and resize the window.
    pub fn set_frame(&mut self, x: i32, y: i32, width: u32, height: u32) {
        self.bounds = WindowRect::new(x, y, width, height);

        if let Some(hwnd) = self.hwnd {
            set_native_window_frame(hwnd, x, y, width, height);
        }

        if let Some(ref mut gpu) = self.gpu {
            gpu.resize(width, height);
        }
    }

    /// Render a Vello scene to this window.
    pub fn render(&mut self, scene: &Scene) -> Result<(), GpuError> {
        if !self.visible {
            return Ok(());
        }
        let gpu = self.gpu.as_mut().ok_or(GpuError::InvalidWindowHandle)?;
        gpu.render(scene)
    }

    pub fn bounds(&self) -> &WindowRect {
        &self.bounds
    }

    /// Set whether this window is click-through (input passes to windows below).
    pub fn set_click_through(&mut self, click_through: bool) {
        if let Some(hwnd) = self.hwnd {
            set_native_click_through(hwnd, click_through);
        }
    }

    pub fn close(&mut self) {
        if let Some(hwnd) = self.hwnd.take() {
            close_native_window(hwnd);
        }
        self.handle_wrapper = None;
        self.gpu = None;
        self.visible = false;
    }
}

impl Drop for TransparentWindow {
    fn drop(&mut self) {
        self.close();
    }
}

// ===========================================================================
// Platform: macOS (Cocoa)
// ===========================================================================

#[cfg(target_os = "macos")]
fn create_native_transparent_window(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Option<(RawHwnd, NativeHandleWrapper)> {
    use cocoa::appkit::{NSBackingStoreType, NSWindow, NSWindowStyleMask};
    use cocoa::base::{NO, YES, id, nil};
    use cocoa::foundation::{NSPoint, NSRect, NSSize};
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        // Get main screen height for coordinate conversion (macOS is bottom-left origin)
        let screens: id = msg_send![class!(NSScreen), screens];
        let screen_height = if screens.is_null() {
            1080.0
        } else {
            let count: usize = msg_send![screens, count];
            if count == 0 {
                1080.0
            } else {
                let main_screen: id = msg_send![screens, objectAtIndex: 0usize];
                let frame: NSRect = msg_send![main_screen, frame];
                frame.size.height
            }
        };

        let flipped_y = screen_height - (y as f64) - (height as f64);

        let frame = NSRect::new(
            NSPoint::new(x as f64, flipped_y),
            NSSize::new(width as f64, height as f64),
        );

        let style = NSWindowStyleMask::NSBorderlessWindowMask;

        let window: id = msg_send![class!(NSWindow), alloc];
        let window: id = msg_send![
            window,
            initWithContentRect:frame
            styleMask:style
            backing:NSBackingStoreType::NSBackingStoreBuffered
            defer:NO
        ];

        if window.is_null() {
            tracing::error!("Failed to create NSWindow");
            return None;
        }

        let _: () = msg_send![window, setOpaque: NO];
        let clear_color: id = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![window, setBackgroundColor: clear_color];
        let _: () = msg_send![window, setHasShadow: NO];
        let _: () = msg_send![window, setLevel: 3i64]; // NSFloatingWindowLevel

        let content_view: id = msg_send![window, contentView];
        let _: () = msg_send![content_view, setWantsLayer: YES];

        let layer: id = msg_send![content_view, layer];
        if !layer.is_null() {
            let _: () = msg_send![layer, setOpaque: NO];
            let cg_clear: id = msg_send![class!(NSColor), clearColor];
            let cg_color: id = msg_send![cg_clear, CGColor];
            let _: () = msg_send![layer, setBackgroundColor: cg_color];
        }

        tracing::debug!(x, y, width, height, "Created transparent overlay window");

        let wrapper = NativeHandleWrapper {
            ns_view: content_view as *mut std::ffi::c_void,
        };

        Some((content_view as RawHwnd, wrapper))
    }
}

#[cfg(target_os = "macos")]
fn show_native_window(ns_view: RawHwnd, show: bool) {
    use cocoa::base::{id, nil};
    use objc::{msg_send, sel, sel_impl};

    unsafe {
        let view: id = ns_view as id;
        if view.is_null() {
            return;
        }
        let window: id = msg_send![view, window];
        if window.is_null() {
            return;
        }
        if show {
            let _: () = msg_send![window, orderFront: nil];
        } else {
            let _: () = msg_send![window, orderOut: nil];
        }
    }
}

#[cfg(target_os = "macos")]
fn set_native_window_frame(ns_view: RawHwnd, x: i32, y: i32, width: u32, height: u32) {
    use cocoa::base::{YES, id};
    use cocoa::foundation::{NSPoint, NSRect, NSSize};
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let view: id = ns_view as id;
        if view.is_null() {
            return;
        }
        let window: id = msg_send![view, window];
        if window.is_null() {
            return;
        }

        let screens: id = msg_send![class!(NSScreen), screens];
        let screen_height = if screens.is_null() {
            1080.0
        } else {
            let count: usize = msg_send![screens, count];
            if count == 0 {
                1080.0
            } else {
                let main_screen: id = msg_send![screens, objectAtIndex: 0usize];
                let frame: NSRect = msg_send![main_screen, frame];
                frame.size.height
            }
        };

        let flipped_y = screen_height - (y as f64) - (height as f64);
        let frame = NSRect::new(
            NSPoint::new(x as f64, flipped_y),
            NSSize::new(width as f64, height as f64),
        );

        let _: () = msg_send![window, setFrame:frame display:YES];
    }
}

#[cfg(target_os = "macos")]
fn close_native_window(ns_view: RawHwnd) {
    use cocoa::base::id;
    use objc::{msg_send, sel, sel_impl};

    unsafe {
        let view: id = ns_view as id;
        if view.is_null() {
            return;
        }
        let window: id = msg_send![view, window];
        if !window.is_null() {
            let _: () = msg_send![window, close];
        }
    }
}

#[cfg(target_os = "macos")]
fn set_native_click_through(ns_view: RawHwnd, click_through: bool) {
    use cocoa::base::{NO, YES, id};
    use objc::{msg_send, sel, sel_impl};

    unsafe {
        let view: id = ns_view as id;
        if view.is_null() {
            return;
        }
        let window: id = msg_send![view, window];
        if window.is_null() {
            return;
        }
        let value = if click_through { YES } else { NO };
        let _: () = msg_send![window, setIgnoresMouseEvents: value];
    }
}

// ===========================================================================
// Platform: Linux (X11)
// ===========================================================================

#[cfg(target_os = "linux")]
mod x11_state {
    use std::sync::Mutex;
    use x11_dl::xlib::{Display, Xlib};

    /// Process-global Xlib function table + persistent display connection.
    struct X11Global {
        xlib: Xlib,
        display: *mut Display,
    }

    // SAFETY: REAPER extensions are single-threaded on the main thread.
    unsafe impl Send for X11Global {}
    unsafe impl Sync for X11Global {}

    /// Retry-capable lazy init. Unlike OnceLock, this retries if the initial
    /// attempt fails (e.g., DISPLAY="" during headless init, then restored
    /// by the test wrapper script before the first panel opens).
    static GLOBAL: Mutex<Option<X11Global>> = Mutex::new(None);

    fn try_init() -> bool {
        let mut guard = GLOBAL.lock().unwrap();
        if guard.is_some() {
            return true;
        }
        let xlib = match Xlib::open() {
            Ok(xlib) => xlib,
            Err(_) => {
                tracing::debug!("Failed to load libX11");
                return false;
            }
        };
        let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
        if display.is_null() {
            tracing::debug!("Failed to open X11 display");
            return false;
        }
        *guard = Some(X11Global { xlib, display });
        true
    }

    pub fn xlib() -> Option<&'static Xlib> {
        if !try_init() {
            return None;
        }
        let guard = GLOBAL.lock().unwrap();
        // SAFETY: Once initialized, the value lives for the process lifetime.
        // We never remove it from the Mutex.
        guard
            .as_ref()
            .map(|g| unsafe { &*(&g.xlib as *const Xlib) })
    }

    pub fn display() -> Option<*mut Display> {
        if !try_init() {
            return None;
        }
        let guard = GLOBAL.lock().unwrap();
        guard.as_ref().map(|g| g.display)
    }
}

#[cfg(target_os = "linux")]
fn create_native_transparent_window(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Option<(RawHwnd, NativeHandleWrapper)> {
    use x11_dl::xlib::{
        AllocNone, CWBackPixel, CWBorderPixel, CWColormap, CWOverrideRedirect, InputOutput, True,
        TrueColor, XSetWindowAttributes, XVisualInfo,
    };

    let xlib = x11_state::xlib()?;
    let display = x11_state::display()?;

    unsafe {
        let screen = (xlib.XDefaultScreen)(display);
        let root = (xlib.XRootWindow)(display, screen);

        // Find a 32-bit ARGB visual for transparency
        let mut visual_info = std::mem::zeroed::<XVisualInfo>();
        let found = (xlib.XMatchVisualInfo)(display, screen, 32, TrueColor, &mut visual_info);
        if found == 0 {
            tracing::error!("No 32-bit ARGB visual available — compositor required");
            return None;
        }

        // Create a colormap for the 32-bit visual
        let colormap = (xlib.XCreateColormap)(display, root, visual_info.visual, AllocNone);

        let mut attrs: XSetWindowAttributes = std::mem::zeroed();
        attrs.override_redirect = True;
        attrs.colormap = colormap;
        attrs.border_pixel = 0;
        attrs.background_pixel = 0; // Transparent (ARGB 0x00000000)

        let attr_mask = CWOverrideRedirect | CWColormap | CWBorderPixel | CWBackPixel;

        let window = (xlib.XCreateWindow)(
            display,
            root,
            x,
            y,
            width,
            height,
            0,  // border width
            32, // depth — must match visual
            InputOutput as u32,
            visual_info.visual,
            attr_mask,
            &mut attrs,
        );

        if window == 0 {
            tracing::error!("Failed to create X11 window");
            (xlib.XFreeColormap)(display, colormap);
            return None;
        }

        // Set window type to notification/utility — avoids taskbar entry
        set_x11_atoms(&xlib, display, window);

        (xlib.XFlush)(display);

        tracing::debug!(
            x,
            y,
            width,
            height,
            "Created X11 transparent overlay window"
        );

        let visual_id = visual_info.visualid as u32;

        // We store the Display* and Window as the "hwnd". Since we need both,
        // we use the wrapper for the display and pack the X11 Window as the hwnd.
        let wrapper = NativeHandleWrapper {
            window,
            display: display as *mut std::ffi::c_void,
            visual_id,
        };

        // Use the window XID cast to pointer as the RawHwnd for the platform functions.
        // We'll recover it via the NativeHandleWrapper stored alongside.
        Some((window as RawHwnd, wrapper))
    }
}

#[cfg(target_os = "linux")]
fn set_x11_atoms(xlib_fns: &x11_dl::xlib::Xlib, display: *mut x11_dl::xlib::Display, window: u64) {
    use std::ffi::CString;
    use x11_dl::xlib::{False, PropModeReplace, XA_ATOM};

    unsafe {
        let atom_name = |name: &str| {
            let c = CString::new(name).unwrap();
            (xlib_fns.XInternAtom)(display, c.as_ptr(), False)
        };

        // Window type: utility (floats above, no taskbar)
        let wm_type = atom_name("_NET_WM_WINDOW_TYPE");
        let wm_type_utility = atom_name("_NET_WM_WINDOW_TYPE_UTILITY");
        (xlib_fns.XChangeProperty)(
            display,
            window,
            wm_type,
            XA_ATOM,
            32,
            PropModeReplace,
            &wm_type_utility as *const u64 as *const u8,
            1,
        );

        // State: above + skip taskbar + skip pager
        let wm_state = atom_name("_NET_WM_STATE");
        let wm_state_above = atom_name("_NET_WM_STATE_ABOVE");
        let wm_skip_taskbar = atom_name("_NET_WM_STATE_SKIP_TASKBAR");
        let wm_skip_pager = atom_name("_NET_WM_STATE_SKIP_PAGER");
        let state_atoms = [wm_state_above, wm_skip_taskbar, wm_skip_pager];
        (xlib_fns.XChangeProperty)(
            display,
            window,
            wm_state,
            XA_ATOM,
            32,
            PropModeReplace,
            state_atoms.as_ptr() as *const u8,
            state_atoms.len() as i32,
        );
    }
}

#[cfg(target_os = "linux")]
fn show_native_window(hwnd: RawHwnd, show: bool) {
    let Some(xlib) = x11_state::xlib() else {
        return;
    };
    let Some(display) = x11_state::display() else {
        return;
    };
    let window = hwnd as u64;

    unsafe {
        if show {
            (xlib.XMapRaised)(display, window);
        } else {
            (xlib.XUnmapWindow)(display, window);
        }
        (xlib.XFlush)(display);
    }
}

#[cfg(target_os = "linux")]
fn set_native_window_frame(hwnd: RawHwnd, x: i32, y: i32, width: u32, height: u32) {
    let Some(xlib) = x11_state::xlib() else {
        return;
    };
    let Some(display) = x11_state::display() else {
        return;
    };
    let window = hwnd as u64;

    unsafe {
        (xlib.XMoveResizeWindow)(display, window, x, y, width, height);
        (xlib.XFlush)(display);
    }
}

#[cfg(target_os = "linux")]
fn close_native_window(hwnd: RawHwnd) {
    let Some(xlib) = x11_state::xlib() else {
        return;
    };
    let Some(display) = x11_state::display() else {
        return;
    };
    let window = hwnd as u64;

    unsafe {
        (xlib.XDestroyWindow)(display, window);
        (xlib.XFlush)(display);
    }
}

#[cfg(target_os = "linux")]
fn set_native_click_through(hwnd: RawHwnd, click_through: bool) {
    let Some(xlib) = x11_state::xlib() else {
        return;
    };
    let Some(display) = x11_state::display() else {
        return;
    };
    let window = hwnd as u64;

    unsafe {
        // Try to load the XFixes extension
        if let Ok(xfixes) = x11_dl::xfixes::Xlib::open() {
            if click_through {
                // Create an empty region (no input area → all clicks pass through)
                let region = (xfixes.XFixesCreateRegion)(display, std::ptr::null_mut(), 0);
                (xfixes.XFixesSetWindowShapeRegion)(
                    display, window, 2, // ShapeInput
                    0, 0, region,
                );
                (xfixes.XFixesDestroyRegion)(display, region);
            } else {
                // Reset to normal (accept input everywhere)
                (xfixes.XFixesSetWindowShapeRegion)(
                    display, window, 2, // ShapeInput
                    0, 0, 0, // None — reset to default
                );
            }
        } else {
            tracing::warn!("XFixes extension not available — click-through not supported");
        }

        (xlib.XFlush)(display);
    }
}

// ===========================================================================
// Platform: Fallback (unsupported)
// ===========================================================================

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn create_native_transparent_window(
    _x: i32,
    _y: i32,
    _width: u32,
    _height: u32,
) -> Option<(RawHwnd, NativeHandleWrapper)> {
    tracing::warn!("Transparent windows not yet implemented on this platform");
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn show_native_window(_hwnd: RawHwnd, _show: bool) {}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn set_native_window_frame(_hwnd: RawHwnd, _x: i32, _y: i32, _width: u32, _height: u32) {}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn close_native_window(_hwnd: RawHwnd) {}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn set_native_click_through(_hwnd: RawHwnd, _click_through: bool) {}
