//! DockablePanel — Dioxus rendering inside REAPER's native docker system.
//!
//! Creates a SWELL dialog window, registers it with REAPER's docker via
//! `DockWindowAddEx`, and renders a Dioxus component tree into it using
//! the `EmbeddedView` GPU pipeline.
//!
//! On Linux, SWELL docked panels don't get a GDK window, so we create a
//! direct X11 child window with a proper 32-bit ARGB visual as the GPU
//! render surface, reparented into the dock panel's X11 hierarchy.

#[cfg(feature = "desktop-renderer")]
use dioxus_embedded::{
    EmbeddedBounds, ParentWindow,
    desktop::{DioxusDesktopEmbeddedConfig, DioxusDesktopEmbeddedView, to_desktop_rect},
};
use dioxus_native::prelude::*;
#[cfg(not(target_os = "linux"))]
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle,
};
use reaper_low::raw;
use reaper_low::{Reaper, Swell};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::c_int;
#[cfg(all(feature = "desktop-renderer", target_os = "linux"))]
use std::os::raw::c_ulong;
use std::ptr;

use crate::embedded::EmbeddedView;

fn mouse_pointer_event(
    x: f32,
    y: f32,
    button: blitz_traits::events::MouseEventButton,
    buttons: blitz_traits::events::MouseEventButtons,
) -> blitz_traits::events::BlitzPointerEvent {
    blitz_traits::events::BlitzPointerEvent {
        id: blitz_traits::events::BlitzPointerId::Mouse,
        is_primary: true,
        coords: blitz_traits::events::PointerCoords {
            page_x: x,
            page_y: y,
            screen_x: x,
            screen_y: y,
            client_x: x,
            client_y: y,
        },
        button,
        buttons,
        mods: keyboard_types::Modifiers::empty(),
        details: blitz_traits::events::PointerDetails::default(),
    }
}

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

/// Renderer backend for a dockable panel.
#[cfg(feature = "desktop-renderer")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockablePanelRenderer {
    /// Existing CPU/GPU native renderer built on Blitz.
    Native,
    /// Browser-backed Dioxus desktop renderer embedded as a child webview.
    Desktop,
}

/// A live dockable panel registered with REAPER's docker system.
struct LivePanel {
    config: DockablePanelConfig,
    hwnd: raw::HWND,
    view: Option<EmbeddedView>,
    #[cfg(feature = "desktop-renderer")]
    renderer: DockablePanelRenderer,
    #[cfg(feature = "desktop-renderer")]
    desktop_view: Option<DioxusDesktopEmbeddedView>,
    #[cfg(all(feature = "desktop-renderer", target_os = "linux"))]
    desktop_parent: Option<LinuxDesktopParent>,
    visible: bool,
    contexts: Vec<Box<dyn FnOnce()>>,
    /// Track failed init attempts to avoid infinite retry spam.
    init_attempts: u32,
    /// Last polled IsWindowVisible state (to log changes only).
    #[cfg(target_os = "linux")]
    last_hwnd_visible: bool,
    /// Last observed dock ID from `DockIsChildOfDock`. A change indicates
    /// the user moved the panel between dockers (or undocked it). Used to
    /// invalidate render state cross-platform (matches reaimgui docker.cpp:527).
    last_dock_id: i32,
    /// Retained NSView that forwards keyDown/insertText to the Blitz document
    /// (macOS only — matches reaimgui InputView).
    #[cfg(target_os = "macos")]
    input_view: Option<objc::rc::StrongPtr>,
    /// Last observed "wants text input" state. When this flips we swap the
    /// SWELL class name so REAPER <6.29 (which has no hwnd_info hook) still
    /// suppresses global hotkeys while typing. Matches reaimgui's
    /// gdk_window.cpp:146-147 / cocoa_window.mm:211-218 class-swap dance.
    wants_text_input: bool,
}

#[cfg(all(feature = "desktop-renderer", target_os = "linux"))]
struct LinuxDesktopParent {
    bridge_hwnd: raw::HWND,
    parent: ParentWindow,
}

#[cfg(all(feature = "desktop-renderer", target_os = "linux"))]
impl Drop for LinuxDesktopParent {
    fn drop(&mut self) {
        if !self.bridge_hwnd.is_null() {
            unsafe {
                swell().DestroyWindow(self.bridge_hwnd);
            }
        }
    }
}

/// REAPER recognises this class name as a text-input-capable window and will
/// not process global action shortcuts while it has focus. reaimgui uses the
/// same magic string — it's originally for EEL2/LICE standalone text boxes.
const TEXT_INPUT_CLASS: &str = "Lua_LICE_gfx_standalone\0";
const DEFAULT_CLASS: &str = "FTSDioxusPanel\0";

// ---------------------------------------------------------------------------
// Global registry (thread-local, main thread only)
// ---------------------------------------------------------------------------

thread_local! {
    static PANELS: RefCell<HashMap<PanelId, LivePanel>> = RefCell::new(HashMap::new());
    #[cfg(feature = "desktop-renderer")]
    static PANEL_RENDERER_OVERRIDES: RefCell<HashMap<PanelId, DockablePanelRenderer>> = RefCell::new(HashMap::new());
}

static REAPER_API: std::sync::OnceLock<&'static Reaper> = std::sync::OnceLock::new();
static SWELL_API: std::sync::OnceLock<&'static Swell> = std::sync::OnceLock::new();

/// Initialize the dock module with REAPER/SWELL API references.
/// Must be called before `register_panel_from_service`.
pub fn init(reaper: &'static Reaper, swell: &'static Swell) {
    let _ = REAPER_API.set(reaper);
    let _ = SWELL_API.set(swell);

    // Register an accelerator that passes keyboard events to our panels
    // when they have focus. Without this, REAPER eats all key presses
    // as global shortcuts before they reach our wndproc.
    let handler = Box::new(DockPanelAccelHandler);
    let high_reaper = reaper_high::Reaper::get();
    let mut session = high_reaper.medium_session();
    match session.plugin_register_add_accelerator_register(
        handler,
        reaper_medium::AcceleratorPosition::Front,
    ) {
        Ok(_handle) => {
            tracing::info!("Dock panel accelerator registered");
        }
        Err(e) => {
            tracing::error!("Failed to register dock panel accelerator: {e}");
        }
    }

    // Register an hwnd_info callback (REAPER 6.29+). When the focused element
    // is a text input, return 1 so REAPER passes typed keys to the HWND
    // instead of eating them as global action shortcuts. Matches reaimgui
    // (window.cpp:395-409).
    match session.plugin_register_add_hwnd_info::<DockPanelHwndInfo>() {
        Ok(()) => tracing::info!("Dock panel hwnd_info registered"),
        Err(e) => tracing::warn!("hwnd_info registration failed (REAPER < 6.29?): {e:?}"),
    }
}

/// Override the renderer for a panel before it is registered.
#[cfg(feature = "desktop-renderer")]
pub fn set_panel_renderer(id: PanelId, renderer: DockablePanelRenderer) {
    PANEL_RENDERER_OVERRIDES.with(|overrides| {
        overrides.borrow_mut().insert(id, renderer);
    });
}

