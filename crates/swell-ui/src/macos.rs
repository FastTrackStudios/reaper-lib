//! The few AppKit messages swell-ui sends, untyped through objc2 0.6.
//!
//! A SWELL `HWND` on macOS is an `NSView`. Every function takes one and
//! must run on the main thread (AppKit's rule, which SWELL callers keep).

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{class, msg_send};

/// `view` as an Objective-C object.
///
/// # Safety
///
/// `view` is a live `NSView` (a SWELL `HWND`).
unsafe fn object<'a>(view: *const AnyObject) -> &'a AnyObject {
    unsafe { &*view }
}

fn is_kind_of(object: &AnyObject, class: &AnyClass) -> bool {
    unsafe { msg_send![object, isKindOfClass: class] }
}

/// The content view of the first child window of `view`'s window.
///
/// # Safety
///
/// `view` is a live `NSView`, on the main thread.
pub(crate) unsafe fn first_child_window_content_view(
    view: *const AnyObject,
) -> Option<Retained<AnyObject>> {
    let view = unsafe { object(view) };
    let window: Option<Retained<AnyObject>> = unsafe { msg_send![view, window] };
    let children: Option<Retained<AnyObject>> = unsafe { msg_send![&*window?, childWindows] };
    let child: Option<Retained<AnyObject>> = unsafe { msg_send![&*children?, firstObject] };
    unsafe { msg_send![&*child?, contentView] }
}

/// Whether `view` is a text field or text view, which handle their own keys.
///
/// # Safety
///
/// `view` is a live `NSView`, on the main thread.
pub(crate) unsafe fn is_text_input(view: *const AnyObject) -> bool {
    let view = unsafe { object(view) };
    is_kind_of(view, class!(NSTextField)) || is_kind_of(view, class!(NSTextView))
}

/// Sends the application's current event to `view`'s window, if there are
/// both.
///
/// # Safety
///
/// `view` is a live `NSView`, on the main thread.
pub(crate) unsafe fn send_current_event_to_window(view: *const AnyObject) {
    let view = unsafe { object(view) };
    let app: Option<Retained<AnyObject>> =
        unsafe { msg_send![class!(NSApplication), sharedApplication] };
    let Some(app) = app else { return };
    let event: Option<Retained<AnyObject>> = unsafe { msg_send![&*app, currentEvent] };
    let window: Option<Retained<AnyObject>> = unsafe { msg_send![view, window] };
    if let (Some(event), Some(window)) = (event, window) {
        let _: () = unsafe { msg_send![&*window, sendEvent: &*event] };
    }
}
