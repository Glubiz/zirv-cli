//! The orchestrator dashboard's colours and its cell grid. The palette is the approved
//! prototype's `C`; a frame is drawn into a [`Grid`] (so the layout, the hit regions and the
//! motion are one pure function of the data and the clock) and then copied into the terminal
//! buffer, as truecolor or as the nearest xterm-256 index. The DIM modifier is never used here:
//! `dim` and `faint` are colours.

use std::sync::OnceLock;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rgb(pub(super) u8, pub(super) u8, pub(super) u8);

const fn hex(c: u32) -> Rgb {
    Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// The prototype's `C` tokens.
pub(super) mod c {
    use super::{Rgb, hex};

    pub(in super::super) const BG: Rgb = hex(0x101216);
    pub(in super::super) const PANEL: Rgb = hex(0x15181E);
    pub(in super::super) const RAISE: Rgb = hex(0x1D212A);
    pub(in super::super) const SEL: Rgb = hex(0x22304A);
    pub(in super::super) const RULE: Rgb = hex(0x2C323D);
    pub(in super::super) const FAINT: Rgb = hex(0x525A69);
    pub(in super::super) const DIM: Rgb = hex(0x8E96A4);
    pub(in super::super) const FG: Rgb = hex(0xD5D9E0);
    pub(in super::super) const HI: Rgb = hex(0xF3F5F8);
    pub(in super::super) const SEAT: Rgb = hex(0x62D2D8);
    pub(in super::super) const AGENT: Rgb = hex(0x86ABF5);
    pub(in super::super) const AGENT_DIM: Rgb = hex(0x34466B);
    pub(in super::super) const JEV: Rgb = hex(0x7DD3B2);
    pub(in super::super) const YOU: Rgb = hex(0xEBC46E);
    pub(in super::super) const OK: Rgb = hex(0x73D291);
    pub(in super::super) const WARN: Rgb = hex(0xEBC46E);
    pub(in super::super) const WARN_DIM: Rgb = hex(0x5E5030);
    pub(in super::super) const ERR: Rgb = hex(0xF07A72);
    pub(in super::super) const CHIP: Rgb = hex(0x252A34);
    pub(in super::super) const CHIP_HI: Rgb = hex(0x323947);
    pub(in super::super) const WARN_BG: Rgb = hex(0x211E16);
    pub(in super::super) const OK_BG: Rgb = hex(0x16241B);
    /// The supervisor's hue in the activity list (the prototype has none).
    pub(in super::super) const ARCH: Rgb = hex(0xB79CF0);
}

pub(super) const BRAILLE: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The braille spinner frame at `now` ms.
pub(super) fn spin(now: u64) -> char {
    BRAILLE[(now / 80 % 10) as usize]
}

/// `a` towards `b`, `t` clamped to 0..=1.
pub(super) fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as u8;
    Rgb(ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

/// 0.5 + 0.5 sin: a slow breath with the given period in seconds.
pub(super) fn breathe(now: u64, period: f64) -> f64 {
    0.5 + 0.5 * (now as f64 / 1000.0 * std::f64::consts::TAU / period).sin()
}

/// The nearest xterm-256 colour in the 6x6x6 cube or the grey ramp (the 16 system colours are
/// the terminal theme's, so they are never picked).
pub(super) fn to_indexed(rgb: Rgb) -> u8 {
    let level = |v: u8| -> usize {
        const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        (0..6)
            .min_by_key(|&i| i32::from(STEPS[i]).abs_diff(i32::from(v)))
            .unwrap_or(0)
    };
    const STEPS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let (r, g, b) = (level(rgb.0), level(rgb.1), level(rgb.2));
    let cube = (STEPS[r], STEPS[g], STEPS[b]);
    let cube_index = 16 + 36 * r + 6 * g + b;
    let grey_level =
        ((i32::from(rgb.0) + i32::from(rgb.1) + i32::from(rgb.2)) / 3 - 8).clamp(0, 230);
    let grey_step = ((grey_level + 5) / 10).clamp(0, 23);
    let grey = 8 + 10 * grey_step;
    let dist = |p: (i32, i32, i32)| {
        let d = |a: i32, b: u8| (a - i32::from(b)).pow(2);
        d(p.0, rgb.0) + d(p.1, rgb.1) + d(p.2, rgb.2)
    };
    if dist((grey, grey, grey)) < dist(cube) {
        (232 + grey_step) as u8
    } else {
        cube_index as u8
    }
}

pub(super) fn color(rgb: Rgb, truecolor: bool) -> Color {
    if truecolor {
        Color::Rgb(rgb.0, rgb.1, rgb.2)
    } else {
        Color::Indexed(to_indexed(rgb))
    }
}

/// Whether `COLORTERM` promises 24-bit colour.
pub(super) fn truecolor_from(colorterm: Option<&str>) -> bool {
    colorterm.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "truecolor" | "24bit"))
}

