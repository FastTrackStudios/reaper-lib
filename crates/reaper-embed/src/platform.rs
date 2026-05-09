//! Platform types shared across modules.

/// Window rectangle in screen coordinates.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl WindowRect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Returns the height of the main screen in points (macOS).
///
/// Used for coordinate flipping: SWELL's `ClientToScreen` returns bottom-left
/// origin coordinates, but NSWindow positioning uses top-left origin.
#[cfg(target_os = "macos")]
pub fn main_screen_height() -> f64 {
    use cocoa::base::id;
    use cocoa::foundation::NSRect;
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let screens: id = msg_send![class!(NSScreen), screens];
        if screens.is_null() {
            return 1080.0;
        }
        let count: usize = msg_send![screens, count];
        if count == 0 {
            return 1080.0;
        }
        let main_screen: id = msg_send![screens, objectAtIndex: 0usize];
        let frame: NSRect = msg_send![main_screen, frame];
        frame.size.height
    }
}

/// Returns the height of the main screen in pixels (Linux/X11).
#[cfg(target_os = "linux")]
pub fn main_screen_height() -> f64 {
    use crate::transparent::x11_display;
    let Some((xlib, display)) = x11_display() else {
        return 1080.0;
    };
    unsafe {
        let screen = (xlib.XDefaultScreen)(display);
        (xlib.XDisplayHeight)(display, screen) as f64
    }
}

/// Stub for unsupported platforms.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn main_screen_height() -> f64 {
    1080.0
}

// ---------------------------------------------------------------------------
// Display scale factor (HiDPI)
// ---------------------------------------------------------------------------

/// Returns the display scale factor for HiDPI support.
///
/// - macOS: queries `backingScaleFactor` from the main screen
/// - Linux: queries `Xft.dpi` from X resources (default 96 → scale 1.0)
/// - Other: returns 1.0
#[cfg(target_os = "macos")]
pub fn display_scale_factor() -> f64 {
    use cocoa::base::id;
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let screens: id = msg_send![class!(NSScreen), screens];
        if screens.is_null() {
            return 1.0;
        }
        let count: usize = msg_send![screens, count];
        if count == 0 {
            return 1.0;
        }
        let main_screen: id = msg_send![screens, objectAtIndex: 0usize];
        let scale: f64 = msg_send![main_screen, backingScaleFactor];
        if scale > 0.0 { scale } else { 1.0 }
    }
}

#[cfg(target_os = "linux")]
pub fn display_scale_factor() -> f64 {
    use crate::transparent::x11_display;

    let Some((xlib, display)) = x11_display() else {
        return 1.0;
    };

    unsafe {
        // Try Xft.dpi from X resources (set by desktop environments)
        let resource_manager = (xlib.XResourceManagerString)(display);
        if !resource_manager.is_null() {
            let rm_str = std::ffi::CStr::from_ptr(resource_manager).to_string_lossy();
            for line in rm_str.lines() {
                if let Some(dpi_str) = line.strip_prefix("Xft.dpi:") {
                    if let Ok(dpi) = dpi_str.trim().parse::<f64>() {
                        if dpi > 0.0 {
                            return dpi / 96.0;
                        }
                    }
                }
            }
        }

        // Fallback: compute from physical screen dimensions
        let screen = (xlib.XDefaultScreen)(display);
        let width_px = (xlib.XDisplayWidth)(display, screen) as f64;
        let width_mm = (xlib.XDisplayWidthMM)(display, screen) as f64;
        if width_mm > 0.0 {
            let dpi = width_px / (width_mm / 25.4);
            // Only use this if it suggests scaling (> 120 DPI)
            if dpi > 120.0 {
                return (dpi / 96.0 * 4.0).round() / 4.0; // Round to nearest 0.25
            }
        }

        1.0
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn display_scale_factor() -> f64 {
    1.0
}
