//! Draws the overlay on the CPU: shapes in points at the surface's scale, and text shaped with the
//! system's fonts, falling back for any script they cover.

#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    reason = "pixel arithmetic on a small surface"
)]

use std::collections::HashMap;

use cosmic_text::{
    Align, Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent, Weight,
    Wrap,
};
use thalvi_desktop::host::OverlayFrame;
use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LinearGradient, Mask, Paint, PathBuilder, Pixmap,
    PixmapPaint, Point, Shader, SpreadMode, Stroke, Transform,
};

/// Shaped lines kept for reuse; the overlay shows a handful at a time.
const SHAPED_KEPT: usize = 32;

/// A colour with straight alpha.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    pub const fn alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }

    fn skia(self) -> Color {
        Color::from_rgba8(
            self.r,
            self.g,
            self.b,
            (self.a.clamp(0.0, 1.0) * 255.0).round() as u8,
        )
    }
}

pub const TRANSPARENT: Rgba = Rgba::rgb(0, 0, 0).alpha(0.0);

/// How a shape is painted.
pub enum Fill<'a> {
    Solid(Rgba),
    /// A gradient from `from` to `to` (points) through `stops` of (offset, colour).
    Linear {
        from: (f32, f32),
        to: (f32, f32),
        stops: &'a [(f32, Rgba)],
    },
}

impl Fill<'_> {
    fn paint(&self) -> Paint<'static> {
        let shader = match self {
            Self::Solid(color) => Shader::SolidColor(color.skia()),
            Self::Linear { from, to, stops } => LinearGradient::new(
                Point::from_xy(from.0, from.1),
                Point::from_xy(to.0, to.1),
                stops
                    .iter()
                    .map(|(at, color)| GradientStop::new(*at, color.skia()))
                    .collect(),
                SpreadMode::Pad,
                Transform::identity(),
            )
            .unwrap_or(Shader::SolidColor(
                stops.first().map_or(Color::TRANSPARENT, |s| s.1.skia()),
            )),
        };
        Paint {
            shader,
            anti_alias: true,
            ..Paint::default()
        }
    }
}

/// A premultiplied RGBA picture `width` × `height` points large at `scale` pixels per point.
pub struct Canvas {
    pixmap: Pixmap,
    scale: f32,
    clip: Option<Mask>,
    /// Where text is kept, in points: clipped outside, faded over the ends' lengths.
    text_bounds: Option<TextBounds>,
}

#[derive(Clone, Copy, Debug)]
pub struct TextBounds {
    pub left: f32,
    pub right: f32,
    pub fade_left: f32,
    pub fade_right: f32,
}

impl Canvas {
    pub fn new(width: f32, height: f32, scale: f32) -> Option<Self> {
        Some(Self {
            pixmap: Pixmap::new(
                (width * scale).ceil() as u32,
                (height * scale).ceil() as u32,
            )?,
            scale,
            clip: None,
            text_bounds: None,
        })
    }

    pub fn scale_of(&self) -> f32 {
        self.scale
    }

    fn transform(&self) -> Transform {
        Transform::from_scale(self.scale, self.scale)
    }

    pub fn clear(&mut self) {
        self.pixmap.fill(Color::TRANSPARENT);
    }

    /// Starts over from `background`, a canvas of the same size.
    pub fn copy(&mut self, background: &Self) {
        self.pixmap
            .data_mut()
            .copy_from_slice(background.pixmap.data());
    }

    /// Keeps what follows inside the rectangle, until `clip(None)`.
    pub fn clip(&mut self, rect: Option<(f32, f32, f32, f32)>) {
        self.clip = rect.and_then(|(x, y, w, h)| {
            let mut mask = Mask::new(self.pixmap.width(), self.pixmap.height())?;
            let path = PathBuilder::from_rect(tiny_skia::Rect::from_xywh(x, y, w, h)?);
            mask.fill_path(&path, FillRule::Winding, false, self.transform());
            Some(mask)
        });
    }

    /// Keeps what follows inside the polygon through `points`, until `clip(None)`.
    pub fn clip_to(&mut self, points: &[(f32, f32)]) {
        self.clip = polyline(points, true).and_then(|path| {
            let mut mask = Mask::new(self.pixmap.width(), self.pixmap.height())?;
            mask.fill_path(&path, FillRule::Winding, true, self.transform());
            Some(mask)
        });
    }

    pub fn polygon(&mut self, points: &[(f32, f32)], fill: &Fill) {
        let Some(path) = polyline(points, true) else {
            return;
        };
        let transform = self.transform();
        self.pixmap.fill_path(
            &path,
            &fill.paint(),
            FillRule::Winding,
            transform,
            self.clip.as_ref(),
        );
    }