/// Decided once per process from the environment.
pub(super) fn truecolor() -> bool {
    static CHOICE: OnceLock<bool> = OnceLock::new();
    *CHOICE.get_or_init(|| truecolor_from(std::env::var("COLORTERM").ok().as_deref()))
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Cell {
    pub(super) ch: char,
    pub(super) fg: Rgb,
    pub(super) bg: Rgb,
    pub(super) bold: bool,
    /// Painted this frame; only these cells reach the terminal.
    pub(super) set: bool,
    /// The second column of a double-width character.
    pub(super) skip: bool,
}

const BLANK: Cell = Cell {
    ch: ' ',
    fg: c::FG,
    bg: c::BG,
    bold: false,
    set: false,
    skip: false,
};

/// A frame under construction. Coordinates are signed so a card scrolled half off the edge simply
/// clips.
pub(super) struct Grid {
    pub(super) w: i32,
    pub(super) h: i32,
    cells: Vec<Cell>,
}

impl Grid {
    pub(super) fn new(w: u16, h: u16) -> Self {
        Self {
            w: i32::from(w),
            h: i32::from(h),
            cells: vec![BLANK; usize::from(w) * usize::from(h)],
        }
    }

    /// Every cell painted with the page background.
    pub(super) fn clear_page(&mut self) {
        self.cells.fill(Cell { set: true, ..BLANK });
    }

    fn idx(&self, x: i32, y: i32) -> Option<usize> {
        (x >= 0 && y >= 0 && x < self.w && y < self.h).then(|| (y * self.w + x) as usize)
    }

    pub(super) fn put(
        &mut self,
        x: i32,
        y: i32,
        ch: char,
        fg: Option<Rgb>,
        bg: Option<Rgb>,
        bold: bool,
    ) {
        let Some(i) = self.idx(x, y) else {
            return;
        };
        let cell = &mut self.cells[i];
        cell.ch = ch;
        cell.set = true;
        cell.skip = false;
        cell.bold = bold;
        if let Some(fg) = fg {
            cell.fg = fg;
        }
        if let Some(bg) = bg {
            cell.bg = bg;
        }
    }

    pub(super) fn set_fg(&mut self, x: i32, y: i32, fg: Rgb) {
        if let Some(i) = self.idx(x, y) {
            self.cells[i].fg = fg;
        }
    }

    /// `s` from `x`; returns the column after it.
    pub(super) fn text_on(
        &mut self,
        x: i32,
        y: i32,
        s: &str,
        fg: Rgb,
        bg: Option<Rgb>,
        bold: bool,
    ) -> i32 {
        let mut px = x;
        for ch in s.chars() {
            let w = ch.width().unwrap_or(0);
            if w == 0 {
                continue;
            }
            self.put(px, y, ch, Some(fg), bg, bold);
            if w == 2 {
                self.put(px + 1, y, ' ', None, bg, bold);
                if let Some(i) = self.idx(px + 1, y) {
                    self.cells[i].skip = true;
                }
            }
            px += w as i32;
        }
        px
    }

    pub(super) fn text(&mut self, x: i32, y: i32, s: &str, fg: Rgb) -> i32 {
        self.text_on(x, y, s, fg, None, false)
    }

    pub(super) fn bold(&mut self, x: i32, y: i32, s: &str, fg: Rgb) -> i32 {
        self.text_on(x, y, s, fg, None, true)
    }

    pub(super) fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, bg: Rgb) {
        for j in y..y + h {
            for i in x..x + w {
                self.put(i, j, ' ', None, Some(bg), false);
            }
        }
    }

    /// A rounded box; `bg` fills the inside first.
    pub(super) fn boxed(&mut self, x: i32, y: i32, w: i32, h: i32, col: Rgb, bg: Option<Rgb>) {
        if w < 2 || h < 2 {
            return;
        }
        if let Some(bg) = bg {
            self.fill(x, y, w, h, bg);
        }
        let f = Some(col);
        self.put(x, y, '╭', f, None, false);
        self.put(x + w - 1, y, '╮', f, None, false);
        self.put(x, y + h - 1, '╰', f, None, false);
        self.put(x + w - 1, y + h - 1, '╯', f, None, false);
        for i in x + 1..x + w - 1 {
            self.put(i, y, '─', f, None, false);
            self.put(i, y + h - 1, '─', f, None, false);
        }
        for j in y + 1..y + h - 1 {
            self.put(x, j, '│', f, None, false);
            self.put(x + w - 1, j, '│', f, None, false);
        }
    }

    /// Parts `(text, colour, bold)` centred in `w` columns from `x`.
    pub(super) fn center(&mut self, x: i32, w: i32, y: i32, parts: &[(&str, Rgb, bool)]) {
        let len: i32 = parts.iter().map(|p| width(p.0)).sum();
        let mut px = x + (w - len) / 2;
        for (s, fg, bold) in parts {
            px = self.text_on(px, y, s, *fg, None, *bold);
        }
    }

    /// Copy the painted cells into `buf`, `origin` being where the grid's corner sits.
    pub(super) fn blit(&self, buf: &mut Buffer, origin: (u16, u16), truecolor: bool) {
        for y in 0..self.h {
            for x in 0..self.w {
                let cell = self.cells[(y * self.w + x) as usize];
                if !cell.set || cell.skip {
                    continue;
                }
                let (bx, by) = (origin.0 + x as u16, origin.1 + y as u16);
                let Some(out) = buf.cell_mut((bx, by)) else {
                    continue;
                };
                let mut style = Style::default()
                    .fg(color(cell.fg, truecolor))
                    .bg(color(cell.bg, truecolor));
                if cell.bold {
                    style = style.add_modifier(Modifier::BOLD);
                }
                out.reset();
                out.set_symbol(cell.ch.encode_utf8(&mut [0; 4]));
                out.set_style(style);
            }
        }
    }
}

