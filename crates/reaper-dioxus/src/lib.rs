//! Dioxus-native rendering for REAPER extensions.
//!
//! Provides three rendering modes:
//!
//! - **`DioxusOverlay`** — A transparent overlay window that floats above REAPER.
//!   Click-through by default. Great for HUDs, which-key popups, status indicators.
//!
//! - **`EmbeddedView`** — A Dioxus view rendered inside an existing window handle.
//!   Fully interactive (mouse, keyboard). For custom windows, child panels, etc.
//!
//! - **`DockablePanel`** — A Dioxus view inside REAPER's native docker system.
//!   Shows up as a dockable tab alongside Mixer, FX Browser, etc. Full REAPER
//!   integration: dock position saved, toggle via actions, HiDPI, state persistence.
//!
//! All modes are driven by the caller's timer callback (no baseview, no event loop).

pub mod dock;
pub mod embedded;
pub mod overlay;

pub use dock::{DockablePanelConfig, register_panel, toggle_panel, show_panel, hide_panel,
               is_panel_visible, update_panels, unregister_all_panels,
               save_dock_state, restore_dock_state};
pub use embedded::EmbeddedView;
pub use overlay::{DioxusOverlay, DioxusOverlayBuilder, OverlayConfig};

// Re-export dioxus prelude for component authors
pub use dioxus_native::prelude;