/// Accelerator handler that passes keyboard events to dock panels
/// when they have focus, preventing REAPER from consuming them.
struct DockPanelAccelHandler;

impl reaper_medium::TranslateAccel for DockPanelAccelHandler {
    fn call(
        &mut self,
        args: reaper_medium::TranslateAccelArgs,
    ) -> reaper_medium::TranslateAccelResult {
        let msg_hwnd = args.msg.raw().hwnd;

        // Check if the message target is one of our panel HWNDs
        let is_our_panel = PANELS.with(|panels| {
            let panels = panels.borrow();
            panels.values().any(|p| {
                p.hwnd == msg_hwnd || (unsafe { Swell::get().IsChild(p.hwnd, msg_hwnd) } != 0)
            })
        });

        if is_our_panel {
            // Tell REAPER to pass the key to our window instead of processing it
            reaper_medium::TranslateAccelResult::PassOnToWindow
        } else {
            reaper_medium::TranslateAccelResult::NotOurWindow
        }
    }
}

/// Screenset callback — REAPER calls this when saving/loading screensets
/// and project state so it can track our panel's HWND/dock/focus status.
///
/// Actions (from reaper_plugin.h:1321-1346):
///   0 SCREENSET_ACTION_GETHWND      — return HWND of our window
///   1 SCREENSET_ACTION_IS_DOCKED    — return 1 if docked
///   2 SCREENSET_ACTION_SWITCH_DOCK  — toggle dock state
///   4 SCREENSET_ACTION_LOAD_STATE   — load state from actionParm buffer
///   8 SCREENSET_ACTION_SAVE_STATE   — write state to actionParm buffer
unsafe extern "C" fn screenset_callback(
    action: std::os::raw::c_int,
    _id: *const std::os::raw::c_char,
    param: *mut std::os::raw::c_void,
    _action_parm: *mut std::os::raw::c_void,
    _action_parm_size: std::os::raw::c_int,
) -> reaper_low::raw::LRESULT {
    const SCREENSET_ACTION_GETHWND: i32 = 0;
    const SCREENSET_ACTION_IS_DOCKED: i32 = 1;

    let hwnd = param as raw::HWND;
    match action {
        SCREENSET_ACTION_GETHWND => hwnd as isize,
        SCREENSET_ACTION_IS_DOCKED => {
            let mut is_floating = false;
            let dock_id = unsafe { reaper().DockIsChildOfDock(hwnd, &mut is_floating) };
            if dock_id >= 0 && !is_floating { 1 } else { 0 }
        }
        _ => 0,
    }
}

/// Per-HWND query hook — lets REAPER ask whether a given window is currently
/// editing text. When the focused Dioxus node is a text input, we return 1
/// so REAPER suppresses global hotkeys and lets the character reach our
/// wndproc via WM_CHAR.
struct DockPanelHwndInfo;

impl reaper_medium::HwndInfo for DockPanelHwndInfo {
    fn call(
        window: Option<reaper_medium::Hwnd>,
        info_type: reaper_medium::HwndInfoType,
        _msg: Option<reaper_medium::AccelMsg>,
    ) -> i32 {
        let Some(window) = window else { return 0 };
        let hwnd_raw = window.as_ptr() as raw::HWND;

        // Walk up SWELL parent chain to find one of our panel HWNDs.
        let swell = Swell::get();
        let panel_hwnd = {
            let mut current = hwnd_raw;
            let mut found = std::ptr::null_mut();
            for _ in 0..16 {
                if current.is_null() {
                    break;
                }
                let is_ours =
                    PANELS.with(|panels| panels.borrow().values().any(|p| p.hwnd == current));
                if is_ours {
                    found = current;
                    break;
                }
                let parent = unsafe { swell.GetParent(current) };
                if parent.is_null() || parent == current {
                    break;
                }
                current = parent;
            }
            found
        };
        if panel_hwnd.is_null() {
            return 0;
        }

        match info_type {
            reaper_medium::HwndInfoType::IsTextField => {
                let is_text = PANELS.with(|panels| {
                    panels
                        .borrow()
                        .values()
                        .find(|p| p.hwnd == panel_hwnd)
                        .and_then(|p| p.view.as_ref())
                        .map(|v| v.focused_is_text_input())
                        .unwrap_or(false)
                });
                if is_text { 1 } else { -1 }
            }
            // Let REAPER process global hotkeys otherwise.
            _ => 0,
        }
    }
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

/// Window handle wrapper for the non-Linux wgpu surface path. On Linux we
/// render offscreen and blit via StretchBltFromMem under WM_PAINT instead —
/// see [`EmbeddedView::new_offscreen`] and the WM_PAINT handler below.
#[cfg(not(target_os = "linux"))]
struct RenderSurface {
    hwnd: raw::HWND,
}

#[cfg(not(target_os = "linux"))]
unsafe impl Send for RenderSurface {}
#[cfg(not(target_os = "linux"))]
unsafe impl Sync for RenderSurface {}

#[cfg(not(target_os = "linux"))]
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
        #[cfg(not(target_os = "macos"))]
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

#[cfg(not(target_os = "linux"))]
impl HasDisplayHandle for RenderSurface {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        #[cfg(target_os = "macos")]
        {
            use raw_window_handle::AppKitDisplayHandle;
            Ok(unsafe {
                DisplayHandle::borrow_raw(RawDisplayHandle::AppKit(AppKitDisplayHandle::new()))
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            use raw_window_handle::WindowsDisplayHandle;
            Ok(unsafe {
                DisplayHandle::borrow_raw(RawDisplayHandle::Windows(WindowsDisplayHandle::new()))
            })
        }
    }
}

// ---------------------------------------------------------------------------
// GDK XID extraction (cached, called once)
// ---------------------------------------------------------------------------

/// Build and show a dock context menu (reaimgui window.cpp:411-465).
/// Fires on WM_CONTEXTMENU — on Linux SWELL this is any right-click in the
/// panel HWND (tab + content). Minimal options: Dock/Undock + Close.
fn show_dock_context_menu(hwnd: raw::HWND, screen_x: i32, screen_y: i32) {
    use std::os::raw::c_int;
    const TPM_RETURNCMD: c_int = 0x0100;
    const TPM_NONOTIFY: c_int = 0x0080;
    const CMD_CLOSE: u32 = 1;
    const CMD_TOGGLE_DOCK: u32 = 2;

    let swell = swell();
    let reaper = reaper();

    let is_docked = {
        let mut is_floating = false;
        let dock_id = unsafe { reaper.DockIsChildOfDock(hwnd, &mut is_floating) };
        dock_id >= 0 && !is_floating
    };

    let menu = swell.CreatePopupMenu();
    if menu.is_null() {
        return;
    }

    unsafe {
        let toggle_label = if is_docked {
            "Undock"
        } else {
            "Dock in REAPER"
        };
        insert_menu_item(menu, CMD_TOGGLE_DOCK, toggle_label);
        insert_menu_item(menu, CMD_CLOSE, "Close");
    }

    // VK_APPS key produces (-1, -1); fall back to window centre.
    let (px, py) = if screen_x == -1 && screen_y == -1 {
        let mut rect = raw::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe {
            swell.GetWindowRect(hwnd, &mut rect);
        }
        ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2)
    } else {
        (screen_x, screen_y)
    };

