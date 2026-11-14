//! The diagram as a PNG: the SVG export rasterized by resvg, with the app's
//! bundled families loaded so the labels are set in the faces on screen.
//! gpui rasterizes SVG with resvg too, but keeps its font database private,
//! so this builds its own.

use std::sync::{Arc, LazyLock};

use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{self, fontdb};

use crate::theme::Fonts;

/// Pixels per pixel at 100%: sharp on a high-density screen and in a
/// document that scales the image down.
pub const SCALE: f32 = 2.0;
/// The widest or tallest image written, whatever the diagram's size.
pub const MAX_SIDE: u32 = 16_384;
/// The most pixels an image holds: 256 MiB of pixmap, before the encoder's
/// own buffers.
pub const MAX_PIXELS: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct Png {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Pixels per pixel at 100%: [`SCALE`], or less where the diagram had to
    /// be shrunk to fit [`MAX_SIDE`] and [`MAX_PIXELS`].
    pub scale: f32,
}

impl Png {
    pub fn shrunk(&self) -> bool {
        self.scale < SCALE
    }
}

/// The scale an image `width` by `height` at 100% is drawn at: [`SCALE`],
/// or whatever keeps it inside both caps.
pub fn scale_for(width: f32, height: f32) -> f32 {
    let (width, height) = (width.max(1.0), height.max(1.0));
    let side = MAX_SIDE as f32;
    SCALE
        .min(side / width)
        .min(side / height)
        .min((MAX_PIXELS as f32 / (width * height)).sqrt())
}

/// The bundled families, and the system's behind them for a family the user
/// picked that is not bundled, or a glyph the bundled ones lack. Loaded once:
/// reading the system's fonts takes a while, and they do not change.
static FONTS: LazyLock<Arc<fontdb::Database>> = LazyLock::new(|| {
    let mut db = fontdb::Database::new();
    for font in guic_gpui_assets::BUNDLED_FONTS {
        db.load_font_data(font.to_vec());
    }
    db.load_system_fonts();
    db.set_sans_serif_family(Fonts::DEFAULT_CHROME);
    db.set_monospace_family(Fonts::DEFAULT_GRID);
    Arc::new(db)
});

/// `svg`, the diagram's SVG export, as a PNG. Slow on a large diagram: run
/// it off the UI thread.
pub fn render(svg: &str) -> Result<Png, String> {
    render_with(svg, FONTS.clone())
}

fn render_with(svg: &str, fonts: Arc<fontdb::Database>) -> Result<Png, String> {
    let options = usvg::Options {
        fontdb: fonts,
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_str(svg, &options)
        .map_err(|error| format!("Could not draw the diagram: {error}"))?;
    let size = tree.size();
    let scale = scale_for(size.width(), size.height());
    let width = (size.width() * scale).ceil() as u32;
    let height = (size.height() * scale).ceil() as u32;
    let mut pixmap = Pixmap::new(width.max(1), height.max(1))
        .ok_or_else(|| format!("Could not make a {width} by {height} image."))?;
    resvg::render(
        &tree,
        Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let bytes = pixmap
        .encode_png()
        .map_err(|error| format!("Could not encode the image: {error}"))?;
    Ok(Png {
        bytes,
        width: pixmap.width(),
        height: pixmap.height(),
        scale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagram::layout::schema;
    use crate::diagram::svg::{self, frame, tests::style};
    use crate::diagram::tests::graph;

    fn bundled_only() -> Arc<fontdb::Database> {
        let mut db = fontdb::Database::new();
        for font in guic_gpui_assets::BUNDLED_FONTS {
            db.load_font_data(font.to_vec());
        }
        Arc::new(db)
    }

    #[test]
    fn a_small_diagram_renders_at_twice_its_size_with_its_labels_set() {
        let graph = graph(3, &[(1, 0), (2, 1)]);
        let layout = schema(&graph, false);
        let document = svg::svg(&graph, &layout, &style());
        let png = render_with(&document, bundled_only()).unwrap();
        let frame = frame(&layout);
        assert_eq!(png.scale, SCALE);
        assert!(!png.shrunk());
        // The file rounds its size to two places.
        assert!(png.width.abs_diff((frame.width * SCALE).ceil() as u32) <= 1);
        assert!(png.height.abs_diff((frame.height * SCALE).ceil() as u32) <= 1);
        let decoded = Pixmap::decode_png(&png.bytes).expect("a PNG");
        assert_eq!((decoded.width(), decoded.height()), (png.width, png.height));

        // With no font to set them in, the labels draw nothing: a database of
        // the bundled families alone sets them.
        let blank = render_with(&document, Arc::new(fontdb::Database::new())).unwrap();
        let lit = |png: &Png| {
            let text = Srgb8::from(style().palette.text);
            Pixmap::decode_png(&png.bytes)
                .unwrap()
                .pixels()
                .iter()
                .filter(|pixel| {
                    let pixel = pixel.demultiply();
                    (pixel.red(), pixel.green(), pixel.blue()) == text.0
                })
                .count()
        };
        assert_eq!(lit(&blank), 0, "no text without a font");
        assert!(lit(&png) > 100, "the bundled families set the labels");
    }

    struct Srgb8((u8, u8, u8));

    impl From<crate::theme::color::Srgb> for Srgb8 {
        fn from(colour: crate::theme::color::Srgb) -> Self {
            let channel = |value: f32| (value * 255.0).round() as u8;
            Self((channel(colour.r), channel(colour.g), channel(colour.b)))
        }
    }

    #[test]
    fn a_huge_diagram_is_shrunk_to_the_caps_rather_than_allocated_whole() {
        assert_eq!(scale_for(100.0, 100.0), SCALE);
        let wide = scale_for(40_000.0, 200.0);
        assert!((40_000.0 * wide).ceil() as u32 <= MAX_SIDE, "{wide}");
        let big = scale_for(12_000.0, 12_000.0);
        let pixels = (12_000.0 * big).ceil() as u64 * (12_000.0 * big).ceil() as u64;
        assert!(pixels <= MAX_PIXELS + 2 * 12_000, "{big}: {pixels}");
        assert!(big < SCALE);

        let document = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="10"><rect width="100000" height="10" fill="red"/></svg>"#;
        let png = render_with(document, bundled_only()).unwrap();
        assert!(png.shrunk());
        assert_eq!(png.width, MAX_SIDE);
        assert!(png.height >= 1);
    }
}
