//! One frame of the orchestrator dashboard: the cell grid, the regions the mouse acts on and
//! what the keys need to know about what was drawn. Built by [`super::orch`] and [`super::flow`]
//! as a pure function of the data, the facts, the view state and the clock, then painted and
//! hit-tested from the same value, so a click can never land on something the frame did not show.

use crossterm::event::KeyCode;

use super::model::{Model, Sel};
use super::theme::{Grid, Rgb, c, width};
use super::{TreeView, content};

/// What a region does when it is clicked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Act {
    /// Open this node's harness (its pane chat, or its host's).
    Open(Sel),
    Select(Sel),
    /// A key, as if it were typed.
    Press(KeyCode),
    /// `^A t` from the flow: back to the classic dashboard.
    Classic,
    /// `^A t` from an open chat: back to the flow.
    BackToFlow,
    Help,
}

#[derive(Debug, Clone)]
pub(super) struct Reg {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) w: i32,
    pub(super) h: i32,
    pub(super) act: Option<Act>,
    /// The node a hover over this region previews.
    pub(super) node: Option<Sel>,
    /// The chip a hover over this region lights.
    pub(super) hk: Option<String>,
}

/// A request drawn with its answer keys: only this one can be answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShownApproval {
    pub(super) short: String,
    pub(super) conn: u64,
    /// Shown in full, so `y` and `d` are offered.
    pub(super) answerable: bool,
    /// The request itself offers "always allow".
    pub(super) always: bool,
}

/// A supervisor ruling drawn with its override key: only this one can be overridden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShownRuling {
    pub(super) id: String,
    /// The short id of the session it was made for.
    pub(super) short: String,
}

/// Where the flow put its cards, for scrolling and for tests.
#[derive(Debug, Clone, Default)]
pub(super) struct Metrics {
    pub(super) first_row: usize,
    pub(super) vis_rows: usize,
    pub(super) total_rows: usize,
    pub(super) per_row: usize,
    /// Node ids in card order: the order the arrows walk.
    pub(super) order: Vec<String>,
    /// The folded FINISHED pills, newest first.
    pub(super) folded: Vec<String>,
    /// The cells a pulse walks from the seat to each card of the first visible row (and to Jev),
    /// as `(node id, path)`.
    pub(super) lanes: Vec<(String, Vec<(i32, i32)>)>,
}

pub(super) struct Scene {
    pub(super) grid: Grid,
    pub(super) regs: Vec<Reg>,
    pub(super) shown_approvals: Vec<ShownApproval>,
    pub(super) shown_rulings: Vec<ShownRuling>,
    pub(super) metrics: Metrics,
    /// The activity rows' rectangle `(x, y, w, h)`, where the wheel scrolls them.
    pub(super) activity: (i32, i32, i32, i32),
    /// The flow's rectangle, where the wheel scrolls the agent rows.
    pub(super) flow: (i32, i32, i32, i32),
}

impl Scene {
    pub(super) fn new(w: u16, h: u16) -> Self {
        Self {
            grid: Grid::new(w, h),
            regs: Vec::new(),
            shown_approvals: Vec::new(),
            shown_rulings: Vec::new(),
            metrics: Metrics::default(),
            activity: (0, 0, 0, 0),
            flow: (0, 0, 0, 0),
        }
    }

    pub(super) fn reg(&mut self, x: i32, y: i32, w: i32, h: i32, act: Option<Act>) -> &mut Reg {
        self.regs.push(Reg {
            x,
            y,
            w,
            h,
            act,
            node: None,
            hk: None,
        });
        self.regs.last_mut().expect("just pushed")
    }

    /// The top-most region under a cell.
    pub(super) fn hit(&self, cx: i32, cy: i32) -> Option<&Reg> {
        self.regs
            .iter()
            .rev()
            .find(|r| cx >= r.x && cx < r.x + r.w && cy >= r.y && cy < r.y + r.h)
    }

    /// A key chip at `(x, y)`: bold key, then label, on a chip background that lights under the
    /// pointer. A disabled chip is drawn faint and registers nothing, so a click does nothing.
    pub(super) fn chip(&mut self, ctx: &Ctx, x: i32, y: i32, spec: Chip) -> i32 {
        let id = spec
            .id
            .clone()
            .unwrap_or_else(|| format!("{}{}", spec.key, spec.label));
        let hov = ctx.hover_key == Some(id.as_str());
        let dis = spec.act.is_none() && !spec.hint;
        let bg = if dis {
            c::PANEL
        } else if hov {
            c::CHIP_HI
        } else {
            spec.bg.unwrap_or(c::CHIP)
        };
        let key_fg = if dis {
            c::FAINT
        } else {
            spec.kc.unwrap_or(c::HI)
        };
        let label_fg = if dis { c::FAINT } else { c::FG };
        let after = self
            .grid
            .text_on(x, y, &format!(" {} ", spec.key), key_fg, Some(bg), true);
        let end = self.grid.text_on(
            after,
            y,
            &format!("{} ", spec.label),
            label_fg,
            Some(bg),
            false,
        );
        if let Some(act) = spec.act {
            let r = self.reg(x, y, end - x, 1, Some(act));
            r.hk = Some(id);
            r.node = spec.node;
        }
        end
    }
}

