//! Dioxus-native rendering service for REAPER extensions.
//!
//! Runs as a standalone REAPER extension that provides GPU-accelerated
//! Dioxus rendering to any other crate in the process (like ReaImGui).
//!
//! ## Service Model
//!
//! The extension owns the renderer, timer, and panel registry. Other crates
//! call the global service API:
//!
//! ```rust,ignore
//! reaper_dioxus::service::register_panel_from_def(&panel_def);
//! reaper_dioxus::service::toggle("FTS_LAUNCHER");
//! reaper_dioxus::service::show("FTS_INPUT_STATUS");
//! ```
//!
//! ## Rendering Modes
//!
//! - **`DockablePanel`** — REAPER's native docker (tabs alongside Mixer, FX Browser)
//! - **`EmbeddedView`** — Dioxus inside an existing HWND
//! - **`DioxusOverlay`** — Transparent floating window (HUDs, popups)
//!
//! All rendering is timer-driven (~30 Hz from REAPER's main thread).

pub mod dock;
pub mod embedded;
pub mod hot_reload;
#[cfg(target_os = "macos")]
pub mod macos_input;
pub mod overlay;
pub mod service;

pub use dock::{
    register_panel, toggle_panel, show_panel, hide_panel,
    is_panel_visible, update_panels, unregister_all_panels,
    save_dock_state, restore_dock_state, DockablePanelConfig,
};
pub use embedded::EmbeddedView;
pub use overlay::{DioxusOverlay, DioxusOverlayBuilder, OverlayConfig};

// Re-export dioxus prelude for component authors
pub use dioxus_native::prelude;