    /// An open line through `points`, `width` points wide with square-cut ends.
    pub fn stroke(&mut self, points: &[(f32, f32)], width: f32, fill: &Fill) {
        let Some(path) = polyline(points, false) else {
            return;
        };
        let stroke = Stroke {
            width,
            line_cap: LineCap::Butt,
            ..Stroke::default()
        };
        let transform = self.transform();
        self.pixmap
            .stroke_path(&path, &fill.paint(), &stroke, transform, self.clip.as_ref());
    }

    pub fn rect(&mut self, (x, y, w, h): (f32, f32, f32, f32), radius: f32, fill: &Fill) {
        let Some(path) = rounded(x, y, w, h, radius) else {
            return;
        };
        let transform = self.transform();
        self.pixmap.fill_path(
            &path,
            &fill.paint(),
            FillRule::Winding,
            transform,
            self.clip.as_ref(),
        );
    }

    pub fn outline(&mut self, (x, y, w, h): (f32, f32, f32, f32), radius: f32, color: Rgba) {
        let Some(path) = rounded(x + 0.5, y + 0.5, w - 1.0, h - 1.0, radius) else {
            return;
        };
        let transform = self.transform();
        self.pixmap.stroke_path(
            &path,
            &Fill::Solid(color).paint(),
            &Stroke::default(),
            transform,
            self.clip.as_ref(),
        );
    }

    /// Draws `other`, a canvas at the same scale, with its top-left corner at `at` points.
    pub fn draw(&mut self, other: &Self, at: (f32, f32), opacity: f32) {
        let paint = PixmapPaint {
            opacity: opacity.clamp(0.0, 1.0),
            ..PixmapPaint::default()
        };
        self.pixmap.draw_pixmap(
            (at.0 * self.scale).round() as i32,
            (at.1 * self.scale).round() as i32,
            other.pixmap.as_ref(),
            &paint,
            Transform::identity(),
            self.clip.as_ref(),
        );
    }

    /// Blurs the whole canvas about `radius` points, with three box passes each way.
    pub fn blur(&mut self, radius: f32) {
        let radius = (radius * self.scale / 2.0).round().max(1.0) as usize;
        let (width, height) = (self.pixmap.width() as usize, self.pixmap.height() as usize);
        let data = self.pixmap.data_mut();
        let mut scratch = vec![0u8; data.len()];
        for _ in 0..3 {
            box_pass(data, &mut scratch, width, height, radius, 4, width * 4);
            box_pass(&scratch, data, height, width, radius, width * 4, 4);
        }
    }

    pub fn set_text_bounds(&mut self, bounds: Option<TextBounds>) {
        self.text_bounds = bounds;
    }

    /// Fills `frame` with this canvas as premultiplied BGRA.
    pub fn frame_into(&self, frame: &mut OverlayFrame) {
        frame.width = self.pixmap.width();
        frame.height = self.pixmap.height();
        frame.bgra.resize(self.pixmap.data().len(), 0);
        let pixels = self.pixmap.data().as_chunks::<4>().0;
        for (out, [r, g, b, a]) in frame.bgra.as_chunks_mut::<4>().0.iter_mut().zip(pixels) {
            *out = [*b, *g, *r, *a];
        }
    }

    /// Blends `alpha` (0–255 coverage, `width` wide) in `color` with its top-left at pixel `(x, y)`.
    fn blend(&mut self, (x, y): (i32, i32), width: usize, coverage: &[u8], color: Rgba) {
        let (cw, ch) = (self.pixmap.width() as i32, self.pixmap.height() as i32);
        let bounds = self.text_bounds;
        let scale = self.scale;
        let data = self.pixmap.data_mut();
        for (row, line) in coverage.chunks_exact(width).enumerate() {
            let py = y + row as i32;
            if py < 0 || py >= ch {
                continue;
            }
            for (column, &cover) in line.iter().enumerate() {
                let px = x + column as i32;
                if cover == 0 || px < 0 || px >= cw {
                    continue;
                }
                let fade = bounds.map_or(1.0, |b| fade_at(px as f32 / scale, b));
                let a = color.a * f32::from(cover) / 255.0 * fade;
                if a <= 0.0 {
                    continue;
                }
                let at = (py * cw + px) as usize * 4;
                let pixel = &mut data[at..at + 4];
                for (channel, value) in pixel.iter_mut().zip([color.r, color.g, color.b]) {
                    *channel =
                        (f32::from(value) * a + f32::from(*channel) * (1.0 - a)).round() as u8;
                }
                pixel[3] = (255.0 * a + f32::from(pixel[3]) * (1.0 - a)).round() as u8;
            }
        }
    }
}

