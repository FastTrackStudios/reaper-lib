//! DioxusOverlay — transparent window with dioxus-native rendering.
//!
//! Creates a `TransparentWindow` from reaper-embed, then drives a DioxusDocument
//! render loop on top of it. Designed to be ticked from a REAPER timer callback
//! (~30Hz) rather than an event loop.

use anyrender_vello::VelloScenePainter;
use blitz_dom::Document as _;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use crossbeam::channel::{Receiver, Sender, unbounded};
use dioxus_native::prelude::*;
use dioxus_native::{DioxusDocument, DocumentConfig};
use futures_util::task::ArcWake;
use reaper_embed::TransparentWindow;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use vello::Scene;

// ---------------------------------------------------------------------------
// Document message channel (for document::Style support)
// ---------------------------------------------------------------------------

/// Messages sent by `document::Style` and similar head-element components.
pub(crate) enum DocumentMessage {
    CreateHeadElement {
        name: String,
        attributes: Vec<(String, String)>,
        contents: Option<String>,
    },
}

/// Proxy that implements `dioxus::document::Document` and forwards head element
/// creation requests through a channel. This enables `document::Style { }` in RSX.
pub(crate) struct DocumentProxy {
    sender: Sender<DocumentMessage>,
}

impl DocumentProxy {
    pub(crate) fn new(sender: Sender<DocumentMessage>) -> Self {
        Self { sender }
    }
}

impl document::Document for DocumentProxy {
    fn eval(&self, js: String) -> document::Eval {
        // No JS eval support in native overlay context
        document::NoOpDocument.eval(js)
    }

    fn create_head_element(
        &self,
        name: &str,
        attributes: &[(&str, String)],
        contents: Option<String>,
    ) {
        let _ = self.sender.send(DocumentMessage::CreateHeadElement {
            name: name.to_string(),
            attributes: attributes
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            contents,
        });
    }
}

// ---------------------------------------------------------------------------
// Waker
// ---------------------------------------------------------------------------

/// Waker that sets a flag when the VirtualDom has pending work.
struct OverlayWaker(Arc<AtomicBool>);

impl ArcWake for OverlayWaker {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// DioxusOverlay
// ---------------------------------------------------------------------------

/// A transparent overlay window that renders a Dioxus component tree.
///
/// Supports `document::Style` for global CSS injection, HiDPI scaling,
/// and optional click-through behavior.
///
/// Call `update()` from your timer callback to drive the render loop.
pub struct DioxusOverlay {
    window: TransparentWindow,
    doc: DioxusDocument,
    doc_message_rx: Receiver<DocumentMessage>,
    scene: Scene,
    waker: std::task::Waker,
    needs_redraw: Arc<AtomicBool>,
    animation_start: Instant,
    scale_factor: f32,
    width: u32,
    height: u32,
    interactive: bool,
}

/// Configuration for overlay behavior.
#[derive(Clone, Debug, Default)]
pub struct OverlayConfig {
    /// Whether the overlay accepts mouse/keyboard input (default: false = click-through).
    pub interactive: bool,
    /// Whether to auto-fit the window size to the DOM content after initial build.
    pub auto_fit: bool,
    /// Whether to hide the window after the initial render for later instant display.
    pub start_hidden: bool,
}

/// Builder for creating a `DioxusOverlay` with optional context injection.
pub struct DioxusOverlayBuilder {
    app: fn() -> Element,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    config: OverlayConfig,
    contexts: Vec<Box<dyn FnOnce()>>,
}

impl DioxusOverlayBuilder {
    /// Register a context value to be provided to the component tree.
    pub fn with_context<T: Clone + 'static>(mut self, value: T) -> Self {
        self.contexts.push(Box::new(move || {
            provide_context(value);
        }));
        self
    }

    /// Make this overlay interactive (receives mouse/keyboard events).
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.config.interactive = interactive;
        self
    }

    /// Auto-fit the window size to DOM content after initial build.
    pub fn auto_fit(mut self, auto_fit: bool) -> Self {
        self.config.auto_fit = auto_fit;
        self
    }

    /// Build and render the overlay, then hide it before returning.
    pub fn start_hidden(mut self, start_hidden: bool) -> Self {
        self.config.start_hidden = start_hidden;
        self
    }

    /// Build and open the overlay window.
    pub fn build(self) -> Result<DioxusOverlay, reaper_embed::GpuError> {
        DioxusOverlay::new_inner(
            self.app,
            self.x,
            self.y,
            self.width,
            self.height,
            self.config,
            self.contexts,
        )
    }
}

