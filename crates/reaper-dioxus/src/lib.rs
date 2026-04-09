//! Dioxus-native rendering for REAPER extensions.
//!
//! Provides two rendering modes:
//!
//! - **`DioxusOverlay`** — A transparent overlay window that floats above REAPER.
//!   Click-through by default. Great for HUDs, which-key popups, status indicators.
//!
//! - **`EmbeddedView`** — A Dioxus view rendered inside an existing REAPER window.
//!   Fully interactive (mouse, keyboard). For dockable panels, custom editors, etc.
//!
//! Both are driven by the caller's timer callback (no baseview, no event loop).

pub mod embedded;
pub mod overlay;

pub use embedded::EmbeddedView;
pub use overlay::{DioxusOverlay, DioxusOverlayBuilder, OverlayConfig};

// Re-export dioxus prelude for component authors
pub use dioxus_native::prelude;
