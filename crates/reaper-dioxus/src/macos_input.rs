//! macOS keyboard/IME input view.
//!
//! On macOS, SWELL HWNDs are NSViews but REAPER's firstResponder chain
//! doesn't reliably deliver keyDown: / insertText: events into a SWELL
//! wndproc. We mirror reaimgui's approach (cocoa_window.mm): add a
//! dedicated NSView subclass as a subview that becomes firstResponder
//! on click and forwards keyboard events directly into the Blitz
//! document via the dock module's event dispatcher.
//!
//! The view sits ON TOP of the SWELL HWND but has `isOpaque = NO` and
//! does no drawing — it only intercepts keyboard events.

#![cfg(target_os = "macos")]

use cocoa::base::{BOOL, NO, YES, id, nil};
use cocoa::foundation::{NSPoint, NSRect, NSSize, NSString, NSUInteger};
use objc::declare::ClassDecl;
use objc::rc::StrongPtr;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};
use reaper_low::raw;
use std::ffi::c_void;
use std::sync::Once;

static REGISTER_CLASS: Once = Once::new();
const PARENT_HWND_IVAR: &str = "_fts_parent_hwnd";

/// Register the `FTSDioxusInputView` NSView subclass with the Obj-C runtime.
/// Idempotent — safe to call repeatedly.
fn register_class() -> &'static Class {
    REGISTER_CLASS.call_once(|| {
        let superclass = class!(NSView);
        let mut decl = ClassDecl::new("FTSDioxusInputView", superclass)
            .expect("Failed to declare FTSDioxusInputView");

        // Ivar to store the SWELL parent HWND pointer.
        decl.add_ivar::<*mut c_void>(PARENT_HWND_IVAR);

        unsafe {
            decl.add_method(
                sel!(acceptsFirstResponder),
                accepts_first_responder as extern "C" fn(&Object, Sel) -> BOOL,
            );
            decl.add_method(
                sel!(canBecomeKeyView),
                can_become_key_view as extern "C" fn(&Object, Sel) -> BOOL,
            );
            decl.add_method(
                sel!(isOpaque),
                is_opaque as extern "C" fn(&Object, Sel) -> BOOL,
            );
            decl.add_method(
                sel!(keyDown:),
                key_down as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(keyUp:),
                key_up as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(insertText:replacementRange:),
                insert_text as extern "C" fn(&Object, Sel, id, NSRange),
            );
            decl.add_method(
                sel!(mouseDown:),
                mouse_down as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(mouseUp:),
                mouse_up as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(mouseDragged:),
                mouse_dragged as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(mouseMoved:),
                mouse_dragged as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(rightMouseDown:),
                right_mouse_down as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(rightMouseUp:),
                right_mouse_up as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(otherMouseDown:),
                other_mouse_down as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(otherMouseUp:),
                other_mouse_up as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(scrollWheel:),
                scroll_wheel as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(acceptsFirstMouse:),
                accepts_first_mouse as extern "C" fn(&Object, Sel, id) -> BOOL,
            );
        }

        decl.register();
    });

    Class::get("FTSDioxusInputView").expect("class must be registered")
}

#[repr(C)]
#[derive(Copy, Clone)]
struct NSRange {
    location: NSUInteger,
    length: NSUInteger,
}

extern "C" fn accepts_first_responder(_this: &Object, _sel: Sel) -> BOOL { YES }
extern "C" fn can_become_key_view(_this: &Object, _sel: Sel) -> BOOL { YES }
extern "C" fn is_opaque(_this: &Object, _sel: Sel) -> BOOL { NO }

fn parent_hwnd(this: &Object) -> raw::HWND {
    unsafe {
        let ptr: *mut c_void = *this.get_ivar(PARENT_HWND_IVAR);
        ptr as raw::HWND
    }
}

extern "C" fn key_down(this: &Object, _sel: Sel, event: id) {
    let hwnd = parent_hwnd(this);
    if hwnd.is_null() {
        return;
    }
    if let Some(ui_event) = ns_key_event_to_blitz(event, true) {
        crate::dock::forward_keyboard_event(hwnd, ui_event);
    }
    // Also deliver text via the input-method machinery so inserText: fires
    // when needed (dead keys, IME, etc.).
    unsafe {
        let array: id = msg_send![class!(NSArray), arrayWithObject: event];
        let _: () = msg_send![this, interpretKeyEvents: array];
    }
}

