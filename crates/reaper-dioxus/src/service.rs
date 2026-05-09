//! Global service API — the ReaImGui-like interface.
//!
//! `reaper-dioxus` runs as a standalone REAPER extension that owns the
//! GPU renderer and panel registry. Other crates in the same process
//! call these global functions to create and manage panels.
//!
//! # Architecture
//!
//! ```text
//! reaper-dioxus-ext.so (REAPER extension)
//!   ├─ Owns GPU state (wgpu)
//!   ├─ Owns timer callback (~30 Hz)
//!   ├─ Panel registry (HashMap<&str, DockablePanel>)
//!   └─ Global API (OnceLock<Service>)
//!        ↑
//!        ├── FTS-Extensions calls register_panel(), toggle_panel(), etc.
//!        ├── fts-reaper-input calls register_panel() for keyboard viz
//!        └── Any other Rust crate in the process
//! ```
//!
//! # Usage from other crates
//!
//! ```rust,ignore
//! // No need to depend on reaper-dioxus at build time!
//! // Just call the global API at runtime:
//! reaper_dioxus::service::register_panel(PanelConfig { ... });
//! reaper_dioxus::service::toggle_panel("FTS_LAUNCHER");
//! reaper_dioxus::service::show_panel("FTS_INPUT_STATUS");
//! ```

use std::sync::OnceLock;

use daw::module::PanelDef;

/// The global service instance. Set once by the reaper-dioxus extension.
static SERVICE: OnceLock<ServiceState> = OnceLock::new();

struct ServiceState {
    initialized: bool,
}

/// Check if the reaper-dioxus service is available.
///
/// Returns false if the reaper-dioxus extension hasn't loaded yet.
/// Other crates should check this before calling service functions.
pub fn is_available() -> bool {
    SERVICE.get().is_some_and(|s| s.initialized)
}

/// Initialize the service. Call once at startup before registering panels.
/// Returns false if already initialized.
pub fn init() -> bool {
    SERVICE.set(ServiceState { initialized: true }).is_ok()
}

// ── Panel management API ───────────────────────────────────────

/// Register a panel from a DawModule PanelDef.
///
/// This is the primary API for modules. Call it from the host extension
/// after collecting panels via `daw::module::collect_panels()`.
pub fn register_panel_from_def(def: &PanelDef) {
    if !is_available() {
        tracing::warn!(
            panel = def.id,
            "reaper-dioxus not available — panel not registered"
        );
        return;
    }

    tracing::info!(
        panel = def.id,
        title = def.title,
        "Registering panel with reaper-dioxus service"
    );

    // Delegate to the dock module's register_panel
    // The component pointer from PanelDef is cast to the Dioxus component type
    crate::dock::register_panel_from_service(def);
}

/// Register multiple panels from a slice of PanelDefs.
pub fn register_panels(defs: &[PanelDef]) {
    for def in defs {
        register_panel_from_def(def);
    }
}

/// Toggle a panel's visibility.
pub fn toggle(id: &str) {
    if !is_available() {
        return;
    }
    // Leak the string to get 'static lifetime — panel IDs are process-lifetime
    let id: &'static str = Box::leak(id.to_string().into_boxed_str());
    crate::dock::toggle_panel(id);
}

/// Show a panel.
pub fn show(id: &str) {
    if !is_available() {
        return;
    }
    let id: &'static str = Box::leak(id.to_string().into_boxed_str());
    crate::dock::show_panel(id);
}

/// Hide a panel.
pub fn hide(id: &str) {
    if !is_available() {
        return;
    }
    let id: &'static str = Box::leak(id.to_string().into_boxed_str());
    crate::dock::hide_panel(id);
}

/// Check if a panel is visible.
pub fn is_visible(id: &str) -> bool {
    if !is_available() {
        return false;
    }
    let id: &'static str = Box::leak(id.to_string().into_boxed_str());
    crate::dock::is_panel_visible(id)
}

/// Update all registered panels (called from the timer callback).
pub fn tick() {
    crate::dock::update_panels();
}