pub(super) fn width(s: &str) -> i32 {
    s.chars().map(|c| c.width().unwrap_or(0) as i32).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_palette_falls_back_to_the_nearest_xterm_256_colour_only_without_truecolor() {
        assert!(truecolor_from(Some("truecolor")));
        assert!(truecolor_from(Some("24bit")));
        assert!(!truecolor_from(Some("256color")));
        assert!(!truecolor_from(None));
        assert_eq!(color(c::SEAT, true), Color::Rgb(0x62, 0xD2, 0xD8));
        // The cube's own corners and the grey ramp's ends map to themselves.
        assert_eq!(to_indexed(Rgb(0, 0, 0)), 16);
        assert_eq!(to_indexed(Rgb(255, 255, 255)), 231);
        assert_eq!(to_indexed(Rgb(255, 0, 0)), 196);
        assert_eq!(to_indexed(Rgb(8, 8, 8)), 232);
        assert_eq!(to_indexed(Rgb(238, 238, 238)), 255);
        // Near-black panels land on the grey ramp, bright accents in the cube.
        assert_eq!(color(c::BG, false), Color::Indexed(233));
        assert_eq!(color(c::PANEL, false), Color::Indexed(234));
        assert_eq!(color(c::SEAT, false), Color::Indexed(80));
        assert_eq!(color(c::AGENT, false), Color::Indexed(111));
        assert_eq!(color(c::ERR, false), Color::Indexed(209));
    }

    #[test]
    fn mix_blends_and_clamps_and_a_breath_stays_between_zero_and_one() {
        assert_eq!(mix(c::BG, c::HI, 0.0), c::BG);
        assert_eq!(mix(c::BG, c::HI, 1.0), c::HI);
        assert_eq!(mix(c::BG, c::HI, 9.0), c::HI);
        assert_eq!(mix(Rgb(0, 0, 0), Rgb(100, 200, 50), 0.5), Rgb(50, 100, 25));
        for now in (0..4000).step_by(130) {
            let b = breathe(now, 1.6);
            assert!((0.0..=1.0).contains(&b), "{now}: {b}");
        }
    }

    #[test]
    fn only_painted_cells_reach_the_buffer_and_a_wide_character_takes_two() {
        let mut grid = Grid::new(6, 2);
        grid.text_on(0, 0, "a世", c::HI, Some(c::SEL), true);
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 6, 2));
        buf[(5, 1)].set_symbol("x");
        grid.blit(&mut buf, (0, 0), true);
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(buf[(1, 0)].symbol(), "世");
        assert_eq!(buf[(5, 1)].symbol(), "x", "an unpainted cell is left alone");
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(0x22, 0x30, 0x4A));
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
        assert!(!buf[(0, 0)].modifier.contains(Modifier::DIM));
    }
}
