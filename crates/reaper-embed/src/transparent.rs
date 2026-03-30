//! Transparent overlay window.
//!
//! A borderless transparent window with GPU rendering via Vello. Click-through
//! by default — designed for HUD-style overlays that don't intercept input.

use std::ptr::NonNull;

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

struct NativeHandleWrapper {
    #[cfg(target_os = "macos")]
    ns_view: *mut std::ffi::c_void,
    #[cfg(not(target_os = "macos"))]
    _dummy: (),
}

// SAFETY: REAPER extensions run single-threaded on the main thread.
unsafe impl Send for NativeHandleWrapper {}
unsafe impl Sync for NativeHandleWrapper {}

#[cfg(target_os = "macos")]
impl HasWindowHandle for NativeHandleWrapper {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        use raw_window_handle::AppKitWindowHandle;
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

#[cfg(not(target_os = "macos"))]
impl HasWindowHandle for NativeHandleWrapper {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

#[cfg(not(target_os = "macos"))]
impl HasDisplayHandle for NativeHandleWrapper {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

impl TransparentWindow {
    /// Open a new transparent click-through overlay window.
    pub fn open(x: i32, y: i32, width: u32, height: u32) -> Result<Self, GpuError> {
        let bounds = WindowRect::new(x, y, width, height);

        let hwnd = create_native_transparent_window(x, y, width, height);
        let hwnd = hwnd.ok_or(GpuError::InvalidWindowHandle)?;

        // Make click-through by default
        set_native_click_through(hwnd, true);

        #[cfg(target_os = "macos")]
        let handle_wrapper = NativeHandleWrapper {
            ns_view: hwnd as *mut std::ffi::c_void,
        };

        #[cfg(not(target_os = "macos"))]
        let handle_wrapper = NativeHandleWrapper { _dummy: () };

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

// ---------------------------------------------------------------------------
// Platform: macOS (Cocoa)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn create_native_transparent_window(x: i32, y: i32, width: u32, height: u32) -> Option<RawHwnd> {
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

        Some(content_view as RawHwnd)
    }
}

#[cfg(not(target_os = "macos"))]
fn create_native_transparent_window(
    _x: i32,
    _y: i32,
    _width: u32,
    _height: u32,
) -> Option<RawHwnd> {
    tracing::warn!("Transparent windows not yet implemented on this platform");
    None
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

#[cfg(not(target_os = "macos"))]
fn show_native_window(_hwnd: RawHwnd, _show: bool) {}

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

#[cfg(not(target_os = "macos"))]
fn set_native_window_frame(_hwnd: RawHwnd, _x: i32, _y: i32, _width: u32, _height: u32) {}

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

#[cfg(not(target_os = "macos"))]
fn close_native_window(_hwnd: RawHwnd) {}

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

#[cfg(not(target_os = "macos"))]
fn set_native_click_through(_hwnd: RawHwnd, _click_through: bool) {}