/// A chip before it has a place. No action means it does not apply right now.
pub(super) struct Chip {
    key: &'static str,
    label: &'static str,
    act: Option<Act>,
    bg: Option<Rgb>,
    kc: Option<Rgb>,
    id: Option<String>,
    /// A key named only to be read: drawn like any chip, nothing to click.
    hint: bool,
    /// The node the chip acts on: pressing it selects this first, and hovering it previews it.
    node: Option<Sel>,
}

impl Chip {
    pub(super) fn new(key: &'static str, label: &'static str, act: Option<Act>) -> Self {
        Self {
            key,
            label,
            act,
            bg: None,
            kc: None,
            id: None,
            hint: false,
            node: None,
        }
    }

    pub(super) fn node(mut self, node: Option<Sel>) -> Self {
        self.node = node;
        self
    }

    pub(super) fn hint(mut self) -> Self {
        self.hint = true;
        self
    }

    pub(super) fn bg(mut self, bg: Rgb) -> Self {
        self.bg = Some(bg);
        self
    }

    pub(super) fn kc(mut self, kc: Rgb) -> Self {
        self.kc = Some(kc);
        self
    }

    pub(super) fn id(mut self, id: &str) -> Self {
        self.id = Some(id.to_string());
        self
    }

    /// The columns the chip takes.
    pub(super) fn width(&self) -> i32 {
        width(self.key) + 2 + width(self.label) + 1
    }
}

/// What a frame is drawn from.
pub(super) struct Ctx<'a> {
    pub(super) model: &'a Model<'a>,
    pub(super) view: &'a TreeView,
    /// The injected animation clock, in milliseconds.
    pub(super) now: u64,
    /// Unix seconds, for ages.
    pub(super) wall: u64,
    pub(super) sel: &'a Sel,
    /// The node under the pointer, previewed in place of the selection.
    pub(super) hover: Option<&'a Sel>,
    pub(super) hover_key: Option<&'a str>,
}

// -- Text helpers, ported from the prototype ------------------------------------------------

/// `s` cut to `w` columns with a trailing `…`.
pub(super) fn cut(s: &str, w: i32) -> String {
    let s = content::clean(s);
    if width(&s) <= w {
        return s;
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as i32;
        if used + cw > w - 1 {
            break;
        }
        out.push(ch);
        used += cw;
    }
    if !out.ends_with('…') {
        out.push('…');
    }
    out
}

/// Word-wrap to `w` columns; with `max`, a longer text is cut at the last kept line with `…`.
/// The flag says whether everything fitted.
pub(super) fn wrap(s: &str, w: usize, max: Option<usize>) -> (Vec<String>, bool) {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let w = w.max(1);
    let len = |s: &str| s.chars().count();
    for word in content::clean(s).split_whitespace() {
        if cur.is_empty() {
            cur = word.to_string();
        } else if len(&cur) + 1 + len(word) <= w {
            cur.push(' ');
            cur.push_str(word);
        } else {
            out.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
        while len(&cur) > w {
            out.push(cur.chars().take(w).collect());
            cur = cur.chars().skip(w).collect();
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    let Some(max) = max.filter(|m| out.len() > *m && *m > 0) else {
        return (out, true);
    };
    out.truncate(max);
    let mut last: String = out[max - 1].clone();
    if len(&last) >= w {
        last = last.chars().take(w - 1).collect();
    }
    let last = last.trim_end_matches(|c: char| c.is_whitespace() || ",.;:".contains(c));
    out[max - 1] = format!("{last}…");
    (out, false)
}

/// A job cut at a word to `w` columns.
pub(super) fn short_name(job: &str, w: i32) -> String {
    let job = content::clean(job);
    if width(&job) <= w {
        return job;
    }
    let mut out = String::new();
    for word in job.split(' ') {
        let next = if out.is_empty() {
            word.to_string()
        } else {
            format!("{out} {word}")
        };
        if width(&next) > w - 1 {
            break;
        }
        out = next;
    }
    if out.is_empty() {
        return cut(&job, w - 1) + "…";
    }
    out + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_at_words_splits_a_long_word_and_cuts_the_last_line_with_an_ellipsis() {
        assert_eq!(wrap("one two three", 7, None).0, ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4, None).0, ["abcd", "efgh", "ij"]);
        let (lines, whole) = wrap("alpha beta gamma delta epsilon", 11, Some(2));
        assert_eq!(lines, ["alpha beta", "gamma delt…"]);
        assert!(!whole);
        assert!(wrap("alpha beta", 11, Some(2)).1);
        assert_eq!(
            short_name("Integrate graph fields now", 20),
            "Integrate graph…"
        );
        assert_eq!(cut("abcdef", 4), "abc…");
    }
}