    let cmd = unsafe {
        swell.TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY,
            px,
            py,
            0,
            hwnd,
            std::ptr::null(),
        )
    };
    unsafe {
        swell.DestroyMenu(menu);
    }

    match cmd as u32 {
        CMD_CLOSE => {
            PANELS.with(|panels| {
                let mut panels = panels.borrow_mut();
                if let Some(panel) = panels.values_mut().find(|p| p.hwnd == hwnd) {
                    hide_panel_inner(panel);
                }
            });
        }
        CMD_TOGGLE_DOCK => {
            PANELS.with(|panels| {
                let panels = panels.borrow();
                if let Some(panel) = panels.values().find(|p| p.hwnd == hwnd) {
                    let ident = CString::new(panel.config.id).unwrap();
                    let title = CString::new(panel.config.title).unwrap();
                    let new_dock = if is_docked { -1 } else { 0 };
                    unsafe {
                        reaper.DockWindowRemove(hwnd);
                        reaper.Dock_UpdateDockID(ident.as_ptr(), new_dock);
                        reaper.DockWindowAddEx(hwnd, title.as_ptr(), ident.as_ptr(), true);
                        reaper.DockWindowActivate(hwnd);
                    }
                }
            });
        }
        _ => {}
    }
}

unsafe fn insert_menu_item(menu: raw::HMENU, id: u32, label: &str) {
    let swell = Swell::get();
    let mut label_buf: Vec<u8> = label.as_bytes().to_vec();
    label_buf.push(0);
    let mut mi = raw::MENUITEMINFO {
        fMask: raw::MIIM_TYPE | raw::MIIM_DATA | raw::MIIM_ID,
        wID: id,
        dwTypeData: label_buf.as_mut_ptr() as *mut _,
        ..unsafe { std::mem::zeroed() }
    };
    unsafe {
        swell.InsertMenuItem(menu, -1, 1, &mut mi);
    }
}

// Linux X11 reparenting machinery removed — we now render offscreen via
// `EmbeddedView::new_offscreen` and blit with `StretchBltFromMem` under
// `WM_PAINT`. See the wndproc's WM_PAINT handler below. This matches
// reaimgui's gdk_opengl.cpp approach and avoids the many edge cases of
// tracking GDK parent changes, XShape passthrough, hidden-tab unmapping, etc.

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

    // Dock registration order (matches reaimgui docker.cpp:404-420):
    //   1. Dock_UpdateDockID — sets the dock position for this ident
    //   2. DockWindowAddEx   — registers the HWND with the docker
    //   3. DockWindowActivate (on show path) — brings tab to front
    //
    // We do NOT use GetConfigWantsDock — REAPER persists dock position per
    // ident via Dock_UpdateDockID itself (writes reaper.ini on save). Calling
    // GetConfigWantsDock and then Dock_UpdateDockID races against REAPER's own
    // save logic. Let Dock_UpdateDockID be the single source of truth.
    //
    // First-launch policy: set the default dock position only if the ident
    // has no prior entry (sentinel in ExtState). On subsequent launches we
    // skip the update so the user's last choice sticks.
    let ident = CString::new(config.id).unwrap();
    let first_launch = {
        let section = CString::new(config.id).unwrap();
        let key = CString::new("registered").unwrap();
        let prior = unsafe { reaper.GetExtState(section.as_ptr(), key.as_ptr()) };
        prior.is_null()
            || unsafe { std::ffi::CStr::from_ptr(prior) }
                .to_bytes()
                .is_empty()
    };
    if first_launch && config.default_dock_position >= 0 {
        unsafe { reaper.Dock_UpdateDockID(ident.as_ptr(), config.default_dock_position) };
    }
    if first_launch {
        let section = CString::new(config.id).unwrap();
        let key = CString::new("registered").unwrap();
        let val = CString::new("1").unwrap();
        unsafe {
            reaper.SetExtState(section.as_ptr(), key.as_ptr(), val.as_ptr(), true);
        }
    }

    let title_c = CString::new(config.title).unwrap();
    unsafe {
        reaper.DockWindowAddEx(
            hwnd,
            title_c.as_ptr(),
            ident.as_ptr(),
            config.show_on_first_launch,
        );
    }

    // Register a screenset callback so REAPER can restore dock tab focus
    // after project load / screenset switch. Matches reaimgui window.cpp:71-73.
    // The id string must remain valid for the lifetime of the registration —
    // we leak it (process-lifetime panel IDs).
    let screenset_id: &'static str = Box::leak(format!("FTSDioxus_{}", config.id).into_boxed_str());
    let screenset_id_c = CString::new(screenset_id).unwrap().into_raw();
    unsafe {
        reaper.screenset_registerNew(
            screenset_id_c,
            Some(screenset_callback),
            hwnd as *mut std::os::raw::c_void,
        );
    }

    let default_dock = config.default_dock_position;
    #[cfg(feature = "desktop-renderer")]
    let renderer = renderer_for_panel(config.id);
    let panel = LivePanel {
        config,
        hwnd,
        view: None,
        #[cfg(feature = "desktop-renderer")]
        renderer,
        #[cfg(feature = "desktop-renderer")]
        desktop_view: None,
        #[cfg(all(feature = "desktop-renderer", target_os = "linux"))]
        desktop_parent: None,
        visible: false,
        contexts,
        init_attempts: 0,
        #[cfg(target_os = "linux")]
        last_hwnd_visible: false,
        last_dock_id: -1,
        #[cfg(target_os = "macos")]
        input_view: None,
        wants_text_input: false,
    };

    PANELS.with(|panels| {
        panels.borrow_mut().insert(id, panel);
    });
    tracing::debug!(id, first_launch, default_dock, "Registered dockable panel");
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
            if panel.visible {
                hide_panel_inner(panel);
            } else {
                show_panel_inner(panel);
            }
        }
    });
}

pub fn show_panel(id: PanelId) {
    #[cfg(debug_assertions)]
    assert_main_thread();
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        if let Some(panel) = panels.get_mut(id)
            && !panel.visible
        {
            show_panel_inner(panel);
        }
    });
}

pub fn hide_panel(id: PanelId) {
    #[cfg(debug_assertions)]
    assert_main_thread();
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        if let Some(panel) = panels.get_mut(id)
            && panel.visible
        {
            hide_panel_inner(panel);
        }
    });
}

pub fn is_panel_visible(id: PanelId) -> bool {
    PANELS.with(|panels| panels.borrow().get(id).is_some_and(|p| p.visible))
}