/// How much of text at `x` points shows inside `bounds`.
fn fade_at(x: f32, bounds: TextBounds) -> f32 {
    if x < bounds.left || x > bounds.right {
        return 0.0;
    }
    let from_left = if bounds.fade_left > 0.0 {
        ((x - bounds.left) / bounds.fade_left).min(1.0)
    } else {
        1.0
    };
    let from_right = if bounds.fade_right > 0.0 {
        ((bounds.right - x) / bounds.fade_right).min(1.0)
    } else {
        1.0
    };
    from_left.min(from_right)
}

/// One box-blur pass along `lines` lines of `length` pixels, `step` bytes apart within a line and
/// `stride` bytes apart between lines.
fn box_pass(
    from: &[u8],
    to: &mut [u8],
    length: usize,
    lines: usize,
    radius: usize,
    step: usize,
    stride: usize,
) {
    let window = (2 * radius + 1) as u32;
    for line in 0..lines {
        let base = line * stride;
        for channel in 0..4 {
            let at = |i: usize| u32::from(from[base + i * step + channel]);
            let mut sum: u32 =
                (0..=radius).map(|i| at(i.min(length - 1))).sum::<u32>() + radius as u32 * at(0);
            for i in 0..length {
                to[base + i * step + channel] = (sum / window) as u8;
                let add = (i + radius + 1).min(length - 1);
                let drop = i.saturating_sub(radius);
                sum = sum + at(add) - at(drop);
            }
        }
    }
}

fn polyline(points: &[(f32, f32)], close: bool) -> Option<tiny_skia::Path> {
    let (first, rest) = points.split_first()?;
    let mut path = PathBuilder::new();
    path.move_to(first.0, first.1);
    for point in rest {
        path.line_to(point.0, point.1);
    }
    if close {
        path.close();
    }
    path.finish()
}

fn rounded(left: f32, top: f32, width: f32, height: f32, radius: f32) -> Option<tiny_skia::Path> {
    let (right, bottom) = (left + width, top + height);
    let corner = radius.min(width / 2.0).min(height / 2.0);
    let mut path = PathBuilder::new();
    path.move_to(left + corner, top);
    path.line_to(right - corner, top);
    path.quad_to(right, top, right, top + corner);
    path.line_to(right, bottom - corner);
    path.quad_to(right, bottom, right - corner, bottom);
    path.line_to(left + corner, bottom);
    path.quad_to(left, bottom, left, bottom - corner);
    path.line_to(left, top + corner);
    path.quad_to(left, top, left + corner, top);
    path.close();
    path.finish()
}

/// How a piece of text is set.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub size: f32,
    pub line_height: f32,
    pub weight: u16,
    pub mono: bool,
    /// Extra space after each character, in points.
    pub tracking: f32,
}

/// Where a run of text sits: one line from its left edge, or wrapped in a width, each line from
/// its left edge or centred.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Layout {
    Line,
    Wrapped { width: f32 },
    Centred { width: f32 },
}

/// The desktops' UI fonts, the first installed of which sets the overlay's text; Windows' and the
/// Linux desktops' names together, since each system has only its own.
const SANS: [&str; 6] = [
    "Segoe UI",
    "Adwaita Sans",
    "Cantarell",
    "Noto Sans",
    "DejaVu Sans",
    "Arial",
];
const MONO: [&str; 6] = [
    "Cascadia Mono",
    "Consolas",
    "Adwaita Mono",
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
];

/// The system's fonts and the glyphs drawn from them so far.
pub struct Text {
    fonts: FontSystem,
    glyphs: SwashCache,
    shaped: HashMap<String, Buffer>,
}

impl Text {
    /// Reads the system's font list; slow enough to do once, off the UI's threads.
    pub fn new() -> Self {
        let mut fonts = FontSystem::new();
        // cosmic-text assumes Open Sans and Noto Sans Mono, which many systems lack.
        let db = fonts.db_mut();
        let installed = |names: &[&str]| {
            names
                .iter()
                .find(|name| {
                    db.faces()
                        .any(|face| face.families.iter().any(|(family, _)| family == *name))
                })
                .map(|name| (*name).to_owned())
        };
        let (sans, mono) = (installed(&SANS), installed(&MONO));
        if let Some(sans) = sans {
            db.set_sans_serif_family(sans);
        }
        if let Some(mono) = mono {
            db.set_monospace_family(mono);
        }
        Self {
            fonts,
            glyphs: SwashCache::new(),
            shaped: HashMap::new(),
        }
    }