extern "C" fn key_up(this: &Object, _sel: Sel, event: id) {
    let hwnd = parent_hwnd(this);
    if hwnd.is_null() {
        return;
    }
    if let Some(ui_event) = ns_key_event_to_blitz(event, false) {
        crate::dock::forward_keyboard_event(hwnd, ui_event);
    }
}

extern "C" fn insert_text(this: &Object, _sel: Sel, text: id, _range: NSRange) {
    let hwnd = parent_hwnd(this);
    if hwnd.is_null() {
        return;
    }
    // `text` is NSString (for single-char commits) or NSAttributedString (IME).
    // We only need the plain UTF-8 bytes.
    unsafe {
        let is_attrib: BOOL = msg_send![text, isKindOfClass: class!(NSAttributedString)];
        let ns_str: id = if is_attrib == YES {
            msg_send![text, string]
        } else {
            text
        };
        let utf8_ptr: *const i8 = msg_send![ns_str, UTF8String];
        if !utf8_ptr.is_null() {
            let s = std::ffi::CStr::from_ptr(utf8_ptr)
                .to_string_lossy()
                .into_owned();
            let event = blitz_traits::events::UiEvent::Ime(
                blitz_traits::events::BlitzImeEvent::Commit(s),
            );
            crate::dock::forward_keyboard_event(hwnd, event);
        }
    }
}

/// Convert an NSEvent key event into a Blitz UiEvent. Returns None for
/// events we don't care about (modifier-only, etc.).
fn ns_key_event_to_blitz(event: id, pressed: bool) -> Option<blitz_traits::events::UiEvent> {
    use blitz_traits::events::{BlitzKeyEvent, KeyState, UiEvent};
    use keyboard_types::{Code, Key, Location, Modifiers};

    unsafe {
        let keycode: u16 = msg_send![event, keyCode];
        let modifiers_raw: NSUInteger = msg_send![event, modifierFlags];
        let is_repeat: BOOL = msg_send![event, isARepeat];
        let chars: id = msg_send![event, charactersIgnoringModifiers];

        let key = if chars.is_null() {
            Key::Unidentified
        } else {
            let utf8: *const i8 = msg_send![chars, UTF8String];
            if utf8.is_null() {
                Key::Unidentified
            } else {
                let s = std::ffi::CStr::from_ptr(utf8).to_string_lossy();
                mac_key_from_str(&s).unwrap_or_else(|| Key::Character(s.into_owned()))
            }
        };

        let code = mac_keycode_to_code(keycode);

        let mut mods = Modifiers::empty();
        if modifiers_raw & (1 << 17) != 0 { mods |= Modifiers::SHIFT; }   // NSEventModifierFlagShift
        if modifiers_raw & (1 << 18) != 0 { mods |= Modifiers::CONTROL; } // NSEventModifierFlagControl
        if modifiers_raw & (1 << 19) != 0 { mods |= Modifiers::ALT; }     // NSEventModifierFlagOption
        if modifiers_raw & (1 << 20) != 0 { mods |= Modifiers::META; }    // NSEventModifierFlagCommand

        Some(UiEvent::KeyDown(BlitzKeyEvent {
            key,
            code,
            modifiers: mods,
            location: Location::Standard,
            is_auto_repeating: is_repeat == YES,
            is_composing: false,
            state: if pressed { KeyState::Pressed } else { KeyState::Released },
            text: None,
        }))
        .map(|e| {
            // We built KeyDown above; convert to KeyUp for key_up().
            if pressed { e } else if let UiEvent::KeyDown(k) = e { UiEvent::KeyUp(k) } else { e }
        })
    }
}

// ── Mouse forwarding ────────────────────────────────────────────────────

extern "C" fn accepts_first_mouse(_this: &Object, _sel: Sel, _event: id) -> BOOL { YES }