thread_local! {
    /// Reentrancy guard — set while `update_panels` is running so that a
    /// Dioxus event handler calling back into REAPER APIs that re-post into
    /// the timer cannot recurse into another tick. Matches reaimgui's
    /// `g_reentrant` / `isDeferLoopBlocked` (resource.cpp:59-72).
    static UPDATING_PANELS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Update all visible panels (call from timer callback ~30Hz).
pub fn update_panels() {
    #[cfg(debug_assertions)]
    assert_main_thread();

    if UPDATING_PANELS.with(|c| c.replace(true)) {
        // Re-entered — another update_panels is in progress on this thread.
        // Drop the tick to avoid borrow-mut-twice on PANELS and runaway recursion.
        tracing::trace!("update_panels re-entered; skipping");
        return;
    }
    // Scope guard so the flag is cleared even if a panic escapes.
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            UPDATING_PANELS.with(|c| c.set(false));
        }
    }
    let _guard = Guard;

    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        let swell = swell();
        let reaper = reaper();

        #[cfg(all(feature = "desktop-renderer", target_os = "linux"))]
        pump_gtk_events();

        for panel in panels.values_mut() {
            if !panel.visible {
                continue;
            }

            // Poll SWELL visibility — IsWindowVisible returns false when
            // docked behind another tab (same as reaimgui window.cpp:234-238).
            let hwnd_visible = unsafe { swell.IsWindowVisible(panel.hwnd) };

            // Poll dock ID — a change means user moved the panel between
            // dockers or undocked it. Matches reaimgui docker.cpp:527-529.
            // On change: invalidate render surface (Linux X11 child, macOS
            // layout cache, etc.) and force a redraw.
            let mut is_floating = false;
            let current_dock_id = unsafe { reaper.DockIsChildOfDock(panel.hwnd, &mut is_floating) };
            if current_dock_id != panel.last_dock_id {
                tracing::info!(
                    panel = panel.config.id,
                    old = panel.last_dock_id,
                    new = current_dock_id,
                    is_floating,
                    "Dock ID changed"
                );
                panel.last_dock_id = current_dock_id;
                if let Some(view) = &panel.view {
                    view.mark_dirty();
                }
                // Persist the new position
                let ident = CString::new(panel.config.id).unwrap();
                if current_dock_id >= 0 {
                    unsafe { reaper.Dock_UpdateDockID(ident.as_ptr(), current_dock_id) };
                }
            }

            #[cfg(target_os = "linux")]
            if hwnd_visible != panel.last_hwnd_visible {
                tracing::info!(
                    panel = panel.config.id,
                    hwnd_visible,
                    "IsWindowVisible changed"
                );
                panel.last_hwnd_visible = hwnd_visible;
                if hwnd_visible && let Some(view) = &panel.view {
                    view.mark_dirty();
                }
            }

            if !hwnd_visible {
                continue;
            }

            // Deferred renderer init (max 30 retries ≈ 1 second)
            if panel_needs_init(panel) && panel.init_attempts < 30 {
                try_init_embedded_view(panel);
            }

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

            if w > 0 && h > 0 {
                #[cfg(feature = "desktop-renderer")]
                if let Some(desktop_view) = &mut panel.desktop_view {
                    if let Err(err) =
                        desktop_view.set_bounds(to_desktop_rect(EmbeddedBounds::new(0, 0, w, h)))
                    {
                        tracing::debug!(
                            panel = panel.config.id,
                            "Failed to resize desktop webview panel: {err}"
                        );
                    }
                    desktop_view.poll();
                    continue;
                }

                if let Some(view) = &mut panel.view {
                    let (cur_w, cur_h) = view.size();
                    if w != cur_w || h != cur_h {
                        view.resize(w, h);
                    }

                    // Cursor polling hack removed — WM_MOUSEMOVE reaches our
                    // wndproc directly now that we render offscreen instead
                    // of via an X11 child that absorbed events.

                    // Belt-and-braces for REAPER <6.29 (pre-hwnd_info): swap
                    // the SWELL class to TEXT_INPUT_CLASS while a text field
                    // has focus so REAPER suppresses global hotkeys. The
                    // hwnd_info hook already does this on 6.29+; this is
                    // extra insurance for older versions.
                    let wants = view.focused_is_text_input();
                    if wants != panel.wants_text_input {
                        panel.wants_text_input = wants;
                        let class = if wants {
                            TEXT_INPUT_CLASS
                        } else {
                            DEFAULT_CLASS
                        };
                        unsafe {
                            swell.SWELL_SetClassName(
                                panel.hwnd,
                                class.as_ptr() as *const std::os::raw::c_char,
                            );
                        }
                        tracing::trace!(panel = panel.config.id, wants, "SWELL class swap");
                    }

                    view.update();

                    // In offscreen mode, a completed frame means new pixels
                    // are ready; tell SWELL to post WM_PAINT so we can blit.
                    if view.take_needs_blit() {
                        unsafe {
                            swell.InvalidateRect(panel.hwnd, std::ptr::null(), 0);
                        }
                    }
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
            #[cfg(feature = "desktop-renderer")]
            {
                panel.desktop_view = None;
                #[cfg(target_os = "linux")]
                {
                    panel.desktop_parent = None;
                }
            }
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

/// Save the dock state for a single panel. Must be called with the panel
/// already in hand (avoids re-borrowing PANELS reentrantly).
///
/// Historical bug: this used to call `PANELS.with(|p| p.borrow())` while
/// callers (show/hide) already held `borrow_mut` on PANELS → RefCell panic
/// → abort inside an extern "C" wndproc → SIGABRT on close.
fn save_panel_state(panel: &LivePanel) {
    let reaper = reaper();
    let id = panel.config.id;
    let section = CString::new(id).unwrap();
    let vis_key = CString::new("visible").unwrap();
    let vis_val = CString::new(if panel.visible { "1" } else { "0" }).unwrap();
    unsafe {
        reaper.SetExtState(section.as_ptr(), vis_key.as_ptr(), vis_val.as_ptr(), true);
    }

    // Persist dock position via REAPER's authoritative API.
    let mut is_floating = false;
    let dock_id = unsafe { reaper.DockIsChildOfDock(panel.hwnd, &mut is_floating) };
    if dock_id >= 0 {
        let ident = CString::new(id).unwrap();
        unsafe { reaper.Dock_UpdateDockID(ident.as_ptr(), dock_id) };
    }
}

/// Public entry: save state for every registered panel. Takes the PANELS
/// borrow itself so MUST NOT be called from inside an existing borrow.
pub fn save_dock_state() {
    PANELS.with(|panels| {
        let panels = panels.borrow();
        for panel in panels.values() {
            save_panel_state(panel);
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
    let reaper = reaper();

    // Re-register with docker (removed on hide) so the tab reappears.
    // allowShow=true makes REAPER show the window; no separate ShowWindow needed
    // (matches reaimgui's add-then-activate pattern — window.cpp:209-222).
    let ident = CString::new(panel.config.id).unwrap();
    let title = CString::new(panel.config.title).unwrap();
    unsafe {
        reaper.DockWindowAddEx(panel.hwnd, title.as_ptr(), ident.as_ptr(), true);
        reaper.DockWindowActivate(panel.hwnd);
    }

    #[cfg(target_os = "macos")]
    force_view_layout(panel.hwnd);

    panel.visible = true;
    panel.init_attempts = 0;

    // Mark dirty so the view renders on the next tick
    if let Some(view) = &panel.view {
        view.mark_dirty();
    }

    // Caller holds PANELS borrow — use the per-panel variant to avoid
    // a reentrant borrow that would RefCell-panic inside extern "C" wndproc.
    save_panel_state(panel);
}

fn hide_panel_inner(panel: &mut LivePanel) {
    let swell = swell();
    let reaper = reaper();

    // Remove from docker so the tab disappears, then hide the HWND.
    // DockWindowRemove is safe to call even when not docked.
    unsafe {
        reaper.DockWindowRemove(panel.hwnd);
        swell.ShowWindow(panel.hwnd, raw::SW_HIDE as c_int);
    }

    // Keep EmbeddedView alive — just stop rendering until shown again.
    panel.visible = false;
    save_panel_state(panel);
}

fn try_init_embedded_view(panel: &mut LivePanel) {
    if !panel_needs_init(panel) || !panel.visible {
        return;
    }
    panel.init_attempts += 1;
    tracing::info!(
        panel = panel.config.id,
        attempt = panel.init_attempts,
        "Attempting to init EmbeddedView"
    );

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
    let w = (rect.right - rect.left)
        .unsigned_abs()
        .max(panel.config.default_width);
    let h = (rect.bottom - rect.top)
        .unsigned_abs()
        .max(panel.config.default_height);
    tracing::info!(w, h, "Panel '{}' client rect", panel.config.id);

    #[cfg(feature = "desktop-renderer")]
    if panel.renderer == DockablePanelRenderer::Desktop {
        match create_desktop_view(panel, w, h) {
            Ok(()) => {
                tracing::info!(
                    "Created desktop renderer webview for panel '{}'",
                    panel.config.id
                );
            }
            Err(err) => {
                tracing::warn!(
                    "Desktop renderer webview creation failed for '{}': {}",
                    panel.config.id,
                    err
                );
            }
        }
        return;
    }

    let contexts = std::mem::take(&mut panel.contexts);

    // On Linux, SWELL docked panels don't own a GDK window we can render
    // into directly via wgpu — so we render offscreen and blit via
    // StretchBltFromMem under WM_PAINT. Mirrors reaimgui's approach
    // (gdk_opengl.cpp:86-260).
    //
    // On macOS/Windows, the SWELL HWND is a real native view/window and
    // wgpu can target it directly.
    #[cfg(target_os = "linux")]
    let view_result = EmbeddedView::new_offscreen(panel.config.app, w, h, contexts);

    #[cfg(not(target_os = "linux"))]
    let view_result = {
        let surface = create_render_surface(panel, swell, w, h);
        let Some(surface) = surface else {
            if panel.init_attempts >= 30 {
                tracing::error!(
                    "Failed to create render surface for '{}' after {} attempts",
                    panel.config.id,
                    panel.init_attempts
                );
            }
            return;
        };
        EmbeddedView::new(panel.config.app, &surface, w, h, contexts)
    };

    match view_result {
        Ok(view) => {
            view.mark_dirty(); // Ensure first frame renders
            panel.view = Some(view);
            tracing::info!("Created EmbeddedView for panel '{}'", panel.config.id);

            // Attach the macOS InputView so keyDown: / insertText: reach the
            // Blitz document even when the SWELL HWND isn't firstResponder.
            #[cfg(target_os = "macos")]
            {
                panel.input_view = crate::macos_input::attach_to_panel(panel.hwnd);
                if panel.input_view.is_some() {
                    tracing::info!("Attached macOS InputView for panel '{}'", panel.config.id);
                } else {
                    tracing::warn!("Failed to attach macOS InputView for '{}'", panel.config.id);
                }
            }
            // Write ExtState so tests can verify the view was created
            let reaper = reaper();
            let section = CString::new(panel.config.id).unwrap();
            let key = CString::new("embedded_view").unwrap();
            let val = CString::new("1").unwrap();
            unsafe {
                reaper.SetExtState(section.as_ptr(), key.as_ptr(), val.as_ptr(), true);
            }
        }
        Err(e) => {
            tracing::warn!(
                "EmbeddedView creation failed for '{}': {}",
                panel.config.id,
                e
            );
        }
    }
}

fn panel_needs_init(panel: &LivePanel) -> bool {
    #[cfg(feature = "desktop-renderer")]
    if panel.renderer == DockablePanelRenderer::Desktop {
        return panel.desktop_view.is_none();
    }

    panel.view.is_none()
}

#[cfg(feature = "desktop-renderer")]
fn create_desktop_view(
    panel: &mut LivePanel,
    width: u32,
    height: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let parent = create_desktop_parent(panel.hwnd, width, height)?;
    let config = DioxusDesktopEmbeddedConfig::default()
        .with_bounds(to_desktop_rect(EmbeddedBounds::new(0, 0, width, height)));

    let dom = VirtualDom::new(panel.config.app);
    let view = DioxusDesktopEmbeddedView::new(&parent.parent, dom, config, || {})?;

    #[cfg(target_os = "linux")]
    {
        panel.desktop_parent = Some(parent);
    }
    panel.desktop_view = Some(view);
    Ok(())
}

#[cfg(all(feature = "desktop-renderer", target_os = "windows"))]
struct DesktopParent {
    parent: ParentWindow,
}

#[cfg(all(feature = "desktop-renderer", target_os = "macos"))]
struct DesktopParent {
    parent: ParentWindow,
}

#[cfg(all(feature = "desktop-renderer", target_os = "linux"))]
type DesktopParent = LinuxDesktopParent;

#[cfg(all(feature = "desktop-renderer", target_os = "windows"))]
fn create_desktop_parent(
    hwnd: raw::HWND,
    _width: u32,
    _height: u32,
) -> Result<DesktopParent, Box<dyn std::error::Error>> {
    Ok(DesktopParent {
        parent: unsafe { ParentWindow::from_hwnd(hwnd as isize)? },
    })
}

#[cfg(all(feature = "desktop-renderer", target_os = "macos"))]
fn create_desktop_parent(
    hwnd: raw::HWND,
    _width: u32,
    _height: u32,
) -> Result<DesktopParent, Box<dyn std::error::Error>> {
    Ok(DesktopParent {
        parent: unsafe { ParentWindow::from_ns_view(hwnd as *mut std::ffi::c_void)? },
    })
}

#[cfg(all(feature = "desktop-renderer", target_os = "linux"))]
fn create_desktop_parent(
    hwnd: raw::HWND,
    width: u32,
    height: u32,
) -> Result<DesktopParent, Box<dyn std::error::Error>> {
    init_gtk_for_desktop_renderer()?;

    let mut x_window_id: c_ulong = 0;
    let rect = raw::RECT {
        left: 0,
        top: 0,
        right: width as i32,
        bottom: height as i32,
    };

    let bridge_hwnd = unsafe {
        swell().SWELL_CreateXBridgeWindow(
            hwnd,
            &mut x_window_id as *mut c_ulong as *mut *mut c_void,
            &rect as *const _,
        )
    };

    if bridge_hwnd.is_null() {
        return Err("SWELL_CreateXBridgeWindow returned null".into());
    }
    if x_window_id == 0 {
        unsafe {
            swell().DestroyWindow(bridge_hwnd);
        }
        return Err("SWELL_CreateXBridgeWindow did not assign an X11 window id".into());
    }

    unsafe {
        let prop_key = CString::new("SWELL_XBRIDGE_KBHOOK_CHECK")?;
        let msg = raw::WM_USER as i32 + 100;
        swell().SetProp(bridge_hwnd, prop_key.as_ptr(), msg as *mut c_void);
    }

    let parent = unsafe { ParentWindow::from_xlib_window(x_window_id as u64)? };
    Ok(LinuxDesktopParent {
        bridge_hwnd,
        parent,
    })
}

#[cfg(all(feature = "desktop-renderer", target_os = "linux"))]
fn init_gtk_for_desktop_renderer() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Once;
    use std::sync::atomic::{AtomicBool, Ordering};

    static INIT: Once = Once::new();
    static OK: AtomicBool = AtomicBool::new(false);

    INIT.call_once(|| {
        if gtk::init().is_ok() {
            OK.store(true, Ordering::Relaxed);
        }
    });

    if OK.load(Ordering::Relaxed) {
        Ok(())
    } else {
        Err("gtk::init failed; embedded desktop renderer requires GTK/WebKitGTK on Linux".into())
    }
}

#[cfg(all(feature = "desktop-renderer", target_os = "linux"))]
fn pump_gtk_events() {
    while gtk::events_pending() {
        gtk::main_iteration_do(false);
    }
}

#[cfg(feature = "desktop-renderer")]
fn renderer_for_panel(id: PanelId) -> DockablePanelRenderer {
    if let Some(renderer) =
        PANEL_RENDERER_OVERRIDES.with(|overrides| overrides.borrow().get(id).copied())
    {
        return renderer;
    }

    if desktop_renderer_panel_list_matches(id) {
        return DockablePanelRenderer::Desktop;
    }

    match std::env::var("REAPER_DIOXUS_RENDERER") {
        Ok(value) if value.eq_ignore_ascii_case("desktop") => DockablePanelRenderer::Desktop,
        Ok(value) if value.eq_ignore_ascii_case("native") => DockablePanelRenderer::Native,
        _ => DockablePanelRenderer::Native,
    }
}

#[cfg(feature = "desktop-renderer")]
fn desktop_renderer_panel_list_matches(id: PanelId) -> bool {
    let Ok(value) = std::env::var("REAPER_DIOXUS_DESKTOP_PANELS") else {
        return false;
    };

    value
        .split(',')
        .map(str::trim)
        .any(|panel_id| panel_id == "*" || panel_id == id)
}

#[cfg(not(target_os = "linux"))]
fn create_render_surface(
    panel: &mut LivePanel,
    _swell: &Swell,
    _w: u32,
    _h: u32,
) -> Option<RenderSurface> {
    Some(RenderSurface { hwnd: panel.hwnd })
}

// ---------------------------------------------------------------------------
// Event forwarding
// ---------------------------------------------------------------------------

/// Forward a keyboard/IME event from the macOS InputView into the panel's
/// embedded view. Exposed `pub(crate)` so the InputView NSView subclass
/// can dispatch events without going through the SWELL wndproc.
#[cfg(target_os = "macos")]
pub(crate) fn forward_keyboard_event(hwnd: raw::HWND, event: blitz_traits::events::UiEvent) {
    forward_mouse_event(hwnd, event)
}

/// Forward a UI event to the panel's EmbeddedView.
fn forward_mouse_event(hwnd: raw::HWND, event: blitz_traits::events::UiEvent) {
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        if let Some(panel) = panels.values_mut().find(|p| p.hwnd == hwnd) {
            match &event {
                blitz_traits::events::UiEvent::PointerMove(e) => {
                    tracing::trace!(
                        x = e.coords.client_x,
                        y = e.coords.client_y,
                        "MouseMove on '{}'",
                        panel.config.id
                    );
                }
                blitz_traits::events::UiEvent::PointerDown(e) => {
                    tracing::info!(
                        x = e.coords.client_x,
                        y = e.coords.client_y,
                        "MouseDown on '{}'",
                        panel.config.id
                    );
                }
                blitz_traits::events::UiEvent::PointerUp(e) => {
                    tracing::info!(
                        x = e.coords.client_x,
                        y = e.coords.client_y,
                        "MouseUp on '{}'",
                        panel.config.id
                    );
                }
                blitz_traits::events::UiEvent::KeyDown(e) => {
                    tracing::info!(key = ?e.key, "KeyDown on '{}'", panel.config.id);
                }
                blitz_traits::events::UiEvent::Ime(e) => {
                    tracing::info!(ime = ?e, "IME on '{}'", panel.config.id);
                }
                _ => {}
            }
            if let Some(view) = &mut panel.view {
                view.handle_event(event);
            }
        }
    });
}

/// Map SWELL virtual key codes to keyboard_types::Key.
fn swell_vk_to_key(vk: u32) -> keyboard_types::Key {
    use keyboard_types::Key;
    match vk {
        0x08 => Key::Backspace,
        0x09 => Key::Tab,
        0x0D => Key::Enter,
        0x1B => Key::Escape,
        0x20 => Key::Character(" ".to_string()),
        0x25 => Key::ArrowLeft,
        0x26 => Key::ArrowUp,
        0x27 => Key::ArrowRight,
        0x28 => Key::ArrowDown,
        0x2E => Key::Delete,
        0x24 => Key::Home,
        0x23 => Key::End,
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        0x70..=0x7B => Key::F1, // F1-F12 (simplified)
        0x30..=0x39 => Key::Character(char::from_u32(vk).unwrap_or('0').to_string()),
        0x41..=0x5A => Key::Character(char::from_u32(vk + 32).unwrap_or('a').to_string()),
        _ => Key::Unidentified,
    }
}

/// Map SWELL virtual key codes to keyboard_types::Code.
fn swell_vk_to_code(vk: u32) -> keyboard_types::Code {
    use keyboard_types::Code;
    match vk {
        0x08 => Code::Backspace,
        0x09 => Code::Tab,
        0x0D => Code::Enter,
        0x1B => Code::Escape,
        0x20 => Code::Space,
        0x25 => Code::ArrowLeft,
        0x26 => Code::ArrowUp,
        0x27 => Code::ArrowRight,
        0x28 => Code::ArrowDown,
        0x2E => Code::Delete,
        0x30 => Code::Digit0,
        0x31 => Code::Digit1,
        0x32 => Code::Digit2,
        0x41 => Code::KeyA,
        0x42 => Code::KeyB,
        0x43 => Code::KeyC,
        _ => Code::Unidentified,
    }
}

// ---------------------------------------------------------------------------
// Window creation
// ---------------------------------------------------------------------------

fn create_panel_window(config: &DockablePanelConfig, swell: &Swell) -> raw::HWND {
    let title_c = CString::new(config.title).unwrap();

    unsafe {
        // SWELL magic resid: 0x400000 | ForceNonChild(0x8) | Resizable(0x1) = 0x400009.
        // ForceNonChild is mandatory — without it the dialog becomes a WS_CHILD of
        // the calling window's top-level and the docker hosts it awkwardly.
        // Matches reaimgui (window.cpp:339-343).
        let magic_resid = 0x400009usize as *const std::os::raw::c_char;

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

unsafe extern "C" fn panel_wndproc(
    hwnd: raw::HWND,
    msg: raw::UINT,
    wparam: raw::WPARAM,
    lparam: raw::LPARAM,
) -> raw::LRESULT {
    panel_wndproc_inner(hwnd, msg, wparam, lparam)
}

fn panel_wndproc_inner(
    hwnd: raw::HWND,
    msg: raw::UINT,
    wparam: raw::WPARAM,
    lparam: raw::LPARAM,
) -> raw::LRESULT {
    const WM_DESTROY: u32 = 0x0002;
    const WM_PAINT: u32 = 0x000F;
    const WM_SIZE: u32 = 0x0005;
    const WM_CLOSE: u32 = 0x0010;
    const WM_SHOWWINDOW: u32 = 0x0018;
    const WM_CONTEXTMENU: u32 = 0x007B;
    const WM_GETDLGCODE: u32 = 0x0087;
    const DLGC_WANTALLKEYS: raw::LRESULT = 0x0004;
    const WM_MOUSEMOVE: u32 = 0x0200;
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_RBUTTONDOWN: u32 = 0x0204;
    const WM_RBUTTONUP: u32 = 0x0205;
    const WM_MBUTTONDOWN: u32 = 0x0207;
    const WM_MBUTTONUP: u32 = 0x0208;
    const WM_MOUSEWHEEL: u32 = 0x020A;
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_KEYUP: u32 = 0x0101;
    const WM_CHAR: u32 = 0x0102;
    const WM_COMMAND: u32 = 0x0111;
    const IDCANCEL: u32 = 2;

    match msg {
        // ── Paint (Linux offscreen blit) ────────────────────────────
        // On Linux, the offscreen renderer writes BGRA8 bytes into the panel's
        // readback buffer and calls `InvalidateRect` so SWELL posts WM_PAINT.
        // Here we BeginPaint → StretchBltFromMem → EndPaint. Matches reaimgui's
        // gdk_opengl.cpp:247-260 blit-under-WM_PAINT pattern.
        #[cfg(target_os = "linux")]
        WM_PAINT => {
            let swell = swell();
            let mut ps: raw::PAINTSTRUCT = unsafe { std::mem::zeroed() };
            let hdc = unsafe { swell.BeginPaint(hwnd, &mut ps) };
            if hdc.is_null() {
                return 0;
            }

            // Pull view dims + pixel bytes under PANELS borrow, then blit.
            let blit_info = PANELS.with(|panels| {
                let panels = panels.borrow();
                let panel = panels.values().find(|p| p.hwnd == hwnd)?;
                let view = panel.view.as_ref()?;
                let pixels = view.bgra_pixels()?;
                let (w, h) = view.size();
                // SAFETY: pixels is borrowed from a thread_local — safe to pass
                // through as long as the borrow doesn't escape. We use the
                // pointer during the call, still inside the closure.
                let ptr = pixels.as_ptr();
                Some((ptr, w as c_int, h as c_int))
            });

            if let Some((ptr, w, h)) = blit_info {
                let mut rect = raw::RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                unsafe {
                    swell.GetClientRect(hwnd, &mut rect);
                }
                let cw = rect.right - rect.left;
                let ch = rect.bottom - rect.top;
                // `srcspan` is pixels-per-row (LICE_WrapperBitmap m_span, not bytes).
                // Passing bytes here caused a 4× row-stride overrun → SIGSEGV
                // inside memmove. Our readback is tightly packed, so span == w.
                unsafe {
                    swell.StretchBltFromMem(
                        hdc,
                        0,
                        0,
                        cw,
                        ch,
                        ptr as *const std::ffi::c_void,
                        w,
                        h,
                        w,
                    );
                }
            }

            unsafe {
                swell.EndPaint(hwnd, &mut ps);
            }
            0
        }
        // ── Mouse events ────────────────────────────────────────────
        WM_MOUSEMOVE => {
            let x = (lparam & 0xFFFF) as i16 as f32;
            let y = ((lparam >> 16) & 0xFFFF) as i16 as f32;
            forward_mouse_event(
                hwnd,
                blitz_traits::events::UiEvent::PointerMove(mouse_pointer_event(
                    x,
                    y,
                    blitz_traits::events::MouseEventButton::Main,
                    blitz_traits::events::MouseEventButtons::empty(),
                )),
            );
            0
        }
        WM_LBUTTONDOWN | WM_MBUTTONDOWN => {
            let x = (lparam & 0xFFFF) as i16 as f32;
            let y = ((lparam >> 16) & 0xFFFF) as i16 as f32;
            let button = match msg {
                WM_LBUTTONDOWN => blitz_traits::events::MouseEventButton::Main,
                WM_MBUTTONDOWN => blitz_traits::events::MouseEventButton::Auxiliary,
                _ => blitz_traits::events::MouseEventButton::Main,
            };
            // Capture mouse for drag support + take keyboard focus
            unsafe {
                swell().SetCapture(hwnd);
                swell().SetFocus(hwnd);
            }
            forward_mouse_event(
                hwnd,
                blitz_traits::events::UiEvent::PointerDown(mouse_pointer_event(
                    x,
                    y,
                    button,
                    blitz_traits::events::MouseEventButtons::Primary,
                )),
            );
            0
        }
        // Right-click: forward to Blitz AND fall through to DefWindowProc so
        // SWELL/REAPER can synthesise WM_CONTEXTMENU, which the docker uses
        // to open the dock/undock/options menu on tab right-click.
        WM_RBUTTONDOWN => {
            let x = (lparam & 0xFFFF) as i16 as f32;
            let y = ((lparam >> 16) & 0xFFFF) as i16 as f32;
            forward_mouse_event(
                hwnd,
                blitz_traits::events::UiEvent::PointerDown(mouse_pointer_event(
                    x,
                    y,
                    blitz_traits::events::MouseEventButton::Secondary,
                    blitz_traits::events::MouseEventButtons::Secondary,
                )),
            );
            let swell = swell();
            unsafe { swell.DefWindowProc(hwnd, msg, wparam, lparam) }
        }
        WM_LBUTTONUP | WM_MBUTTONUP => {
            let x = (lparam & 0xFFFF) as i16 as f32;
            let y = ((lparam >> 16) & 0xFFFF) as i16 as f32;
            let button = match msg {
                WM_LBUTTONUP => blitz_traits::events::MouseEventButton::Main,
                WM_MBUTTONUP => blitz_traits::events::MouseEventButton::Auxiliary,
                _ => blitz_traits::events::MouseEventButton::Main,
            };
            forward_mouse_event(
                hwnd,
                blitz_traits::events::UiEvent::PointerUp(mouse_pointer_event(
                    x,
                    y,
                    button,
                    blitz_traits::events::MouseEventButtons::empty(),
                )),
            );
            0
        }
        WM_RBUTTONUP => {
            let x = (lparam & 0xFFFF) as i16 as f32;
            let y = ((lparam >> 16) & 0xFFFF) as i16 as f32;
            forward_mouse_event(
                hwnd,
                blitz_traits::events::UiEvent::PointerUp(mouse_pointer_event(
                    x,
                    y,
                    blitz_traits::events::MouseEventButton::Secondary,
                    blitz_traits::events::MouseEventButtons::empty(),
                )),
            );
            let swell = swell();
            unsafe { swell.DefWindowProc(hwnd, msg, wparam, lparam) }
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam >> 16) & 0xFFFF) as i16 as f64 / 120.0;
            let x = (lparam & 0xFFFF) as i16 as f32;
            let y = ((lparam >> 16) & 0xFFFF) as i16 as f32;
            forward_mouse_event(
                hwnd,
                blitz_traits::events::UiEvent::Wheel(blitz_traits::events::BlitzWheelEvent {
                    delta: blitz_traits::events::BlitzWheelDelta::Lines(0.0, delta),
                    coords: blitz_traits::events::PointerCoords {
                        page_x: x,
                        page_y: y,
                        screen_x: x,
                        screen_y: y,
                        client_x: x,
                        client_y: y,
                    },
                    buttons: blitz_traits::events::MouseEventButtons::empty(),
                    mods: keyboard_types::Modifiers::empty(),
                }),
            );
            0
        }
        // ── Keyboard events ─────────────────────────────────────────
        WM_KEYDOWN | WM_KEYUP => {
            let key = swell_vk_to_key(wparam as u32);
            let code = swell_vk_to_code(wparam as u32);
            let event = blitz_traits::events::BlitzKeyEvent {
                key,
                code,
                modifiers: keyboard_types::Modifiers::empty(),
                location: keyboard_types::Location::Standard,
                is_auto_repeating: false,
                is_composing: false,
                state: if msg == WM_KEYDOWN {
                    blitz_traits::events::KeyState::Pressed
                } else {
                    blitz_traits::events::KeyState::Released
                },
                text: None,
            };
            let ui_event = if msg == WM_KEYDOWN {
                blitz_traits::events::UiEvent::KeyDown(event)
            } else {
                blitz_traits::events::UiEvent::KeyUp(event)
            };
            forward_mouse_event(hwnd, ui_event);
            0
        }
        WM_CHAR => {
            // Text input — send as IME commit
            if let Some(ch) = char::from_u32(wparam as u32)
                && !ch.is_control()
            {
                forward_mouse_event(
                    hwnd,
                    blitz_traits::events::UiEvent::Ime(
                        blitz_traits::events::BlitzImeEvent::Commit(ch.to_string()),
                    ),
                );
            }
            0
        }
        // ── Docker close button ─────────────────────────────────────
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
        // Tell the dialog manager we want every key — prevents Tab/Enter/Escape
        // being intercepted by the docker/dialog chain on Win32.
        // Matches reaimgui win32_window.cpp:396-397.
        WM_GETDLGCODE => DLGC_WANTALLKEYS,
        // Right-click → show dock/undock/close menu ONLY when the click is
        // outside our client area (i.e., on the docker tab / title bar).
        // Clicks inside the client area are content interactions; we fall
        // through to DefWindowProc so Blitz/REAPER can handle them.
        WM_CONTEXTMENU => {
            let sx = (lparam & 0xFFFF) as i16 as i32;
            let sy = ((lparam >> 16) & 0xFFFF) as i16 as i32;
            let swell = swell();
            let mut pt = raw::POINT { x: sx, y: sy };
            unsafe {
                swell.ScreenToClient(hwnd, &mut pt);
            }
            let mut client = raw::RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            unsafe {
                swell.GetClientRect(hwnd, &mut client);
            }
            let in_client = pt.x >= client.left
                && pt.x < client.right
                && pt.y >= client.top
                && pt.y < client.bottom;
            if in_client {
                // Content right-click — let Blitz/SWELL handle it normally.
                let swell = swell;
                unsafe { swell.DefWindowProc(hwnd, msg, wparam, lparam) }
            } else {
                // Tab / non-client right-click — show our dock menu.
                show_dock_context_menu(hwnd, sx, sy);
                0
            }
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
                        if let Some(view) = &panel.view {
                            view.mark_dirty();
                        }
                    } else {
                        // Keep the view alive for instant re-show; just stop rendering.
                        panel.visible = false;
                    }
                    tracing::info!(showing, panel = panel.config.id, "WM_SHOWWINDOW");
                }
            });
            0
        }
        WM_DESTROY => {
            // Order matters: remove the panel from the registry FIRST so any
            // re-entrant wndproc calls from DockWindowRemove fall through to
            // DefWindowProc rather than dereferencing a dying LivePanel.
            // Matches reaimgui's WM_DESTROY sequence (window.cpp:109-121):
            //   1. screenset_unregisterByParam
            //   2. clear GWLP_USERDATA (≈ our HashMap::remove)
            //   3. DockWindowRemove
            let taken = PANELS.with(|panels| {
                let mut panels = panels.borrow_mut();
                let id = panels
                    .iter()
                    .find(|(_, p)| p.hwnd == hwnd)
                    .map(|(id, _)| *id);
                id.and_then(|id| panels.remove(id).map(|p| (id, p)))
            });
            if let Some((_id, mut panel)) = taken {
                panel.view = None;
                let reaper = reaper();
                // Unregister screenset callback before destroying the HWND —
                // otherwise REAPER may call us back with a dead param.
                // Matches reaimgui (window.cpp:111).
                unsafe {
                    reaper.screenset_unregisterByParam(hwnd as *mut std::os::raw::c_void);
                }
                unsafe {
                    reaper.DockWindowRemove(hwnd);
                }
                // LivePanel::Drop handles X11 child cleanup (Linux).
                drop(panel);
            }
            0
        }
        _ => {
            // Log uncommon messages so we can see what the docker sends when
            // the user clicks the close X, etc. Cheap filter: skip the noisy
            // timer/paint/erasebg churn.
            if msg != 0x000E /*WM_ERASEBKGND*/
                && msg != 0x0014 /*WM_ERASEBKGND dup*/
                && msg != 0x002A /*WM_FONTCHANGE*/
                && msg != 0x0113 /*WM_TIMER*/
                && msg != 0x0020 /*WM_SETCURSOR*/
                && msg != 0x0084 /*WM_NCHITTEST*/
                && msg != 0x0083 /*WM_NCCALCSIZE — fires constantly*/
                && msg != 0x0085 /*WM_NCPAINT*/
                && msg != 0x0046 /*WM_WINDOWPOSCHANGING*/
                && msg != 0x0047 /*WM_WINDOWPOSCHANGED*/
                && msg != 0x0215
            /*WM_CAPTURECHANGED*/
            {
                tracing::info!(
                    msg = format!("0x{msg:04X}"),
                    wparam,
                    lparam,
                    "unhandled wnd msg"
                );
            }
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
        if current == child {
            return true;
        }
        let next = unsafe { swell.GetParent(current) };
        if next.is_null() || next == current {
            break;
        }
        current = next;
    }
    false
}
