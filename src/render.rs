//! CryoLock rendering engine — tiny-skia compositing + fontdue text rasterisation.
//!
//! Draws the lock screen UI: background colour, clock, authentication ring
//! indicator, password dots, and status text. All rendering is CPU-based for
//! maximum portability across Wayland compositors.

use std::process::Command;

use log::{info, warn};
use tiny_skia::{Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::config::{self, Config};

// ---------------------------------------------------------------------------
// Visual state machine
// ---------------------------------------------------------------------------

/// The current visual state of the lock screen input indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputState {
    /// No input — ring shows idle colour.
    Idle,
    /// User is typing — ring shows typing colour.
    Typing,
    /// Password submitted, waiting for PAM — ring pulses typing colour.
    Verifying,
    /// Authentication failed — ring flashes wrong colour.
    Wrong,
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

/// Holds the loaded font and provides the main render entry point.
pub struct Renderer {
    font: Option<fontdue::Font>,
}

impl Renderer {
    /// Create a new renderer, loading the specified system font family.
    pub fn new(font_family: &str) -> Self {
        let font = load_system_font(font_family);
        if font.is_some() {
            info!("Font loaded for family '{font_family}'");
        } else {
            warn!("Could not load font '{font_family}' — text will not render");
        }
        Self { font }
    }

    /// Render a complete lock screen frame into an ARGB8888 Wayland SHM buffer.
    ///
    /// `canvas` must be exactly `width * height * 4` bytes.
    pub fn render_frame(
        &self,
        canvas: &mut [u8],
        width: u32,
        height: u32,
        config: &Config,
        state: InputState,
        password_len: usize,
    ) {
        let Some(mut pixmap) = Pixmap::new(width, height) else {
            return;
        };

        // 1. Background fill ------------------------------------------------
        let bg = parse_color(&config.background_color)
            .unwrap_or(Color::from_rgba8(0x1a, 0x1b, 0x26, 0xFF));
        pixmap.fill(bg);

        let cx = width as f32 / 2.0;
        let cy = height as f32 / 2.0;
        // Proportional ring sizing — looks good from 720p to 4K.
        let ring_radius = (width.min(height) as f32 * 0.1).clamp(60.0, 150.0);
        let ring_stroke = (ring_radius * 0.06).clamp(3.0, 8.0);

        // 2. Ring indicator -------------------------------------------------
        let ring_color = match state {
            InputState::Idle => parse_color(&config.ring_idle_color),
            InputState::Typing => parse_color(&config.ring_typing_color),
            InputState::Verifying => parse_color(&config.ring_typing_color),
            InputState::Wrong => parse_color(&config.ring_wrong_color),
        }
        .unwrap_or(Color::from_rgba8(0x56, 0x5f, 0x89, 0xFF));

        draw_ring(&mut pixmap, cx, cy, ring_radius, ring_stroke, ring_color);

        // 3. Password dots (inside the ring, horizontal row) ----------------
        if password_len > 0 && state != InputState::Wrong {
            let dot_color = parse_color(&config.text_color)
                .unwrap_or(Color::from_rgba8(0xc0, 0xca, 0xf5, 0xFF));
            draw_password_dots(&mut pixmap, cx, cy, password_len, ring_radius, dot_color);
        }

        // 4. Clock text (centred above the ring) ---------------------------
        if config.show_clock {
            if let Some(ref font) = self.font {
                let time_str = chrono::Local::now()
                    .format(&config.clock_format)
                    .to_string();
                let text_color = parse_color(&config.text_color)
                    .unwrap_or(Color::from_rgba8(0xc0, 0xca, 0xf5, 0xFF));
                let clock_y = cy - ring_radius - ring_stroke - 30.0;
                draw_text(
                    &mut pixmap,
                    font,
                    &time_str,
                    config.font_size as f32,
                    cx,
                    clock_y,
                    text_color,
                );
            }
        }

        // 5. Status text (centred below the ring) --------------------------
        let status = match state {
            InputState::Idle | InputState::Typing => "",
            InputState::Verifying => "verifying\u{2026}",
            InputState::Wrong => "authentication failed",
        };
        if !status.is_empty() {
            if let Some(ref font) = self.font {
                let status_color = match state {
                    InputState::Wrong => ring_color,
                    _ => parse_color(&config.text_color)
                        .unwrap_or(Color::from_rgba8(0xc0, 0xca, 0xf5, 0xFF)),
                };
                let status_y = cy + ring_radius + ring_stroke + 40.0;
                let status_size = (config.font_size as f32 * 0.35).max(16.0);
                draw_text(
                    &mut pixmap,
                    font,
                    status,
                    status_size,
                    cx,
                    status_y,
                    status_color,
                );
            }
        }

        // 6. Convert premultiplied RGBA → ARGB8888 (Wayland LE) ------------
        rgba_to_argb8888(pixmap.data(), canvas);
    }
}

// ---------------------------------------------------------------------------
// Font loading
// ---------------------------------------------------------------------------

/// Load a system font by family name using fontconfig's `fc-match`.
fn load_system_font(family: &str) -> Option<fontdue::Font> {
    let path = fc_match_font(family)
        .or_else(|| fc_match_font("monospace"))
        .or_else(|| fc_match_font("sans-serif"))
        .or_else(find_fallback_font)?;

    info!("Loading font from: {path}");
    let data = std::fs::read(&path).ok()?;
    fontdue::Font::from_bytes(data, fontdue::FontSettings::default()).ok()
}

/// Query fontconfig for a font file path.
fn fc_match_font(pattern: &str) -> Option<String> {
    let output = Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() || !std::path::Path::new(&path).exists() {
        None
    } else {
        Some(path)
    }
}

/// Hardcoded fallback font paths for systems without fontconfig.
fn find_fallback_font() -> Option<String> {
    let candidates = [
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf",
        "/usr/share/fonts/TTF/Hack-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    ];
    candidates
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Drawing primitives
// ---------------------------------------------------------------------------

/// Draw a circle ring (stroked, not filled) centred at (cx, cy).
fn draw_ring(pixmap: &mut Pixmap, cx: f32, cy: f32, radius: f32, stroke_width: f32, color: Color) {
    let Some(path) = circle_path(cx, cy, radius) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let stroke = Stroke {
        width: stroke_width,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

/// Draw password dots in a horizontal row centred at (cx, cy).
fn draw_password_dots(
    pixmap: &mut Pixmap,
    cx: f32,
    cy: f32,
    count: usize,
    ring_radius: f32,
    color: Color,
) {
    let dot_radius = (ring_radius * 0.055).clamp(3.0, 7.0);
    let spacing = dot_radius * 3.5;
    let max_visible = ((ring_radius * 1.4) / spacing).max(1.0) as usize;
    let visible = count.min(max_visible);

    let total_width = (visible as f32 - 1.0).max(0.0) * spacing;
    let start_x = cx - total_width / 2.0;

    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;

    for i in 0..visible {
        let dx = start_x + i as f32 * spacing;
        if let Some(path) = circle_path(dx, cy, dot_radius) {
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
}

/// Draw centred text. `cx` = horizontal centre, `cy` = vertical centre of the
/// text bounding box.
fn draw_text(
    pixmap: &mut Pixmap,
    font: &fontdue::Font,
    text: &str,
    size: f32,
    cx: f32,
    cy: f32,
    color: Color,
) {
    if text.is_empty() {
        return;
    }

    // Pre-rasterise every glyph and collect layout metadata.
    struct GlyphInfo {
        bitmap: Vec<u8>,
        width: usize,
        height: usize,
        xmin: i32,
        ymin: i32,
        advance_width: f32,
    }

    let glyphs: Vec<GlyphInfo> = text
        .chars()
        .map(|ch| {
            let (m, bitmap) = font.rasterize(ch, size);
            GlyphInfo {
                bitmap,
                width: m.width,
                height: m.height,
                xmin: m.xmin,
                ymin: m.ymin,
                advance_width: m.advance_width,
            }
        })
        .collect();

    let total_advance: f32 = glyphs.iter().map(|g| g.advance_width).sum();

    // Vertical bounding box relative to baseline (y-up convention):
    //   ascent  = max(ymin + height) — highest pixel above baseline
    //   descent = min(ymin)          — lowest pixel (negative = below baseline)
    let ascent = glyphs
        .iter()
        .filter(|g| g.height > 0)
        .map(|g| g.ymin + g.height as i32)
        .max()
        .unwrap_or(0);
    let descent = glyphs
        .iter()
        .filter(|g| g.height > 0)
        .map(|g| g.ymin)
        .min()
        .unwrap_or(0);

    // Place baseline so the bounding box is vertically centred on `cy`.
    let baseline_y = cy + (ascent + descent) as f32 / 2.0;

    // Horizontally centred pen.
    let mut pen_x = cx - total_advance / 2.0;

    // Decompose colour for the alpha-blending loop (these are premultiplied
    // values from tiny-skia, but with a=255 they equal the straight values).
    let cr = (color.red() * 255.0) as u8;
    let cg = (color.green() * 255.0) as u8;
    let cb = (color.blue() * 255.0) as u8;
    let ca = (color.alpha() * 255.0) as u8;

    for g in &glyphs {
        if g.width > 0 && g.height > 0 {
            let gx = pen_x as i32 + g.xmin;
            let gy = baseline_y as i32 - g.ymin - g.height as i32;
            blit_glyph(pixmap, &g.bitmap, g.width, g.height, gx, gy, cr, cg, cb, ca);
        }
        pen_x += g.advance_width;
    }
}

/// Blit a single-channel coverage bitmap onto the pixmap with alpha blending.
#[allow(clippy::too_many_arguments)]
fn blit_glyph(
    pixmap: &mut Pixmap,
    bitmap: &[u8],
    gw: usize,
    gh: usize,
    gx: i32,
    gy: i32,
    r: u8,
    g: u8,
    b: u8,
    a: u8,
) {
    let pw = pixmap.width() as i32;
    let ph = pixmap.height() as i32;
    let data = pixmap.data_mut();

    for row in 0..gh {
        let py = gy + row as i32;
        if py < 0 || py >= ph {
            continue;
        }
        for col in 0..gw {
            let px = gx + col as i32;
            if px < 0 || px >= pw {
                continue;
            }

            let coverage = bitmap[row * gw + col];
            if coverage == 0 {
                continue;
            }

            // Effective alpha = font alpha × glyph coverage.
            let alpha = (a as u16 * coverage as u16) / 255;
            let inv = 255 - alpha;

            // Premultiplied source.
            let sr = (r as u16 * alpha) / 255;
            let sg = (g as u16 * alpha) / 255;
            let sb = (b as u16 * alpha) / 255;

            // Blend over existing (premultiplied RGBA) pixel.
            let idx = (py as usize * pw as usize + px as usize) * 4;
            data[idx] = ((data[idx] as u16 * inv) / 255 + sr) as u8; // R
            data[idx + 1] = ((data[idx + 1] as u16 * inv) / 255 + sg) as u8; // G
            data[idx + 2] = ((data[idx + 2] as u16 * inv) / 255 + sb) as u8; // B
            data[idx + 3] = ((data[idx + 3] as u16 * inv) / 255 + alpha) as u8; // A
        }
    }
}

/// Construct a circle path using four cubic Bézier curves (standard kappa
/// approximation).
fn circle_path(cx: f32, cy: f32, r: f32) -> Option<tiny_skia::Path> {
    const K: f32 = 0.552_284_8; // kappa for circular arc
    let mut pb = PathBuilder::new();
    pb.move_to(cx + r, cy);
    pb.cubic_to(cx + r, cy + r * K, cx + r * K, cy + r, cx, cy + r);
    pb.cubic_to(cx - r * K, cy + r, cx - r, cy + r * K, cx - r, cy);
    pb.cubic_to(cx - r, cy - r * K, cx - r * K, cy - r, cx, cy - r);
    pb.cubic_to(cx + r * K, cy - r, cx + r, cy - r * K, cx + r, cy);
    pb.close();
    pb.finish()
}

// ---------------------------------------------------------------------------
// Colour helpers
// ---------------------------------------------------------------------------

/// Parse a hex colour string into a tiny-skia `Color`.
fn parse_color(hex: &str) -> Option<Color> {
    let (r, g, b) = config::parse_hex_color(hex)?;
    Some(Color::from_rgba8(r, g, b, 255))
}

/// Convert premultiplied RGBA pixels (tiny-skia) → ARGB8888 pixels (Wayland LE).
///
/// tiny-skia byte order: `[R, G, B, A]`
/// Wayland ARGB8888 LE:  `[B, G, R, A]`
fn rgba_to_argb8888(src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        d[0] = s[2]; // B
        d[1] = s[1]; // G
        d[2] = s[0]; // R
        d[3] = s[3]; // A
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circle_path_is_valid() {
        let path = circle_path(100.0, 100.0, 50.0);
        assert!(path.is_some(), "circle_path should produce a valid path");
    }

    #[test]
    fn rgba_to_argb8888_swaps_correctly() {
        let src = [0xAA, 0xBB, 0xCC, 0xFF]; // R=AA G=BB B=CC A=FF
        let mut dst = [0u8; 4];
        rgba_to_argb8888(&src, &mut dst);
        assert_eq!(dst, [0xCC, 0xBB, 0xAA, 0xFF]); // B=CC G=BB R=AA A=FF
    }

    #[test]
    fn parse_color_valid() {
        let c = parse_color("#1a1b26");
        assert!(c.is_some());
    }
}
