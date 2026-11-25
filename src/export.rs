//! "Save chart as PNG": charts are re-rendered off-screen with tiny-skia (no screen capture).

use std::f32::consts::TAU;
use std::path::{Path, PathBuf};

use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};
use gpui::{Hsla, Rgba};
use tiny_skia::{
    FillRule, LineCap, LineJoin, Mask, Paint, PathBuilder, Pixmap, Stroke, Transform,
};

use crate::charts::{ChartSpec, DonutRow, LegendItem, LineSeries};
use crate::theme::Theme;

const SCALE: f32 = 2.0; // output is 2x for crisp text

// ------------------------------------------------------------------ fonts --

struct Fonts {
    regular: Option<FontVec>,
    bold: Option<FontVec>,
}

fn load_first(paths: &[String]) -> Option<FontVec> {
    paths.iter().find_map(|p| std::fs::read(p).ok().and_then(|d| FontVec::try_from_vec(d).ok()))
}

fn load_fonts() -> Fonts {
    let win = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let s = |v: &[&str]| v.iter().map(|x| x.replace("{WIN}", &win)).collect::<Vec<_>>();
    let regular = load_first(&s(&[
        "{WIN}\\Fonts\\segoeui.ttf",
        "{WIN}\\Fonts\\arial.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/Library/Fonts/Arial.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    ]));
    let bold = load_first(&s(&[
        "{WIN}\\Fonts\\segoeuib.ttf",
        "{WIN}\\Fonts\\arialbd.ttf",
        "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
        "/Library/Fonts/Arial Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    ]));
    Fonts { regular, bold }
}

// ---------------------------------------------------------------- canvas --

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
    Center,
}

struct Canvas {
    pm: Pixmap,
    fonts: Fonts,
}

fn color(h: Hsla, alpha: f32) -> tiny_skia::Color {
    let c: Rgba = h.into();
    tiny_skia::Color::from_rgba(c.r, c.g, c.b, (c.a * alpha).clamp(0.0, 1.0)).unwrap()
}

fn paint(c: tiny_skia::Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(c);
    p.anti_alias = true;
    p
}

impl Canvas {
    fn new(w: f32, h: f32, fonts: Fonts) -> Self {
        Self { pm: Pixmap::new((w * SCALE) as u32, (h * SCALE) as u32).unwrap(), fonts }
    }

    fn font(&self, bold: bool) -> Option<&FontVec> {
        if bold { self.fonts.bold.as_ref().or(self.fonts.regular.as_ref()) } else { self.fonts.regular.as_ref() }
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: tiny_skia::Color) {
        if let Some(r) = tiny_skia::Rect::from_xywh(x * SCALE, y * SCALE, w * SCALE, h * SCALE) {
            self.pm.fill_rect(r, &paint(c), Transform::identity(), None);
        }
    }

    fn line(&mut self, a: (f32, f32), b: (f32, f32), w: f32, c: tiny_skia::Color) {
        let mut pb = PathBuilder::new();
        pb.move_to(a.0 * SCALE, a.1 * SCALE);
        pb.line_to(b.0 * SCALE, b.1 * SCALE);
        if let Some(p) = pb.finish() {
            let st = Stroke { width: w * SCALE, ..Default::default() };
            self.pm.stroke_path(&p, &paint(c), &st, Transform::identity(), None);
        }
    }

    fn polygon(&mut self, pts: &[(f32, f32)], c: tiny_skia::Color) {
        let mut pb = PathBuilder::new();
        for (i, p) in pts.iter().enumerate() {
            if i == 0 { pb.move_to(p.0 * SCALE, p.1 * SCALE) } else { pb.line_to(p.0 * SCALE, p.1 * SCALE) }
        }
        pb.close();
        if let Some(p) = pb.finish() {
            self.pm.fill_path(&p, &paint(c), FillRule::Winding, Transform::identity(), None);
        }
    }

    fn text_width(&self, s: &str, size: f32, bold: bool) -> f32 {
        let Some(f) = self.font(bold) else { return 0.0 };
        let sf = f.as_scaled(PxScale::from(size * SCALE));
        s.chars().map(|ch| sf.h_advance(f.glyph_id(ch))).sum::<f32>() / SCALE
    }

    /// `y` is the top of the text line.
    fn text(&mut self, s: &str, x: f32, y: f32, size: f32, bold: bool, align: Align, c: tiny_skia::Color) {
        let w = self.text_width(s, size, bold);
        let x = match align {
            Align::Left => x,
            Align::Right => x - w,
            Align::Center => x - w / 2.0,
        };
        let Some(font) = self.font(bold) else { return };
        let scale = PxScale::from(size * SCALE);
        let sf = font.as_scaled(scale);
        let baseline = y * SCALE + sf.ascent();
        let mut cx = x * SCALE;
        let (pw, ph) = (self.pm.width() as i32, self.pm.height() as i32);
        let mut px: Vec<(i32, i32, f32)> = Vec::new();
        for ch in s.chars() {
            let id = font.glyph_id(ch);
            let g = id.with_scale_and_position(scale, point(cx, baseline));
            if let Some(og) = font.outline_glyph(g) {
                let b = og.px_bounds();
                og.draw(|gx, gy, cov| px.push((b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32, cov)));
            }
            cx += sf.h_advance(id);
        }
        let data = self.pm.data_mut();
        for (x, y, cov) in px {
            if x < 0 || y < 0 || x >= pw || y >= ph {
                continue;
            }
            let i = ((y * pw + x) * 4) as usize;
            let sa = (c.alpha() * cov).clamp(0.0, 1.0);
            let src = [c.red(), c.green(), c.blue()];
            for k in 0..3 {
                let d = data[i + k] as f32 / 255.0;
                data[i + k] = ((src[k] * sa + d * (1.0 - sa)) * 255.0).round() as u8;
            }
            let da = data[i + 3] as f32 / 255.0;
            data[i + 3] = ((sa + da * (1.0 - sa)) * 255.0).round() as u8;
        }
    }
}

// ----------------------------------------------------------------- charts --

const PAD: f32 = 24.0;

fn frame(cv: &mut Canvas, w: f32, h: f32, t: &Theme) {
    cv.rect(0.0, 0.0, w, h, color(t.surface, 1.0));
    cv.rect(0.0, 0.0, w, 1.0, color(t.border, 1.0));
    cv.rect(0.0, h - 1.0, w, 1.0, color(t.border, 1.0));
    cv.rect(0.0, 0.0, 1.0, h, color(t.border, 1.0));
    cv.rect(w - 1.0, 0.0, 1.0, h, color(t.border, 1.0));
}

fn render_line(
    title: &str,
    legend: &[LegendItem],
    series: &[LineSeries],
    y_max: f32,
    fmt: fn(f32) -> String,
    window_len: usize,
    t: &Theme,
) -> Pixmap {
    let (w, h) = (960.0, 480.0);
    let mut cv = Canvas::new(w, h, load_fonts());
    frame(&mut cv, w, h, t);
    cv.text(title, PAD, 18.0, 18.0, true, Align::Left, color(t.text, 1.0));

    // legend, wrapping
    let (mut lx, mut ly) = (PAD, 52.0);
    for item in legend {
        let iw = 18.0 + cv.text_width(&item.name, 13.0, false) + 6.0 + cv.text_width(&item.value, 13.0, true) + 20.0;
        if lx + iw > w - PAD {
            lx = PAD;
            ly += 20.0;
        }
        cv.rect(lx, ly + 7.0, 12.0, 3.0, color(item.color, 1.0));
        cv.text(&item.name, lx + 18.0, ly, 13.0, false, Align::Left, color(t.text_muted, 1.0));
        let nw = cv.text_width(&item.name, 13.0, false);
        cv.text(&item.value, lx + 18.0 + nw + 6.0, ly, 13.0, true, Align::Left, color(t.text, 1.0));
        lx += iw;
    }

    let (left, right) = (PAD + 70.0, w - PAD);
    let (top, bottom) = (ly + 34.0, h - PAD - 22.0);
    let (pw, ph) = (right - left, bottom - top);
    for i in 0..=4 {
        let y = top + ph * i as f32 / 4.0;
        cv.line((left, y), (right, y), 1.0, color(t.grid, 1.0));
        let v = y_max * (4 - i) as f32 / 4.0;
        cv.text(&fmt(v), left - 10.0, y - 8.0, 12.0, false, Align::Right, color(t.text_muted, 1.0));
    }
    let wl = window_len as f32;
    cv.text(&format!("-{window_len}s"), left, bottom + 8.0, 12.0, false, Align::Left, color(t.text_muted, 1.0));
    cv.text(&format!("-{}s", window_len / 2), left + pw / 2.0, bottom + 8.0, 12.0, false, Align::Center, color(t.text_muted, 1.0));
    cv.text("now", right, bottom + 8.0, 12.0, false, Align::Right, color(t.text_muted, 1.0));

    // clip series to the plot area
    let mut mask = Mask::new(cv.pm.width(), cv.pm.height()).unwrap();
    let mut clip = PathBuilder::new();
    if let Some(r) = tiny_skia::Rect::from_ltrb(left * SCALE, (top - 2.0) * SCALE, right * SCALE, (bottom + 2.0) * SCALE) {
        clip.push_rect(r);
    }
    if let Some(p) = clip.finish() {
        mask.fill_path(&p, FillRule::Winding, true, Transform::identity());
    }

    for s in series {
        let n = s.data.len();
        if n < 2 {
            continue;
        }
        let pts: Vec<(f32, f32)> = s
            .data
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let x = right - (n - 1 - i) as f32 / wl * pw;
                let y = bottom - (v / y_max).clamp(0.0, 1.0) * ph;
                (x * SCALE, y * SCALE)
            })
            .collect();
        let curve = |close: Option<f32>| {
            let mut pb = PathBuilder::new();
            pb.move_to(pts[0].0, pts[0].1);
            for i in 1..pts.len() - 1 {
                let mid = ((pts[i].0 + pts[i + 1].0) / 2.0, (pts[i].1 + pts[i + 1].1) / 2.0);
                pb.quad_to(pts[i].0, pts[i].1, mid.0, mid.1);
            }
            let last = pts[pts.len() - 1];
            pb.line_to(last.0, last.1);
            if let Some(base) = close {
                pb.line_to(last.0, base);
                pb.line_to(pts[0].0, base);
                pb.close();
            }
            pb.finish()
        };
        if s.fill {
            if let Some(p) = curve(Some(bottom * SCALE)) {
                cv.pm.fill_path(&p, &paint(color(s.color, 0.14)), FillRule::Winding, Transform::identity(), Some(&mask));
            }
        }
        if let Some(p) = curve(None) {
            let st = Stroke { width: 2.0 * SCALE, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() };
            cv.pm.stroke_path(&p, &paint(color(s.color, 1.0)), &st, Transform::identity(), Some(&mask));
        }
    }
    cv.pm
}

fn render_donut(title: &str, subtitle: &Option<String>, big: &str, small: &str, rows: &[DonutRow], t: &Theme) -> Pixmap {
    let (w, h) = (820.0, 400.0);
    let mut cv = Canvas::new(w, h, load_fonts());
    frame(&mut cv, w, h, t);
    cv.text(title, PAD, 18.0, 18.0, true, Align::Left, color(t.text, 1.0));
    if let Some(s) = subtitle {
        cv.text(s, w - PAD, 22.0, 13.0, false, Align::Right, color(t.text_muted, 1.0));
    }

    let (r_out, thick) = (130.0, 30.0);
    let r_in = r_out - thick;
    let (cx, cy) = (PAD + 20.0 + r_out, 62.0 + (h - 62.0 - PAD) / 2.0);
    let ring = |cv: &mut Canvas, a0: f32, a1: f32, c: tiny_skia::Color| {
        let steps = (((a1 - a0).abs() / TAU) * 180.0).ceil().max(2.0) as usize;
        let mut pts = Vec::new();
        for i in 0..=steps {
            let a = a0 + (a1 - a0) * i as f32 / steps as f32;
            pts.push((cx + r_out * a.cos(), cy + r_out * a.sin()));
        }
        for i in (0..=steps).rev() {
            let a = a0 + (a1 - a0) * i as f32 / steps as f32;
            pts.push((cx + r_in * a.cos(), cy + r_in * a.sin()));
        }
        cv.polygon(&pts, c);
    };
    let start = -TAU / 4.0;
    let total: f32 = rows.iter().map(|r| r.amount.max(0.0)).sum();
    if total <= 0.0 {
        ring(&mut cv, start, start + TAU * 0.9999, color(t.surface_alt, 1.0));
    } else {
        let visible = rows.iter().filter(|r| r.amount > 0.0).count();
        let gap = if visible > 1 { 0.025 } else { 0.0 };
        let mut a = start;
        for r in rows.iter().filter(|r| r.amount > 0.0) {
            let sweep = r.amount / total * TAU;
            let a0 = a + gap / 2.0;
            let a1 = (a + sweep - gap / 2.0).max(a0 + 0.002).min(a0 + TAU * 0.9999);
            ring(&mut cv, a0, a1, color(r.color, 1.0));
            a += sweep;
        }
    }
    cv.text(big, cx, cy - 26.0, 40.0, true, Align::Center, color(t.text, 1.0));
    cv.text(small, cx, cy + 18.0, 14.0, false, Align::Center, color(t.text_muted, 1.0));

    // legend
    let lx = cx + r_out + 50.0;
    let row_h = 34.0;
    let mut y = cy - row_h * rows.len() as f32 / 2.0 + 8.0;
    for r in rows {
        cv.rect(lx, y + 4.0, 12.0, 12.0, color(r.color, 1.0));
        cv.text(&r.name, lx + 22.0, y, 16.0, false, Align::Left, color(t.text, 1.0));
        cv.text(&r.value, w - PAD - 70.0, y, 16.0, false, Align::Right, color(t.text_muted, 1.0));
        cv.text(&format!("{:.0}%", r.pct), w - PAD, y, 16.0, true, Align::Right, color(t.text, 1.0));
        y += row_h;
    }
    cv.pm
}

/// Render `spec` and write it to `path` as PNG.
pub fn save_png(spec: &ChartSpec, theme: &Theme, path: &Path) -> Result<(), String> {
    let pm = match spec {
        ChartSpec::Line { title, legend, series, y_max, fmt, window_len } => {
            render_line(title, legend, series, *y_max, *fmt, *window_len, theme)
        }
        ChartSpec::Donut { title, subtitle, center_big, center_small, rows } => {
            render_donut(title, subtitle, center_big, center_small, rows, theme)
        }
    };
    pm.save_png(path).map_err(|e| e.to_string())
}

pub fn default_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    home.map(|h| h.join("Pictures"))
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

pub fn suggested_name(title: &str) -> String {
    let slug = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("android-monitor-{slug}-{ts}.png")
}