    fn shape(
        &mut self,
        spans: &[(&str, Rgba)],
        style: Style,
        layout: Layout,
        scale: f32,
    ) -> &Buffer {
        let key = format!("{spans:?}{style:?}{layout:?}{scale}");
        if !self.shaped.contains_key(&key) {
            if self.shaped.len() >= SHAPED_KEPT {
                self.shaped.clear();
            }
            let family = if style.mono {
                Family::Monospace
            } else {
                Family::SansSerif
            };
            let metrics = Metrics::new(style.size * scale, style.line_height * scale);
            let mut buffer = Buffer::new(&mut self.fonts, metrics);
            let base = Attrs::new()
                .family(family)
                .weight(Weight(self.nearest_weight(family, style.weight)))
                .letter_spacing(style.tracking / style.size);
            // Lines are centred when drawn; cosmic-text's own centring spreads the words apart.
            let width = match layout {
                Layout::Line => None,
                Layout::Wrapped { width } | Layout::Centred { width } => Some(width * scale),
            };
            buffer.set_wrap(if width.is_some() {
                Wrap::Word
            } else {
                Wrap::None
            });
            buffer.set_size(width, None);
            buffer.set_rich_text(
                spans.iter().map(|(text, color)| {
                    let color = cosmic_text::Color::rgba(
                        color.r,
                        color.g,
                        color.b,
                        (color.a * 255.0) as u8,
                    );
                    (*text, base.clone().color(color))
                }),
                &base,
                Shaping::Advanced,
                None::<Align>,
            );
            buffer.shape_until_scroll(&mut self.fonts, false);
            self.shaped.insert(key.clone(), buffer);
        }
        &self.shaped[&key]
    }

    /// The weight closest to `wanted` that `family` has. Asked for a weight its family lacks,
    /// cosmic-text takes it from another family, and the spaces from yet another.
    fn nearest_weight(&self, family: Family, wanted: u16) -> u16 {
        let fonts = self.fonts.db();
        let name = fonts.family_name(&family);
        fonts
            .faces()
            .filter(|face| face.families.iter().any(|(named, _)| named == name))
            .map(|face| face.weight.0)
            .min_by_key(|weight| weight.abs_diff(wanted))
            .unwrap_or(wanted)
    }

    /// The width of each line `spans` are set in, in points.
    pub fn line_widths(
        &mut self,
        spans: &[(&str, Rgba)],
        style: Style,
        layout: Layout,
        scale: f32,
    ) -> Vec<f32> {
        self.shape(spans, style, layout, scale)
            .layout_runs()
            .map(|run| run.line_w / scale)
            .collect()
    }

    /// The width of `spans` set on one line, and its line count's height when wrapped, in points.
    pub fn measure(
        &mut self,
        spans: &[(&str, Rgba)],
        style: Style,
        layout: Layout,
        scale: f32,
    ) -> (f32, f32) {
        let buffer = self.shape(spans, style, layout, scale);
        let (width, lines) = buffer
            .layout_runs()
            .fold((0.0f32, 0usize), |(width, lines), run| {
                (width.max(run.line_w), lines + 1)
            });
        (width / scale, lines as f32 * style.line_height)
    }

