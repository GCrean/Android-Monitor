use gpui::{rgb, Hsla};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub mode: Mode,
    pub bg: Hsla,
    pub surface: Hsla,
    pub surface_alt: Hsla,
    pub border: Hsla,
    pub grid: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    pub good: Hsla,
    pub warn: Hsla,
    pub bad: Hsla,
    /// Categorical chart colours (colour-blind friendly, tuned per mode).
    pub series: [Hsla; 5],
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            mode: Mode::Dark,
            bg: c(0x0d1117),
            surface: c(0x161b22),
            surface_alt: c(0x1f2630),
            border: c(0x2d333b),
            grid: c(0x262c36),
            text: c(0xe6edf3),
            text_muted: c(0x8b949e),
            accent: c(0x58a6ff),
            good: c(0x3fb950),
            warn: c(0xd29922),
            bad: c(0xf85149),
            series: [c(0x58a6ff), c(0xf78166), c(0x3fb950), c(0xd2a8ff), c(0xe3b341)],
        }
    }

    pub fn light() -> Self {
        Self {
            mode: Mode::Light,
            bg: c(0xf3f5f8),
            surface: c(0xffffff),
            surface_alt: c(0xeef1f5),
            border: c(0xd5dae1),
            grid: c(0xe6e9ee),
            text: c(0x1b1f24),
            text_muted: c(0x5b6572),
            accent: c(0x0969da),
            good: c(0x1a7f37),
            warn: c(0x9a6700),
            bad: c(0xcf222e),
            series: [c(0x0969da), c(0xd1471f), c(0x1a7f37), c(0x8250df), c(0xb08800)],
        }
    }

    /// Distinct colour for core `i` (golden-angle hue steps stay separable for any core count).
    pub fn core_color(&self, i: usize) -> Hsla {
        let hue = (i as f32 * 137.508 + 215.0) % 360.0 / 360.0;
        match self.mode {
            Mode::Dark => gpui::hsla(hue, 0.78, 0.62, 1.0),
            Mode::Light => gpui::hsla(hue, 0.72, 0.40, 1.0),
        }
    }

    pub fn toggled(&self) -> Self {
        match self.mode {
            Mode::Dark => Self::light(),
            Mode::Light => Self::dark(),
        }
    }
}
