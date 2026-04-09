//! Transparent overlay windows with Vello rendering for REAPER extensions.
//!
//! Provides a `TransparentWindow` backed by a native borderless window (NSWindow on macOS)
//! with GPU-accelerated Vello scene rendering via WGPU. Designed for HUD-style overlays
//! that float above REAPER's arrange view.

#![allow(dead_code)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod gpu;
pub mod platform;
pub mod text;
pub mod transparent;

pub use gpu::{GpuError, GpuState};
pub use platform::{WindowRect, display_scale_factor, main_screen_height};
pub use text::VelloTextRenderer;
pub use transparent::TransparentWindow;
