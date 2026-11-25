//! Custom-drawn charts (donut, scrolling line chart, theme icon) on GPUI canvas + paths.

use std::f32::consts::{PI, TAU};
use std::time::Instant;

use gpui::{
    canvas, div, point, prelude::*, px, Bounds, Hsla, IntoElement, PathBuilder, Pixels, Point,
    Window,
};

use crate::theme::{Mode, Theme};

fn pt(b: &Bounds<Pixels>, x: f32, y: f32) -> Point<Pixels> {
    point(b.origin.x + px(x), b.origin.y + px(y))
}

fn fill_polygon(window: &mut Window, pts: &[Point<Pixels>], color: Hsla) {
    let mut pb = PathBuilder::fill();
    pb.add_polygon(pts, true);
    if let Ok(p) = pb.build() {
        window.paint_path(p, color);
    }
}

fn stroke_line(window: &mut Window, a: Point<Pixels>, b: Point<Pixels>, w: f32, color: Hsla) {
    let mut pb = PathBuilder::stroke(px(w));
    pb.move_to(a);
    pb.line_to(b);
    if let Ok(p) = pb.build() {
        window.paint_path(p, color);
    }
}

/// 1-2-5 "nice" upper bound for an axis.
pub fn nice_max(v: f32, floor: f32) -> f32 {
    let v = v.max(floor);
    let exp = v.log10().floor();
    let base = 10f32.powf(exp);
    let m = v / base;
    let n = if m <= 1.0 {
        1.0
    } else if m <= 2.0 {
        2.0
    } else if m <= 5.0 {
        5.0
    } else {
        10.0
    };
    n * base
}

// ------------------------------------------------------------------ donut --

pub struct Slice {
    pub value: f32,
    pub color: Hsla,
}

/// Donut chart drawn as filled ring segments. Square element of `size` px.
pub fn donut(slices: Vec<Slice>, size: f32, thickness: f32, track: Hsla) -> impl IntoElement {
    div().size(px(size)).flex_none().child(
        canvas(
            |_, _, _| (),
            move |b, _, window, _| {
                let cx = size / 2.0;
                let r_out = size / 2.0 - 1.0;
                let r_in = r_out - thickness;
                let ring = |a0: f32, a1: f32| -> Vec<Point<Pixels>> {
                    let steps = (((a1 - a0).abs() / TAU) * 120.0).ceil().max(2.0) as usize;
                    let mut v = Vec::with_capacity(steps * 2 + 2);
                    for i in 0..=steps {
                        let a = a0 + (a1 - a0) * i as f32 / steps as f32;
                        v.push(pt(&b, cx + r_out * a.cos(), cx + r_out * a.sin()));
                    }
                    for i in (0..=steps).rev() {
                        let a = a0 + (a1 - a0) * i as f32 / steps as f32;
                        v.push(pt(&b, cx + r_in * a.cos(), cx + r_in * a.sin()));
                    }
                    v
                };

                let total: f32 = slices.iter().map(|s| s.value.max(0.0)).sum();
                let start = -PI / 2.0;
                if total <= 0.0 {
                    fill_polygon(window, &ring(start, start + TAU * 0.9999), track);
                    return;
                }
                let visible = slices.iter().filter(|s| s.value > 0.0).count();
                let gap = if visible > 1 { 0.025 } else { 0.0 };
                let mut a = start;
                for s in slices.iter().filter(|s| s.value > 0.0) {
                    let sweep = s.value / total * TAU;
                    let (a0, a1) = (a + gap / 2.0, (a + sweep - gap / 2.0).max(a + gap / 2.0 + 0.002));
                    let a1 = a1.min(a0 + TAU * 0.9999);
                    fill_polygon(window, &ring(a0, a1), s.color);
                    a += sweep;
                }
            },
        )
        .size_full(),
    )
}

// ------------------------------------------------------------- line chart --

#[derive(Clone)]
pub struct LineSeries {
    pub data: Vec<f32>,
    pub color: Hsla,
    pub fill: bool,
}

#[derive(Clone)]
pub struct LegendItem {
    pub name: String,
    pub color: Hsla,
    pub value: String,
}

#[derive(Clone)]
pub struct DonutRow {
    pub name: String,
    pub value: String,
    pub pct: f32,
    pub amount: f32,
    pub color: Hsla,
}

/// Snapshot of everything needed to redraw a card's chart off-screen (PNG export).
#[derive(Clone)]
pub enum ChartSpec {
    Line {
        title: String,
        legend: Vec<LegendItem>,
        series: Vec<LineSeries>,
        y_max: f32,
        fmt: fn(f32) -> String,
        window_len: usize,
    },
    Donut {
        title: String,
        subtitle: Option<String>,
        center_big: String,
        center_small: String,
        rows: Vec<DonutRow>,
    },
}

impl ChartSpec {
    pub fn title(&self) -> &str {
        match self {
            ChartSpec::Line { title, .. } | ChartSpec::Donut { title, .. } => title,
        }
    }
}

pub struct LineChart {
    pub series: Vec<LineSeries>,
    pub y_max: f32,
    pub window_len: usize,
    pub last_arrival: Instant,
    pub animate: bool,
    pub height: f32,
    pub fmt: fn(f32) -> String,
    pub theme: Theme,
}

