//! Dioxus-native overlay rendering for REAPER extensions.
//!
//! Provides `DioxusOverlay` — a transparent overlay window that renders a Dioxus
//! component tree via the blitz rendering engine (dioxus-native + Vello + wgpu).
//!
//! Unlike the plugin integration (nih_plug_dioxus), this does not use baseview.
//! The render loop is driven by the caller (typically a REAPER timer callback).

mod overlay;

pub use overlay::DioxusOverlay;

// Re-export dioxus prelude for component authors
pub use dioxus_native::prelude;