    /// Draws `spans` with the top-left of their box at `at` points.
    pub fn draw(
        &mut self,
        canvas: &mut Canvas,
        spans: &[(&str, Rgba)],
        style: Style,
        layout: Layout,
        at: (f32, f32),
    ) {
        let scale = canvas.scale;
        let origin = (at.0 * scale, at.1 * scale);
        // Kept apart from the cache, so both can be borrowed at once.
        let key = format!("{spans:?}{style:?}{layout:?}{scale}");
        self.shape(spans, style, layout, scale);
        let Some(buffer) = self.shaped.remove(&key) else {
            return;
        };
        for run in buffer.layout_runs() {
            let indent = match layout {
                Layout::Line | Layout::Wrapped { .. } => 0.0,
                Layout::Centred { width } => ((width * scale - run.line_w) / 2.0).max(0.0),
            };
            for glyph in run.glyphs {
                let physical = glyph.physical((origin.0 + indent, origin.1 + run.line_y), 1.0);
                let color = glyph.color_opt.map_or(Rgba::rgb(255, 255, 255), |c| Rgba {
                    r: c.r(),
                    g: c.g(),
                    b: c.b(),
                    a: f32::from(c.a()) / 255.0,
                });
                let Some(image) = self.glyphs.get_image(&mut self.fonts, physical.cache_key) else {
                    continue;
                };
                let place = image.placement;
                let width = place.width as usize;
                if width == 0 {
                    continue;
                }
                let top_left = (physical.x + place.left, physical.y - place.top);
                match image.content {
                    SwashContent::Mask => canvas.blend(top_left, width, &image.data, color),
                    // A colour glyph (an emoji) is drawn by its own alpha, in the text's colour.
                    SwashContent::Color | SwashContent::SubpixelMask => {
                        let alpha: Vec<u8> =
                            image.data.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
                        canvas.blend(top_left, width, &alpha, color);
                    }
                }
            }
        }
        self.shaped.insert(key, buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_in_any_weight_is_set_in_one_family() {
        let mut text = Text::new();
        for weight in [400, 600, 700] {
            let style = Style {
                size: 13.0,
                line_height: 17.0,
                weight,
                mono: false,
                tracking: 0.0,
            };
            let spans = [("Pasted into the editor. Delivered.", Rgba::rgb(0, 0, 0))];
            let buffer = text.shape(&spans, style, Layout::Line, 1.0);
            let fonts: std::collections::BTreeSet<_> = buffer
                .layout_runs()
                .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.font_id))
                .collect();
            assert_eq!(fonts.len(), 1, "weight {weight} drew from {fonts:?}");
        }
    }
    #[test]
    fn a_blur_keeps_a_filled_canvas_whole_and_spreads_a_dot_evenly_and_only_so_far() {
        let alpha = |canvas: &Canvas| {
            let mut frame = OverlayFrame {
                width: 0,
                height: 0,
                bgra: Vec::new(),
            };
            canvas.frame_into(&mut frame);
            let width = frame.width as usize;
            move |x: usize, y: usize| frame.bgra[(y * width + x) * 4 + 3]
        };
        let white = Fill::Solid(Rgba::rgb(255, 255, 255));
        // The second blur reaches past the whole canvas.
        for (width, height, radius) in [(12, 9, 4.0), (2, 3, 8.0)] {
            let mut full = Canvas::new(width as f32, height as f32, 1.0).unwrap();
            full.rect((0.0, 0.0, width as f32, height as f32), 0.0, &white);
            full.blur(radius);
            let at = alpha(&full);
            for y in 0..height {
                for x in 0..width {
                    assert_eq!(at(x, y), 255, "{width} × {height}: ({x}, {y})");
                }
            }
        }
        let mut dot = Canvas::new(21.0, 21.0, 1.0).unwrap();
        dot.rect((10.0, 10.0, 1.0, 1.0), 0.0, &white);
        dot.blur(2.0);
        let at = alpha(&dot);
        assert!(at(10, 10) > 0 && at(10, 10) < 255);
        for step in 1..=3 {
            let around = [
                at(10 - step, 10),
                at(10 + step, 10),
                at(10, 10 - step),
                at(10, 10 + step),
            ];
            assert!(
                around.iter().all(|a| *a == around[0] && *a > 0),
                "{step}: {around:?}"
            );
            assert!(around[0] < at(10 - step + 1, 10), "{step}");
        }
        assert_eq!((at(6, 10), at(14, 10), at(10, 6), at(10, 14)), (0, 0, 0, 0));
        // Three passes of a 2-point blur reach 6 pixels past an edge at scale 2.
        let mut fine = Canvas::new(21.0, 21.0, 2.0).unwrap();
        fine.rect((0.0, 0.0, 10.5, 21.0), 0.0, &white);
        fine.blur(2.0);
        let at = alpha(&fine);
        assert!(at(26, 20) > 0 && at(27, 20) == 0, "the radius is in points");
    }
    const WHITE: Rgba = Rgba::rgb(255, 255, 255);

