//! Vello text renderer using skrifa for font metrics.

use skrifa::raw::FileRef;
use skrifa::{FontRef, MetadataProvider, instance::LocationRef};
use std::sync::Arc;
use vello::kurbo::Affine;
use vello::peniko::{Blob, Color, Fill, FontData};
use vello::{Glyph, Scene};

#[cfg(target_os = "macos")]
const SYSTEM_FONT: &[u8] = include_bytes!("/System/Library/Fonts/Supplemental/Arial.ttf");

#[cfg(target_os = "windows")]
const SYSTEM_FONT: &[u8] = include_bytes!("C:/Windows/Fonts/arial.ttf");

// Bundled DejaVuSans — works on any Linux (NixOS, Debian, Fedora, etc.)
#[cfg(target_os = "linux")]
const SYSTEM_FONT: &[u8] = include_bytes!("../fonts/DejaVuSans.ttf");

/// Text renderer that uses Vello's `draw_glyphs` API with skrifa font metrics.
pub struct VelloTextRenderer {
    font_data: FontData,
}

impl VelloTextRenderer {
    pub fn new() -> Self {
        let font_data = FontData::new(Blob::new(Arc::new(SYSTEM_FONT.to_vec())), 0);
        Self { font_data }
    }

    fn font_ref(&self) -> Option<FontRef<'_>> {
        let file_ref = FileRef::new(self.font_data.data.as_ref()).ok()?;
        match file_ref {
            FileRef::Font(font) => Some(font),
            FileRef::Collection(collection) => collection.get(self.font_data.index).ok(),
        }
    }

    /// Measure the width of a text string in pixels at the given font size.
    pub fn measure_text(&self, text: &str, font_size: f32) -> f64 {
        if let Some(font_ref) = self.font_ref() {
            let size = skrifa::instance::Size::new(font_size);
            let charmap = font_ref.charmap();
            let glyph_metrics = font_ref.glyph_metrics(size, LocationRef::default());

            let mut width = 0.0f32;
            for ch in text.chars() {
                let gid = charmap.map(ch).unwrap_or_default();
                let advance = glyph_metrics.advance_width(gid).unwrap_or_default();
                width += advance;
            }
            width as f64
        } else {
            // Fallback approximation
            text.len() as f64 * font_size as f64 * 0.6
        }
    }

    /// Draw text at (x, y) where y is the baseline position.
    pub fn draw_text(
        &self,
        scene: &mut Scene,
        text: &str,
        x: f64,
        y: f64,
        font_size: f32,
        color: Color,
    ) {
        let Some(font_ref) = self.font_ref() else {
            return;
        };

        let size = skrifa::instance::Size::new(font_size);
        let charmap = font_ref.charmap();
        let glyph_metrics = font_ref.glyph_metrics(size, LocationRef::default());

        let mut glyphs = Vec::new();
        let mut pen_x = 0.0f32;

        for ch in text.chars() {
            if ch == '\n' {
                continue;
            }
            let gid = charmap.map(ch).unwrap_or_default();
            let advance = glyph_metrics.advance_width(gid).unwrap_or_default();

            glyphs.push(Glyph {
                id: gid.to_u32(),
                x: pen_x,
                y: 0.0,
            });

            pen_x += advance;
        }

        let transform = Affine::translate((x, y));

        scene
            .draw_glyphs(&self.font_data)
            .font_size(font_size)
            .transform(transform)
            .brush(color)
            .draw(Fill::NonZero, glyphs.into_iter());
    }
}

impl Default for VelloTextRenderer {
    fn default() -> Self {
        Self::new()
    }
}