fn smooth_path(pts: &[(f32, f32)], b: &Bounds<Pixels>, mut pb: PathBuilder, close_to: Option<f32>) -> Option<gpui::Path<Pixels>> {
    let p = |i: usize| pt(b, pts[i].0, pts[i].1);
    pb.move_to(p(0));
    for i in 1..pts.len() - 1 {
        let mid = pt(b, (pts[i].0 + pts[i + 1].0) / 2.0, (pts[i].1 + pts[i + 1].1) / 2.0);
        pb.curve_to(mid, p(i));
    }
    pb.line_to(p(pts.len() - 1));
    if let Some(base) = close_to {
        pb.line_to(pt(b, pts[pts.len() - 1].0, base));
        pb.line_to(pt(b, pts[0].0, base));
        pb.close();
    }
    pb.build().ok()
}

impl LineChart {
    pub fn render(self) -> impl IntoElement {
        let t = self.theme;
        let h = self.height;
        let ticks = 4;
        let labels: Vec<String> = (0..=ticks)
            .rev()
            .map(|i| (self.fmt)(self.y_max * i as f32 / ticks as f32))
            .collect();
        let secs = self.window_len;

        let LineChart { series, y_max, window_len, last_arrival, animate, .. } = self;
        let grid = t.grid;

        let plot = canvas(
            |_, _, _| (),
            move |b, _, window, _| {
                let w = f32::from(b.size.width);
                let hh = f32::from(b.size.height);
                for i in 0..=ticks {
                    let y = (hh - 1.0) * i as f32 / ticks as f32;
                    stroke_line(window, pt(&b, 0.0, y), pt(&b, w, y), 1.0, grid);
                }
                let wl = window_len as f32;
                // Chart lags the newest sample by one second so the head can be
                // interpolated -> continuous scrolling instead of 1 Hz steps.
                let phase = if animate { last_arrival.elapsed().as_secs_f32().min(1.0) } else { 1.0 };
                let yv = |v: f32| (hh - 2.0) - (v / y_max).clamp(0.0, 1.0) * (hh - 4.0) + 1.0;

                for s in &series {
                    let n = s.data.len();
                    if n < 2 {
                        continue;
                    }
                    let head = (n - 2) as f32 + phase;
                    let x_of = |i: f32| (i - (head - wl)) / wl * w;
                    let mut pts: Vec<(f32, f32)> = Vec::with_capacity(n);
                    for (i, v) in s.data[..n - 1].iter().enumerate() {
                        let x = x_of(i as f32);
                        if x >= -w / wl * 2.0 {
                            pts.push((x, yv(*v)));
                        }
                    }
                    let vh = s.data[n - 2] + (s.data[n - 1] - s.data[n - 2]) * phase;
                    pts.push((w, yv(vh)));
                    if pts.len() < 2 {
                        continue;
                    }
                    if s.fill {
                        if let Some(p) = smooth_path(&pts, &b, PathBuilder::fill(), Some(hh)) {
                            window.paint_path(p, s.color.opacity(0.14));
                        }
                    }
                    if let Some(p) = smooth_path(&pts, &b, PathBuilder::stroke(px(2.0)), None) {
                        window.paint_path(p, s.color);
                    }
                }
                if animate {
                    window.request_animation_frame();
                }
            },
        )
        .size_full();

        let label = |s: String| div().text_xs().text_color(t.text_muted).child(s);

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        div()
                            .w(px(64.0))
                            .h(px(h))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .justify_between()
                            .items_end()
                            .children(labels.into_iter().map(label)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h(px(h))
                            .overflow_hidden()
                            .child(plot),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .pl(px(72.0))
                    .child(label(format!("-{secs}s")))
                    .child(label(format!("-{}s", secs / 2)))
                    .child(label("now".into())),
            )
    }
}

// ------------------------------------------------------------ theme icon --

/// Sun (shown in dark mode -> click for light) or crescent moon (shown in light mode).
pub fn theme_icon(mode: Mode, color: Hsla, bg: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |b, _, window, _| {
            let w = f32::from(b.size.width);
            let h = f32::from(b.size.height);
            let (cx, cy) = (w / 2.0, h / 2.0);
            let circle = |b: &Bounds<Pixels>, x: f32, y: f32, r: f32| -> Vec<Point<Pixels>> {
                (0..32)
                    .map(|i| {
                        let a = TAU * i as f32 / 32.0;
                        pt(b, x + r * a.cos(), y + r * a.sin())
                    })
                    .collect()
            };
            match mode {
                Mode::Dark => {
                    fill_polygon(window, &circle(&b, cx, cy, 4.6), color);
                    for i in 0..8 {
                        let a = TAU * i as f32 / 8.0;
                        stroke_line(
                            window,
                            pt(&b, cx + 7.0 * a.cos(), cy + 7.0 * a.sin()),
                            pt(&b, cx + 9.6 * a.cos(), cy + 9.6 * a.sin()),
                            1.8,
                            color,
                        );
                    }
                }
                Mode::Light => {
                    fill_polygon(window, &circle(&b, cx, cy, 8.0), color);
                    fill_polygon(window, &circle(&b, cx + 4.2, cy - 3.4, 6.8), bg);
                }
            }
        },
    )
    .size_full()
}
