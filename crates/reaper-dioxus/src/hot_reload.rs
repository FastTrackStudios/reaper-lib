//! Hot reload support via dioxus-devtools.
//!
//! When the `hot-reload` feature is enabled, connects to the Dioxus devtools
//! server (`dx serve`) for subsecond RSX template patching. Changes to RSX
//! markup, Tailwind classes, and CSS assets are applied to the running VirtualDom
//! without restarting REAPER.
//!
//! # Usage
//!
//! 1. Run `dx serve` in your project directory
//! 2. Launch REAPER — the extension auto-connects to the devtools server
//! 3. Edit RSX in `.rs` files — changes appear in ~5-20ms
//!
//! Disable with `--no-default-features` or `default-features = false` for release.

#[cfg(feature = "hot-reload")]
use blitz_dom::Document as _;
#[cfg(feature = "hot-reload")]
use crossbeam::channel::{Receiver, Sender, unbounded};
#[cfg(feature = "hot-reload")]
use dioxus_devtools::DevserverMsg;
#[cfg(feature = "hot-reload")]
use dioxus_native::DioxusDocument;

/// State for managing hot reload connections.
///
/// When the `hot-reload` feature is disabled, this is a zero-size struct
/// and all methods are no-ops.
pub struct HotReloadState {
    #[cfg(feature = "hot-reload")]
    receiver: Receiver<DevserverMsg>,
    #[cfg(feature = "hot-reload")]
    sender: Sender<DevserverMsg>,
    #[cfg(feature = "hot-reload")]
    connected: bool,
}

impl HotReloadState {
    pub fn new() -> Self {
        #[cfg(feature = "hot-reload")]
        {
            let (sender, receiver) = unbounded();
            Self {
                receiver,
                sender,
                connected: false,
            }
        }
        #[cfg(not(feature = "hot-reload"))]
        {
            Self {}
        }
    }

    /// Connect to the dioxus devtools server.
    /// Call once when the panel/overlay is created.
    /// No-op if hot-reload feature is disabled or already connected.
    pub fn connect(&mut self) {
        #[cfg(feature = "hot-reload")]
        {
            if self.connected {
                return;
            }
            let sender = self.sender.clone();
            dioxus_devtools::connect(move |msg| {
                let _ = sender.send(msg);
            });
            self.connected = true;
            tracing::info!("Hot reload: connected to dioxus devtools server");
        }
    }

    /// Process pending hot reload messages and apply to the document.
    /// Call this on each frame/tick.
    /// Returns true if any changes were applied (triggers re-render).
    #[cfg(feature = "hot-reload")]
    pub fn process_messages(&self, doc: &mut DioxusDocument) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.receiver.try_recv() {
            match msg {
                DevserverMsg::HotReload(hotreload_msg) => {
                    dioxus_devtools::apply_changes(&doc.vdom, &hotreload_msg);

                    for asset_path in &hotreload_msg.assets {
                        if let Some(url) = asset_path.to_str() {
                            doc.inner_mut().reload_resource_by_href(url);
                        }
                    }
                    changed = true;
                    tracing::debug!("Hot reload: applied RSX changes");
                }
                DevserverMsg::FullReloadStart => {
                    tracing::info!("Hot reload: full reload requested (rebuild needed)");
                }
                DevserverMsg::FullReloadFailed => {
                    tracing::warn!("Hot reload: full reload failed");
                }
                _ => {}
            }
        }
        changed
    }

    /// No-op when hot-reload is disabled.
    #[cfg(not(feature = "hot-reload"))]
    pub fn process_messages(&self, _doc: &mut dioxus_native::DioxusDocument) -> bool {
        false
    }
}

impl Default for HotReloadState {
    fn default() -> Self {
        Self::new()
    }
}