    /// A drawing, the canvas's scale, and the pixels it must cover and leave empty.
    type Case<'a> = (
        &'a str,
        f32,
        Box<dyn Fn(&mut Canvas) + 'a>,
        &'a [(u32, u32)],
        &'a [(u32, u32)],
    );

    fn alpha(canvas: &Canvas, x: u32, y: u32) -> u8 {
        canvas.pixmap.pixel(x, y).unwrap().alpha()
    }

    #[test]
    fn each_shape_covers_its_own_area_and_nothing_else() {
        let white = Fill::Solid(WHITE);
        let mut square = Canvas::new(4.0, 4.0, 1.0).unwrap();
        square.rect((0.0, 0.0, 4.0, 4.0), 0.0, &white);
        let mut fine = Canvas::new(2.0, 2.0, 2.0).unwrap();
        fine.rect((0.0, 0.0, 2.0, 2.0), 0.0, &white);
        let triangle = [(0.0, 0.0), (12.0, 0.0), (0.0, 12.0)];
        let full = (0.0, 0.0, 12.0, 12.0);
        let cases: [Case; 11] = [
            (
                "rect",
                1.0,
                Box::new(|c| c.rect((2.0, 2.0, 6.0, 4.0), 0.0, &white)),
                &[(2, 2), (7, 5)],
                &[(1, 3), (8, 3), (4, 6)],
            ),
            (
                "rounded",
                1.0,
                Box::new(|c| c.rect((2.0, 2.0, 8.0, 8.0), 3.0, &white)),
                &[(6, 6), (2, 5), (5, 2), (9, 5), (5, 9)],
                &[(10, 2), (9, 1), (1, 9), (2, 10)],
            ),
            (
                "outline",
                1.0,
                Box::new(|c| c.outline((1.0, 1.0, 10.0, 10.0), 0.0, WHITE)),
                &[(1, 6), (10, 6), (6, 1), (6, 10)],
                &[(6, 6), (0, 6), (11, 6), (6, 11)],
            ),
            (
                "polygon",
                1.0,
                Box::new(|c| c.polygon(&triangle, &white)),
                &[(2, 2)],
                &[(9, 9)],
            ),
            (
                "stroke",
                1.0,
                Box::new(|c| c.stroke(&[(2.0, 6.0), (10.0, 6.0)], 2.0, &white)),
                &[(2, 6), (9, 5)],
                &[(6, 8), (1, 6), (10, 6)],
            ),
            (
                "clip",
                1.0,
                Box::new(|c| {
                    c.clip(Some((0.0, 0.0, 6.0, 12.0)));
                    c.rect(full, 0.0, &white);
                }),
                &[(2, 6)],
                &[(8, 6)],
            ),
            (
                "unclip",
                1.0,
                Box::new(|c| {
                    c.clip(Some((0.0, 0.0, 6.0, 12.0)));
                    c.clip(None);
                    c.rect(full, 0.0, &white);
                }),
                &[(2, 6), (8, 6)],
                &[],
            ),
            (
                "clip_to",
                1.0,
                Box::new(|c| {
                    c.clip_to(&triangle);
                    c.rect(full, 0.0, &white);
                }),
                &[(2, 2)],
                &[(9, 9)],
            ),
            (
                "draw",
                1.0,
                Box::new(|c| c.draw(&square, (3.0, 3.0), 1.0)),
                &[(3, 3), (6, 6)],
                &[(2, 2), (7, 7)],
            ),
            (
                "draw at scale",
                2.0,
                Box::new(|c| c.draw(&fine, (1.5, 1.5), 1.0)),
                &[(3, 3), (6, 6)],
                &[(2, 2), (7, 7)],
            ),
            (
                "scale",
                2.0,
                Box::new(|c| c.rect((0.0, 0.0, 3.0, 3.0), 0.0, &white)),
                &[(5, 5)],
                &[(6, 6)],
            ),
        ];
        covers(cases);
    }

    #[test]
    fn a_corner_too_big_for_its_shape_rounds_it_into_a_pill() {
        let white = Fill::Solid(WHITE);
        let cases: [Case; 2] = [
            (
                "wide pill",
                1.0,
                Box::new(|c| c.rect((1.0, 4.0, 10.0, 4.0), 10.0, &white)),
                &[(3, 4), (8, 4), (3, 7), (8, 7), (2, 5), (9, 6)],
                &[(0, 5), (11, 5), (2, 3), (9, 3), (2, 8), (9, 8)],
            ),
            (
                "tall pill",
                1.0,
                Box::new(|c| c.rect((4.0, 1.0, 4.0, 10.0), 10.0, &white)),
                &[(4, 3), (4, 8), (7, 3), (7, 8), (5, 2), (6, 9)],
                &[(5, 0), (5, 11), (3, 2), (3, 9), (8, 2), (8, 9)],
            ),
        ];
        covers(cases);
    }

    /// Draws each case on a 12-point canvas and checks the pixels it must fill and leave empty.
    fn covers<const N: usize>(cases: [Case; N]) {
        for (name, scale, draw, filled, empty) in cases {
            let mut canvas = Canvas::new(12.0 / scale, 12.0 / scale, scale).unwrap();
            assert!((canvas.scale_of() - scale).abs() < f32::EPSILON);
            draw(&mut canvas);
            for &(x, y) in filled {
                assert_eq!(alpha(&canvas, x, y), 255, "{name} ({x}, {y})");
            }
            for &(x, y) in empty {
                assert_eq!(alpha(&canvas, x, y), 0, "{name} ({x}, {y})");
            }
        }
    }

    #[test]
    fn a_canvas_is_drawn_at_its_opacity_copied_whole_and_cleared() {
        let mut square = Canvas::new(4.0, 4.0, 1.0).unwrap();
        square.rect((0.0, 0.0, 4.0, 4.0), 0.0, &Fill::Solid(WHITE));
        let mut canvas = Canvas::new(4.0, 4.0, 1.0).unwrap();
        canvas.draw(&square, (0.0, 0.0), 0.5);
        assert!(alpha(&canvas, 1, 1).abs_diff(128) <= 1);
        canvas.clear();
        canvas.draw(&square, (0.0, 0.0), 3.0);
        assert_eq!(alpha(&canvas, 1, 1), 255, "opacity stops at one");
        canvas.clear();
        assert_eq!(alpha(&canvas, 1, 1), 0);
        canvas.copy(&square);
        assert_eq!(canvas.pixmap.data(), square.pixmap.data());
        canvas.clear();
        canvas.rect((0.0, 0.0, 4.0, 4.0), 0.0, &Fill::Solid(WHITE.alpha(0.5)));
        assert!(
            alpha(&canvas, 1, 1).abs_diff(128) <= 1,
            "a fill keeps its colour's opacity"
        );
    }

    #[test]
    fn text_fades_in_from_the_edges_of_its_bounds_and_vanishes_past_them() {
        let bounds = TextBounds {
            left: 10.0,
            right: 50.0,
            fade_left: 4.0,
            fade_right: 8.0,
        };
        let sharp = TextBounds {
            fade_left: 0.0,
            fade_right: 0.0,
            ..bounds
        };
        for (x, bounds, shown) in [
            (9.0, bounds, 0.0),
            (51.0, bounds, 0.0),
            (12.0, bounds, 0.5),
            (46.0, bounds, 0.5),
            (30.0, bounds, 1.0),
            (10.0, sharp, 1.0),
            (50.0, sharp, 1.0),
        ] {
            assert!(
                (fade_at(x, bounds) - shown).abs() < 1e-6,
                "{x} in {bounds:?}"
            );
        }
    }

    #[test]
    fn text_blends_its_colour_by_coverage_over_what_is_there_and_only_in_bounds() {
        let raw = |canvas: &Canvas, x: usize| -> [u8; 4] {
            canvas.pixmap.data()[x * 4..x * 4 + 4].try_into().unwrap()
        };
        let mut canvas = Canvas::new(3.0, 1.0, 1.0).unwrap();
        canvas.pixmap.data_mut()[..8].copy_from_slice(&[100, 100, 100, 255, 100, 100, 100, 100]);
        let color = Rgba {
            r: 200,
            g: 0,
            b: 50,
            a: 0.5,
        };
        canvas.blend((0, 0), 3, &[255, 128, 0], color);
        assert_eq!(raw(&canvas, 0), [150, 50, 75, 255], "over an opaque pixel");
        assert_eq!(
            raw(&canvas, 1),
            [125, 75, 87, 139],
            "partly covered, over a faint pixel"
        );
        assert_eq!(raw(&canvas, 2), [0, 0, 0, 0], "no coverage");
        let mut edges = Canvas::new(4.0, 3.0, 1.0).unwrap();
        edges.blend((-1, -1), 6, &[255; 30], WHITE);
        assert!(
            edges.pixmap.data().iter().all(|byte| *byte == 255),
            "covered exactly once, nothing drawn past the edges"
        );
        let mut scaled = Canvas::new(2.0, 1.0, 2.0).unwrap();
        scaled.set_text_bounds(Some(TextBounds {
            left: 0.0,
            right: 1.0,
            fade_left: 0.0,
            fade_right: 0.0,
        }));
        scaled.blend((0, 0), 4, &[255; 4], WHITE);
        let alphas: Vec<u8> = (0..4).map(|x| raw(&scaled, x)[3]).collect();
        assert_eq!(alphas, [255, 255, 255, 0], "bounds are in points");
    }

    #[test]
    fn text_measures_wider_when_longer_wraps_to_its_width_and_centres_when_asked() {
        let mut text = Text::new();
        let style = Style {
            size: 13.0,
            line_height: 17.0,
            weight: 400,
            mono: false,
            tracking: 0.0,
        };
        let spans = |words: &'static str| [(words, WHITE)];
        let (short, _) = text.measure(&spans("Hello"), style, Layout::Line, 1.0);
        let (long, height) = text.measure(&spans("Hello there"), style, Layout::Line, 1.0);
        assert!(
            long > short + 10.0 && (height - 17.0).abs() < 1e-3,
            "{short} {long} {height}"
        );
        let (double, _) = text.measure(
            &spans("Hello"),
            Style {
                size: 26.0,
                ..style
            },
            Layout::Line,
            2.0,
        );
        assert!((double - 2.0 * short).abs() < 2.0, "{short} {double}");
        let sentence = spans("The overlay keeps the newest words of a long sentence in view");
        let wrapped = Layout::Wrapped { width: 120.0 };
        let widths = text.line_widths(&sentence, style, wrapped, 1.0);
        assert!(
            widths.len() > 2 && widths.iter().all(|w| *w <= 120.0),
            "{widths:?}"
        );
        let (_, tall) = text.measure(&sentence, style, wrapped, 1.0);
        #[expect(clippy::cast_precision_loss, reason = "a few lines")]
        let lines = widths.len() as f32;
        assert!((tall - lines * 17.0).abs() < 1e-3);
        let fine = text.line_widths(&sentence, style, wrapped, 2.0);
        assert_eq!(fine.len(), widths.len(), "wraps the same at scale 2");
        assert!(
            fine.iter().zip(&widths).all(|(a, b)| (a - b).abs() < 2.0),
            "{fine:?}"
        );
        // The leftmost, rightmost and topmost inked pixel, divided by the scale.
        let mut inked = |layout, scale: f32| {
            let mut canvas = Canvas::new(200.0, 40.0, scale).unwrap();
            text.draw(&mut canvas, &spans("Hi"), style, layout, (20.0, 5.0));
            let (w, h) = (canvas.pixmap.width(), canvas.pixmap.height());
            let ink: Vec<(u32, u32)> = (0..h)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .filter(|(x, y)| alpha(&canvas, *x, *y) > 0)
                .collect();
            let points = |v: u32| f32::from(u16::try_from(v).unwrap()) / scale;
            (
                points(ink.iter().map(|p| p.0).min().unwrap()),
                points(ink.iter().map(|p| p.0).max().unwrap()),
                points(ink.iter().map(|p| p.1).min().unwrap()),
            )
        };
        let (left, _, top) = inked(Layout::Line, 1.0);
        assert!((20.0..24.0).contains(&left), "{left}");
        let (fine_left, _, fine_top) = inked(Layout::Line, 2.0);
        assert!(
            (fine_left - left).abs() <= 1.0 && (fine_top - top).abs() <= 1.0,
            "at scale 2: {fine_left} {fine_top}, at 1: {left} {top}"
        );
        for scale in [1.0, 2.0] {
            let (left, right, _) = inked(Layout::Centred { width: 160.0 }, scale);
            assert!(
                (left + right - 200.0).abs() <= 3.0,
                "{scale}: {left}..{right}"
            );
        }
    }

    #[test]
    fn text_keeps_its_tracking_weight_and_transparency() {
        let mut text = Text::new();
        let style = Style {
            size: 13.0,
            line_height: 17.0,
            weight: 400,
            mono: false,
            tracking: 0.0,
        };
        let hello = [("Hello", WHITE)];
        let (plain, _) = text.measure(&hello, style, Layout::Line, 1.0);
        let (spaced, _) = text.measure(
            &hello,
            Style {
                tracking: 2.0,
                ..style
            },
            Layout::Line,
            1.0,
        );
        assert!(
            (spaced - plain - 10.0).abs() <= 2.0,
            "2 points after each of 5 letters: {plain} {spaced}"
        );
        let ink = |text: &mut Text, spans: &[(&str, Rgba)], style: Style| {
            let mut canvas = Canvas::new(120.0, 30.0, 1.0).unwrap();
            text.draw(&mut canvas, spans, style, Layout::Line, (4.0, 4.0));
            canvas
                .pixmap
                .pixels()
                .iter()
                .map(|p| u32::from(p.alpha()))
                .collect::<Vec<_>>()
        };
        let regular: u32 = ink(&mut text, &hello, style).iter().sum();
        let bold: u32 = ink(
            &mut text,
            &hello,
            Style {
                weight: 700,
                ..style
            },
        )
        .iter()
        .sum();
        let name = text.fonts.db().family_name(&Family::SansSerif).to_owned();
        let installed = text
            .fonts
            .db()
            .faces()
            .any(|face| face.families.iter().any(|(family, _)| *family == name));
        let heavier = installed
            && text.nearest_weight(Family::SansSerif, 700)
                > text.nearest_weight(Family::SansSerif, 400);
        assert!(
            !heavier || bold * 10 > regular * 11,
            "bold is heavier where the family has a heavier face: {regular} {bold}"
        );
        let strongest = |ink: Vec<u32>| ink.into_iter().max().unwrap();
        let opaque = strongest(ink(&mut text, &hello, style));
        let faint = strongest(ink(&mut text, &[("Hello", WHITE.alpha(0.5))], style));
        assert!(
            faint * 2 >= opaque.saturating_sub(4) && faint * 2 <= opaque + 4,
            "half-transparent text: {faint} of {opaque}"
        );
    }

    #[test]
    fn a_weight_is_the_nearest_one_the_family_has() {
        let text = Text::new();
        let fonts = text.fonts.db();
        let name = fonts.family_name(&Family::SansSerif);
        let has: Vec<u16> = fonts
            .faces()
            .filter(|face| face.families.iter().any(|(named, _)| named == name))
            .map(|face| face.weight.0)
            .collect();
        for wanted in [1, 400, 650, 1000] {
            let got = text.nearest_weight(Family::SansSerif, wanted);
            // A family that is not installed keeps the weight asked for.
            let Some(best) = has.iter().map(|weight| weight.abs_diff(wanted)).min() else {
                assert_eq!(got, wanted, "{name} is not installed");
                continue;
            };
            assert!(
                has.contains(&got) && got.abs_diff(wanted) == best,
                "{wanted}: {got} from {has:?}"
            );
        }
    }
}
