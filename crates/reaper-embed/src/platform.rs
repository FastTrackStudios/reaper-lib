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

/// Stub for non-macOS platforms.
#[cfg(not(target_os = "macos"))]
pub fn main_screen_height() -> f64 {
    1080.0
}