impl DioxusOverlay {
    /// Open a new overlay with the given root component (click-through by default).
    pub fn open(
        app: fn() -> Element,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<Self, reaper_embed::GpuError> {
        Self::new_inner(
            app,
            x,
            y,
            width,
            height,
            OverlayConfig::default(),
            Vec::new(),
        )
    }

    /// Create a builder for more control over context injection and behavior.
    pub fn builder(
        app: fn() -> Element,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> DioxusOverlayBuilder {
        DioxusOverlayBuilder {
            app,
            x,
            y,
            width,
            height,
            config: OverlayConfig::default(),
            contexts: Vec::new(),
        }
    }

    fn new_inner(
        app: fn() -> Element,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        config: OverlayConfig,
        contexts: Vec<Box<dyn FnOnce()>>,
    ) -> Result<Self, reaper_embed::GpuError> {
        let width = width.max(1);
        let height = height.max(1);

        let t_start = std::time::Instant::now();
        tracing::debug!(x, y, width, height, "DioxusOverlay build: begin");

        // Create the transparent window (platform-specific: macOS Cocoa, Linux X11)
        let t = std::time::Instant::now();
        let mut window = TransparentWindow::open(x, y, width, height)?;
        let t_window_open = t.elapsed();
        tracing::debug!("DioxusOverlay build: transparent window opened");

        // Set click-through based on config
        if !config.interactive {
            // Click-through is the default from TransparentWindow::open(),
            // but we explicitly set it for interactive overlays
        } else {
            window.set_click_through(false);
        }

        window.show();
        tracing::debug!("DioxusOverlay build: window shown");

        // Query HiDPI scale factor
        let scale_factor = reaper_embed::display_scale_factor() as f32;
        tracing::debug!(scale_factor, "DioxusOverlay build: display scale");

        // Set up document::Style message channel
        let (doc_tx, doc_rx) = unbounded();
        let doc_proxy = Rc::new(DocumentProxy::new(doc_tx));

        // Create the Dioxus virtual DOM and document
        let t = std::time::Instant::now();
        let vdom = VirtualDom::new(app);
        let viewport = Viewport::new(width, height, scale_factor, ColorScheme::Dark);

        let mut doc = DioxusDocument::new(
            vdom,
            DocumentConfig {
                viewport: Some(viewport),
                ..Default::default()
            },
        );
        let t_doc_create = t.elapsed();
        tracing::debug!("DioxusOverlay build: document created");

        // Inject contexts into the root scope before initial build
        doc.vdom.in_scope(ScopeId::ROOT, || {
            // Provide DocumentProxy so document::Style works
            provide_context(doc_proxy as Rc<dyn document::Document>);

            // User-provided contexts
            for ctx_fn in contexts {
                ctx_fn();
            }
        });
        tracing::debug!("DioxusOverlay build: contexts injected");

        // Initial DOM build — this may queue document::Style messages
        let t = std::time::Instant::now();
        doc.initial_build();
        let t_initial_build = t.elapsed();
        tracing::debug!("DioxusOverlay build: initial DOM built");

        // Process any document::Style messages before first layout
        let t = std::time::Instant::now();
        while let Ok(msg) = doc_rx.try_recv() {
            match msg {
                DocumentMessage::CreateHeadElement {
                    name,
                    attributes,
                    contents,
                } => {
                    doc.create_head_element(&name, &attributes, &contents);
                }
            }
        }
        let t_head_msgs = t.elapsed();
        tracing::debug!("DioxusOverlay build: head messages processed");

        // Resolve Taffy layout
        let t = std::time::Instant::now();
        doc.inner_mut().resolve(0.0);
        let t_layout = t.elapsed();
        tracing::debug!("DioxusOverlay build: layout resolved");

        // Auto-fit: query the root element's layout size and resize window to fit
        if config.auto_fit
            && let Some((content_w, content_h)) = query_root_content_size(&doc)
        {
            let fit_w = (content_w * scale_factor as f64).ceil() as u32;
            let fit_h = (content_h * scale_factor as f64).ceil() as u32;
            if fit_w > 0 && fit_h > 0 {
                window.set_frame(x, y, fit_w, fit_h);
                let viewport = Viewport::new(fit_w, fit_h, scale_factor, ColorScheme::Dark);
                doc.inner_mut().set_viewport(viewport);
                doc.inner_mut().resolve(0.0);
            }
        }

        // Create waker (sets a flag when async tasks need attention)
        let needs_redraw = Arc::new(AtomicBool::new(true));
        let waker = futures_util::task::waker(Arc::new(OverlayWaker(needs_redraw.clone())));
        tracing::debug!("DioxusOverlay build: initial render");

        let mut overlay = Self {
            window,
            doc,
            doc_message_rx: doc_rx,
            scene: Scene::new(),
            waker,
            needs_redraw,
            animation_start: Instant::now(),
            scale_factor,
            width,
            height,
            interactive: config.interactive,
        };
        let t = std::time::Instant::now();
        overlay.update();
        let t_first_update = t.elapsed();
        if config.start_hidden {
            overlay.hide();
        }

        tracing::info!(
            window_open_ms = t_window_open.as_millis(),
            doc_create_ms = t_doc_create.as_millis(),
            initial_build_ms = t_initial_build.as_millis(),
            head_msgs_ms = t_head_msgs.as_millis(),
            layout_ms = t_layout.as_millis(),
            first_update_ms = t_first_update.as_millis(),
            total_ms = t_start.elapsed().as_millis(),
            "DioxusOverlay build timing"
        );

        Ok(overlay)
    }

    /// Tick the overlay: poll the VirtualDom, resolve layout, and render.
    ///
    /// Call this from your timer callback (~30Hz is fine).
    pub fn update(&mut self) {
        let animation_time = self.animation_start.elapsed().as_secs_f64();

        // Mark all nodes dirty and poll the virtual DOM for changes
        self.doc.vdom.mark_all_dirty();
        let cx = std::task::Context::from_waker(&self.waker);
        self.doc.poll(Some(cx));

        // Process any document::Style messages before layout
        while let Ok(msg) = self.doc_message_rx.try_recv() {
            match msg {
                DocumentMessage::CreateHeadElement {
                    name,
                    attributes,
                    contents,
                } => {
                    self.doc.create_head_element(&name, &attributes, &contents);
                }
            }
        }

        // Resolve Taffy layout
        self.doc.inner_mut().resolve(animation_time);

        // Paint the DOM tree to a Vello scene
        self.scene.reset();
        paint_scene(
            &mut VelloScenePainter::new(&mut self.scene),
            &mut self.doc.inner_mut(),
            self.scale_factor as f64,
            self.width,
            self.height,
            0,
            0,
        );

        // Render the scene to the window's GPU surface
        if let Err(e) = self.window.render(&self.scene) {
            tracing::debug!("Overlay render error: {e}");
        }

        self.needs_redraw.store(false, Ordering::Relaxed);
    }

    /// Reposition and resize the overlay window.
    pub fn set_frame(&mut self, x: i32, y: i32, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        self.window.set_frame(x, y, width, height);

        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;

            // Update the viewport so Taffy re-layouts at the new size
            let viewport = Viewport::new(width, height, self.scale_factor, ColorScheme::Dark);
            self.doc.inner_mut().set_viewport(viewport);
        }
    }

    /// Auto-fit the window to the current DOM content size.
    /// Returns the new (width, height) if resized, or None if content size
    /// couldn't be determined.
    pub fn fit_to_content(&mut self) -> Option<(u32, u32)> {
        if let Some((content_w, content_h)) = query_root_content_size(&self.doc) {
            let fit_w = (content_w * self.scale_factor as f64).ceil() as u32;
            let fit_h = (content_h * self.scale_factor as f64).ceil() as u32;
            if fit_w > 0 && fit_h > 0 {
                let bounds = self.window.bounds();
                self.set_frame(bounds.x, bounds.y, fit_w, fit_h);
                return Some((fit_w, fit_h));
            }
        }
        None
    }

    pub fn show(&mut self) {
        self.window.show();
    }

    pub fn hide(&mut self) {
        self.window.hide();
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    /// Set whether this overlay is click-through or interactive.
    pub fn set_interactive(&mut self, interactive: bool) {
        if self.interactive != interactive {
            self.interactive = interactive;
            self.window.set_click_through(!interactive);
        }
    }

    pub fn close(&mut self) {
        self.window.close();
    }

    /// Get the current window bounds.
    pub fn bounds(&self) -> &reaper_embed::WindowRect {
        self.window.bounds()
    }
}

impl Drop for DioxusOverlay {
    fn drop(&mut self) {
        self.window.close();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Query the root element's content size from the Taffy layout.
/// Returns None if the layout hasn't been resolved yet.
fn query_root_content_size(doc: &DioxusDocument) -> Option<(f64, f64)> {
    let inner = doc.inner();
    let root = inner.root_element();
    let layout = &root.final_layout;
    let w = layout.size.width;
    let h = layout.size.height;
    if w > 0.0 && h > 0.0 {
        Some((w as f64, h as f64))
    } else {
        None
    }
}
