//! DioxusOverlay — transparent window with dioxus-native rendering.
//!
//! Creates a `TransparentWindow` from reaper-embed, then drives a DioxusDocument
//! render loop on top of it. Designed to be ticked from a REAPER timer callback
//! (~30Hz) rather than an event loop.

use anyrender_vello::VelloScenePainter;
use blitz_dom::Document as _;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use dioxus_native::prelude::*;
use dioxus_native::{DioxusDocument, DocumentConfig};
use futures_util::task::ArcWake;
use reaper_embed::TransparentWindow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use vello::Scene;

/// A transparent overlay window that renders a Dioxus component tree.
///
/// The GPU rendering pipeline is handled by `TransparentWindow` internally.
/// This struct owns the DOM layer and paints it to a Vello `Scene` each frame,
/// which is then passed to the window for GPU rendering.
///
/// Call `update()` from your timer callback to drive the render loop.
pub struct DioxusOverlay {
    window: TransparentWindow,
    doc: DioxusDocument,
    scene: Scene,
    waker: std::task::Waker,
    needs_redraw: Arc<AtomicBool>,
    animation_start: Instant,
    width: u32,
    height: u32,
}

/// Waker that sets a flag when the VirtualDom has pending work.
struct OverlayWaker(Arc<AtomicBool>);

impl ArcWake for OverlayWaker {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.store(true, Ordering::Relaxed);
    }
}

/// Builder for creating a `DioxusOverlay` with optional context injection.
pub struct DioxusOverlayBuilder {
    app: fn() -> Element,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
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

    /// Build and open the overlay window.
    pub fn build(self) -> Result<DioxusOverlay, reaper_embed::GpuError> {
        DioxusOverlay::new_inner(self.app, self.x, self.y, self.width, self.height, self.contexts)
    }
}

impl DioxusOverlay {
    /// Open a new overlay with the given root component.
    pub fn open(
        app: fn() -> Element,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<Self, reaper_embed::GpuError> {
        Self::new_inner(app, x, y, width, height, Vec::new())
    }

    /// Create a builder for more control over context injection.
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
            contexts: Vec::new(),
        }
    }

    fn new_inner(
        app: fn() -> Element,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        contexts: Vec<Box<dyn FnOnce()>>,
    ) -> Result<Self, reaper_embed::GpuError> {
        let width = width.max(1);
        let height = height.max(1);

        // Create the transparent window (platform-specific: macOS Cocoa, Linux X11)
        let mut window = TransparentWindow::open(x, y, width, height)?;
        window.show();

        // Create the Dioxus virtual DOM and document
        let vdom = VirtualDom::new(app);
        let viewport = Viewport::new(width, height, 1.0, ColorScheme::Dark);

        let mut doc = DioxusDocument::new(
            vdom,
            DocumentConfig {
                viewport: Some(viewport),
                ..Default::default()
            },
        );

        // Inject contexts into the root scope before initial build
        doc.vdom.in_scope(ScopeId::ROOT, || {
            for ctx_fn in contexts {
                ctx_fn();
            }
        });

        // Initial DOM build + Taffy layout
        doc.initial_build();
        doc.inner_mut().resolve(0.0);

        // Create waker (sets a flag when async tasks need attention)
        let needs_redraw = Arc::new(AtomicBool::new(true));
        let waker = futures_util::task::waker(Arc::new(OverlayWaker(needs_redraw.clone())));

        Ok(Self {
            window,
            doc,
            scene: Scene::new(),
            waker,
            needs_redraw,
            animation_start: Instant::now(),
            width,
            height,
        })
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

        // Resolve Taffy layout
        self.doc.inner_mut().resolve(animation_time);

        // Paint the DOM tree to a Vello scene
        self.scene.reset();
        paint_scene(
            &mut VelloScenePainter::new(&mut self.scene),
            &*self.doc.inner(),
            1.0, // scale factor (physical pixels)
            self.width,
            self.height,
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
            let viewport = Viewport::new(width, height, 1.0, ColorScheme::Dark);
            self.doc.inner_mut().set_viewport(viewport);
        }
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

    pub fn close(&mut self) {
        self.window.close();
    }
}

impl Drop for DioxusOverlay {
    fn drop(&mut self) {
        self.window.close();
    }
}