fn mouse_location_in_view(this: &Object, event: id) -> Option<(f32, f32, f32, f32)> {
    unsafe {
        let window_loc: NSPoint = msg_send![event, locationInWindow];
        let local: NSPoint = msg_send![this, convertPoint:window_loc fromView:nil];
        let bounds: NSRect = msg_send![this, bounds];
        // Cocoa Y grows up; Blitz/DOM Y grows down. Flip.
        let x = local.x as f32;
        let y = (bounds.size.height - local.y) as f32;
        let w = bounds.size.width as f32;
        let h = bounds.size.height as f32;
        Some((x, y, w, h))
    }
}

fn ns_event_modifiers(event: id) -> keyboard_types::Modifiers {
    use keyboard_types::Modifiers;
    let mut mods = Modifiers::empty();
    unsafe {
        let flags: NSUInteger = msg_send![event, modifierFlags];
        if flags & (1 << 17) != 0 { mods |= Modifiers::SHIFT; }
        if flags & (1 << 18) != 0 { mods |= Modifiers::CONTROL; }
        if flags & (1 << 19) != 0 { mods |= Modifiers::ALT; }
        if flags & (1 << 20) != 0 { mods |= Modifiers::META; }
    }
    mods
}

fn forward_button(
    this: &Object,
    event: id,
    button: blitz_traits::events::MouseEventButton,
    down: bool,
) {
    let hwnd = parent_hwnd(this);
    if hwnd.is_null() { return; }
    let Some((x, y, _w, _h)) = mouse_location_in_view(this, event) else { return };
    let mods = ns_event_modifiers(event);
    let buttons = if down {
        blitz_traits::events::MouseEventButtons::Primary
    } else {
        blitz_traits::events::MouseEventButtons::empty()
    };
    let ev = blitz_traits::events::BlitzMouseButtonEvent {
        x, y, button, buttons, mods,
    };
    let ui_event = if down {
        blitz_traits::events::UiEvent::MouseDown(ev)
    } else {
        blitz_traits::events::UiEvent::MouseUp(ev)
    };
    crate::dock::forward_keyboard_event(hwnd, ui_event);
}

extern "C" fn mouse_down(this: &Object, _sel: Sel, event: id) {
    // First click → become firstResponder (so keyDown: works).
    unsafe {
        let window: id = msg_send![this, window];
        if !window.is_null() {
            let _: () = msg_send![window, makeFirstResponder: this];
        }
    }
    forward_button(this, event, blitz_traits::events::MouseEventButton::Main, true);
}

extern "C" fn mouse_up(this: &Object, _sel: Sel, event: id) {
    forward_button(this, event, blitz_traits::events::MouseEventButton::Main, false);
}

extern "C" fn right_mouse_down(this: &Object, _sel: Sel, event: id) {
    forward_button(this, event, blitz_traits::events::MouseEventButton::Secondary, true);
}

extern "C" fn right_mouse_up(this: &Object, _sel: Sel, event: id) {
    forward_button(this, event, blitz_traits::events::MouseEventButton::Secondary, false);
}

extern "C" fn other_mouse_down(this: &Object, _sel: Sel, event: id) {
    forward_button(this, event, blitz_traits::events::MouseEventButton::Auxiliary, true);
}

extern "C" fn other_mouse_up(this: &Object, _sel: Sel, event: id) {
    forward_button(this, event, blitz_traits::events::MouseEventButton::Auxiliary, false);
}

extern "C" fn mouse_dragged(this: &Object, _sel: Sel, event: id) {
    let hwnd = parent_hwnd(this);
    if hwnd.is_null() { return; }
    let Some((x, y, _, _)) = mouse_location_in_view(this, event) else { return };
    let mods = ns_event_modifiers(event);
    let ev = blitz_traits::events::BlitzMouseButtonEvent {
        x, y,
        button: blitz_traits::events::MouseEventButton::Main,
        buttons: blitz_traits::events::MouseEventButtons::Primary,
        mods,
    };
    crate::dock::forward_keyboard_event(
        hwnd,
        blitz_traits::events::UiEvent::MouseMove(ev),
    );
}

