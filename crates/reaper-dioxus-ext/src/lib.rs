//! reaper-dioxus extension — REAPER plugin providing Dioxus rendering as a service.
//!
//! This is the "ReaImGui for Dioxus" — a standalone extension that loads into
//! REAPER and provides GPU-accelerated Dioxus rendering to any other crate
//! in the process.
//!
//! ## How it works
//!
//! 1. REAPER loads `reaper_dioxus.so` from UserPlugins
//! 2. The extension initializes the GPU renderer and panel registry
//! 3. It registers a ~30Hz timer callback that drives all panel rendering
//! 4. Other extensions (FTS-Extensions, etc.) call `reaper_dioxus::service::*`
//!    to register and manage panels
//!
//! ## Installation
//!
//! Place `reaper_dioxus.so` in REAPER's `UserPlugins/` directory.
//! It should load before other FTS extensions (REAPER loads alphabetically).

use std::error::Error;

use reaper_high::Reaper as HighReaper;
use reaper_low::PluginContext;
use reaper_macros::reaper_extension_plugin;
use reaper_medium::ReaperSession;
use tracing::info;

#[reaper_extension_plugin]
fn plugin_main(context: PluginContext) -> Result<(), Box<dyn Error>> {
    // Don't re-init tracing if another FTS extension already did
    // (FTS-Extensions might load first)

    info!("reaper-dioxus service starting…");

    // Initialize REAPER APIs
    // Err means it was already loaded by another extension.
    if HighReaper::load(context).setup().is_ok() {
        info!("reaper-dioxus: REAPER API loaded");
    }

    // Initialize the service registry
    if reaper_dioxus::service::init() {
        info!("reaper-dioxus: Service initialized");
    } else {
        info!("reaper-dioxus: Service already initialized");
    }

    // Restore saved dock state
    reaper_dioxus::restore_dock_state();

    // Register the timer that drives panel rendering
    let _session = ReaperSession::load(context);
    // Note: we can't hold the session — just register and drop
    // The timer callback is a static extern "C" fn

    info!("reaper-dioxus service ready — other extensions can now register panels");
    Ok(())
}

// Timer callback — drives all panel rendering at ~30Hz.
// Registered by FTS-Extensions (or whichever host manages the main timer).
// reaper-dioxus provides the `service::tick()` function for this.
//
// Note: In practice, FTS-Extensions already has a timer callback that
// calls `reaper_dioxus::service::tick()`. The extension itself doesn't
// need its own timer since the host drives rendering.