extern "C" fn scroll_wheel(this: &Object, _sel: Sel, event: id) {
    let hwnd = parent_hwnd(this);
    if hwnd.is_null() { return; }
    let Some((x, y, _, _)) = mouse_location_in_view(this, event) else { return };
    let mods = ns_event_modifiers(event);
    unsafe {
        let dx: f64 = msg_send![event, scrollingDeltaX];
        let dy: f64 = msg_send![event, scrollingDeltaY];
        let has_precise: BOOL = msg_send![event, hasPreciseScrollingDeltas];
        let (mult_x, mult_y) = if has_precise == YES { (1.0, 1.0) } else { (16.0, 16.0) };
        let ev = blitz_traits::events::BlitzWheelEvent {
            delta: blitz_traits::events::BlitzWheelDelta::Pixels(
                dx * mult_x,
                dy * mult_y,
            ),
            x, y,
            button: blitz_traits::events::MouseEventButton::Main,
            buttons: blitz_traits::events::MouseEventButtons::empty(),
            mods,
        };
        crate::dock::forward_keyboard_event(
            hwnd,
            blitz_traits::events::UiEvent::Wheel(ev),
        );
    }
}

fn mac_key_from_str(s: &str) -> Option<keyboard_types::Key> {
    use keyboard_types::Key;
    match s {
        "\u{7F}" | "\u{8}" => Some(Key::Backspace),
        "\t" => Some(Key::Tab),
        "\r" | "\n" => Some(Key::Enter),
        "\u{1B}" => Some(Key::Escape),
        _ => None,
    }
}

fn mac_keycode_to_code(kc: u16) -> keyboard_types::Code {
    use keyboard_types::Code;
    match kc {
        0 => Code::KeyA,
        1 => Code::KeyS,
        2 => Code::KeyD,
        3 => Code::KeyF,
        4 => Code::KeyH,
        5 => Code::KeyG,
        6 => Code::KeyZ,
        7 => Code::KeyX,
        8 => Code::KeyC,
        9 => Code::KeyV,
        11 => Code::KeyB,
        12 => Code::KeyQ,
        13 => Code::KeyW,
        14 => Code::KeyE,
        15 => Code::KeyR,
        16 => Code::KeyY,
        17 => Code::KeyT,
        32 => Code::KeyU,
        34 => Code::KeyI,
        35 => Code::KeyP,
        37 => Code::KeyL,
        38 => Code::KeyJ,
        40 => Code::KeyK,
        45 => Code::KeyN,
        46 => Code::KeyM,
        31 => Code::KeyO,
        51 => Code::Backspace,
        48 => Code::Tab,
        36 => Code::Enter,
        49 => Code::Space,
        53 => Code::Escape,
        123 => Code::ArrowLeft,
        124 => Code::ArrowRight,
        125 => Code::ArrowDown,
        126 => Code::ArrowUp,
        _ => Code::Unidentified,
    }
}

/// Create an `FTSDioxusInputView` covering the SWELL HWND's client area and
/// add it as a subview so it becomes the firstResponder for key events.
///
/// Returns the retained NSView. Caller must release (or store in LivePanel).
pub fn attach_to_panel(parent_hwnd: raw::HWND) -> Option<StrongPtr> {
    if parent_hwnd.is_null() {
        return None;
    }
    let cls = register_class();

    unsafe {
        // On macOS SWELL, an HWND *is* an NSView.
        let parent_view: id = parent_hwnd as id;
        if parent_view.is_null() {
            return None;
        }

        let bounds: NSRect = msg_send![parent_view, bounds];
        let alloc: id = msg_send![cls, alloc];
        let input_view: id = msg_send![alloc, initWithFrame: bounds];
        if input_view.is_null() {
            return None;
        }

        (*input_view).set_ivar(PARENT_HWND_IVAR, parent_hwnd as *mut c_void);

        // Fill the parent so hit testing covers the whole panel.
        // autoresizingMask = NSViewWidthSizable | NSViewHeightSizable = 2 | 16
        let _: () = msg_send![input_view, setAutoresizingMask: 18u64];

        let _: () = msg_send![parent_view, addSubview: input_view];

        // Make it first responder so it receives keys immediately.
        let window: id = msg_send![parent_view, window];
        if !window.is_null() {
            let _: () = msg_send![window, makeFirstResponder: input_view];
        }

        Some(StrongPtr::new(input_view))
    }
}
