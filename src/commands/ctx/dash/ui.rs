//! Pure renderers for the dashboard: every function in this module takes a
//! `&mut Frame` plus already-computed data and draws into it. No I/O, no
//! filesystem, no environment, no clock -- Task 5's event loop assembles the
//! facts (from panes, the session registry, mail/memory) and calls straight
//! through here, and every renderer is exercised with `ratatui::backend::
//! TestBackend` precisely because there is nothing else to stub.
//!
//! `SpawnDraft`/`NudgeDraft`/`MailView`/`MemoryView`/`RestoreView` carry
//! whatever shape their own overlay reducer needs (Tasks 8/9/12, in
//! `dash::mod`); `SpawnDraft` is still the Task 4 placeholder -- Spawn's own
//! reducer is out of this plan's scope.
//!
//! Issue #202 phase 2b: the dashboard's own visual language -- one cyan
//! `zirv` brand chip, semantic colour reserved for state, rounded frames, a
//! live spinner for a working pane, two-tone key hints. Everything else in
//! this module (in particular [`render_grid`], which mirrors a live child
//! terminal's own colours verbatim) stays exactly as it was: the theme
//! migration is about the dashboard's own chrome, never about the harness
//! output it hosts.

use std::collections::HashSet;
use std::path::PathBuf;

use chrono::{DateTime, FixedOffset, TimeZone};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};

use crate::style;

use super::super::attention::{Attention, Projection, SessionStatus, Visibility};
use super::super::mail::Message;
use super::actions::{self, ActionContext, ActionId, PaletteRow};
use super::hit::{FrameSnapshot, HintId, Hit};
use super::pane::PaneState;

/// Dash refresh PR1: one session's own bound workflow, resolved by
/// `dash::mod::resolve_session_workflow` from its `sessions::Record::
/// workflow_id`, never from the repo-wide `active_workflow_summary` pointer.
/// Shared by the sidebar's own 2-line fact block (line 2) and the pane
/// header's right segment -- the same fact, rendered twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWorkflowFact {
    /// `WorkflowKind::as_str()` -- e.g. `"bugfix"`.
    pub kind: String,
    /// The current step's id, or `"done"` once `completed` is set.
    pub step: String,
    /// 1-based position among `total` steps. `0`/`total` once `completed`.
    pub index: usize,
    pub total: usize,
    pub awaiting_approval: bool,
    /// The run finished (`WorkflowStatus::Completed`) within the last 10
    /// minutes -- `resolve_session_workflow` itself is what stops returning
    /// `Some` once that window has passed.
    pub completed: bool,
}

/// One enabled harness's cached subscription usage snapshot. No longer read
/// by the header itself (issue #202 phase 2b dropped the header's own usage
/// segment for width -- the header now has room only for the harness label,
/// the live count and the sticky error/notice line), but still filled by
/// `dash::mod`'s `FactsCache::refresh_if_due` each throttled tick and kept
/// here so that machinery -- and its own tests -- need no change; a future
/// surface (the errors overlay, a status line) can read it back without
/// re-deriving the read. `five_hour`/`seven_day`/`name` are already read by
/// `assemble_footer_facts`; issue #358 (task T6a) drops the blanket
/// `#[allow(dead_code)]` this struct used to carry now that it is no longer
/// landed ahead of every one of its fields' own call sites.
/// Dash refresh PR1: `window::Window`'s own fields, minus `used_percentage`
/// (already `HarnessUsage::five_hour`/`seven_day`) and `observed_at` (the
/// LIMITS block has no use for it). `resets_at` is epoch seconds,
/// vendor-reported (Claude's statusline, Codex's rate-limit events) -- there
/// is no cheap, scan-free way for the dashboard's own throttled tick to tell
/// a vendor reading apart from zirv's own estimate (`window::estimate_
/// windows`, only reachable through `pace::current_windows`, which sums
/// transcripts and is exactly the scan/poll this tick must never do), so
/// this build never marks one `~`-estimated; see `render_limits`'s own doc
/// comment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowDetail {
    pub resets_at: u64,
    pub limit_reached: bool,
    pub overage_covered: bool,
}

#[derive(Clone)]
pub struct HarnessUsage {
    pub name: &'static str,
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    /// Dash refresh PR1: the rest of the 5h window's own `window::Window`
    /// (`resets_at`/`limit_reached`/`overage_covered`) that `five_hour`
    /// alone drops -- the LIMITS block's own reset line needs all three.
    /// `None` exactly when `five_hour` is `None`.
    pub five_hour_detail: Option<WindowDetail>,
    /// The weekly window's own equivalent of `five_hour_detail`.
    pub seven_day_detail: Option<WindowDetail>,
    /// Whether this provider is currently metered by credits rather than a
    /// subscription window (`cfg.pace.use_credits`). Filled every throttled
    /// tick alongside its siblings, but no production render path has
    /// consulted it since issue #202 phase 2b -- kept its own narrow
    /// `#[allow(dead_code)]` (rather than reviving the whole struct's old
    /// blanket one) so a future credits-aware footer/status surface can
    /// still read it back without re-deriving the read.
    #[allow(dead_code)]
    pub credits: bool,
}

/// The header's live facts.
///
/// Dash refresh PR1 round 2: the left side no longer names the dashboard's
/// own launch identity (`chat.model` moved to the pane header, which stays
/// on screen for whichever pane is focused rather than the one fixed
/// identity the whole session launched with) -- it is `zirv`'s own brand
/// mark plus three session counts: `sessions` (every row the sidebar draws
/// that is not `Ended`/exited), `working` (`RowState::Working`) and
/// `needs_you` (`Glyph::NeedsAction`). A zero count omits its whole segment
/// (never "0 working"), so the row only ever names what is actually true
/// right now.
///
/// `error_count`/`latest_error` are the sticky `⚠` channel (`push_error`'s own
/// buffer); `notice` is the transient, auto-expiring informational channel
/// (`push_notice`/`live_notice`) and takes precedence over the error line
/// while it is fresh, exactly as it did before this phase.
pub struct HeaderFacts {
    pub hints: HintContext,
    pub sessions: usize,
    pub working: usize,
    pub needs_you: usize,
    pub error_count: usize,
    pub latest_error: Option<String>,
    pub notice: Option<String>,
    /// Issue #354 phase 4: the first-run tip ([`FIRST_RUN_TIP`]), shown in the
    /// same middle slot as a notice and with the same dim weight, but only on
    /// this operator's very first dashboard launch and only until any
    /// prefixed key is used or `Esc` dismisses it. Lowest precedence of the
    /// three: a real error or a live notice always wins the slot.
    pub tip: Option<&'static str>,
}

/// Issue #354 phase 4 (finding F15): what a brand-new operator is told, once.
/// Three facts, in the order they are worth learning -- where the key
/// reference is, where every action is, and that the roster is clickable.
///
/// Review of cc92a56 (finding 3): the two chords are READ OFF the one
/// action-descriptor table rather than spelled out here. A tip that hard-codes
/// `^A ?` is a fifth place a chord is written down, and the chord-audit test
/// (`every_chord_drawn_anywhere_comes_from_the_action_table`) now renders a
/// header with the tip up, so a rebinding that did not reach this string would
/// fail the audit instead of misleading a first-time operator.
pub static FIRST_RUN_TIP: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let hint = |id: actions::ActionId| match actions::descriptor(id) {
        Some(d) => format!("{} {}", d.chord, d.label),
        // Unreachable: the table's own exhaustiveness tests pin both ids.
        None => String::new(),
    };
    format!(
        "{} \u{b7} {} \u{b7} click a row to focus",
        hint(actions::ActionId::Help),
        hint(actions::ActionId::Palette)
    )
});

/// What the header's right-hand hint cluster is chosen against. Phase 2 adds
/// the two attention-derived states the approved contract names
/// (`needs action` and `ended`) alongside phase 1's plain `alive`; phase 3
/// adds `restorable`, which is what decides whether the ended cluster's own
/// `^A r restore` is drawn at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct HintContext {
    /// Whether the selected row is a live pane.
    pub alive: bool,
    /// Whether the selected row's glyph is `▲` -- something is waiting on the
    /// operator there.
    pub needs_action: bool,
    /// Whether the selected row is an ended pane (a retained completed-worker
    /// row, or one reaped and still on screen).
    pub ended: bool,
    /// Whether the selected row can actually be relaunched -- an ended row
    /// for which the dashboard still holds the spawn request that created it.
    /// A row that cannot be restored is not offered `^A r`: phase 3's own
    /// rule is that a drawn hint always does something (the context menu is
    /// where an unavailable action is named *and* explained instead).
    pub restorable: bool,
    /// Issue #354 phase 5: the cursor is on the sidebar's summary line, so
    /// the cluster describes the DASHBOARD -- `inspect` opens the
    /// dashboard-level inspector, and every per-session chord is dropped.
    pub summary: bool,
}

impl HintContext {
    /// The header's own slice of an [`ActionContext`]: the cluster only ever
    /// needs to know whether the selected row is alive, ended, waiting on the
    /// operator, and whether its spawn request is still held. Everything else
    /// an availability rule can ask about is either irrelevant to the four
    /// chords the cluster draws, or (`attached`, `retained`) implied by them.
    pub fn action_context(&self) -> ActionContext {
        ActionContext {
            selected: self.alive || self.ended,
            attached: self.alive,
            alive: self.alive,
            ended: self.ended,
            needs_action: self.needs_action,
            retained: self.ended,
            has_request: self.restorable,
            clean_exit: false,
            has_cwd: true,
            summary: self.summary,
        }
    }
}

/// Pure: the `(chord, label)` pairs the header offers for `context`, in
/// display order, at most four (the approved design's own cap).
///
/// Issue #354 phase 4: the cluster no longer keeps its own four hard-coded
/// lists. It picks at most four ids out of the one action-descriptor table
/// ([`actions::header_ids`]) and prints each descriptor's own `chord`/`label`
/// -- so a chord drawn here is, by construction, the same chord the help
/// screen, the palette and the context menu name for the same action, and
/// it can never be drawn for an action that is currently unavailable.
pub fn header_hints(context: &HintContext) -> Vec<(&'static str, &'static str)> {
    let ctx = context.action_context();
    actions::header_ids(&ctx)
        .into_iter()
        .filter_map(actions::descriptor)
        .map(|d| (d.chord, d.label))
        .collect()
}

/// The `HintId` a chord from [`header_hints`] reports when clicked. Unknown
/// chords fall back to `Help`, which is the one action that is always safe
/// and always available.
fn hint_id(key: &str) -> HintId {
    match key {
        "^A c" => HintId::Actions,
        "^A n" => HintId::Nudge,
        "^A m" => HintId::Mail,
        "^A e" => HintId::Errors,
        "^A i" => HintId::Inspect,
        "^A r" => HintId::Restore,
        _ => HintId::Help,
    }
}

/// Pure: where the header actually drew each hint chord, so a click on one can
/// be turned back into its action.
///
/// Derived from [`header_layout`]'s own single pass rather than re-deriving a
/// right-aligned block: the drawn cluster is right-aligned only while the row
/// has room for it, and a header shrunk below its fixed content pushes the
/// chords rightward off the row instead. Recomputing "right edge minus cluster
/// width" then produced rects sitting on top of the chip and the harness
/// label, so a click on visible left-hand text fired a header action and each
/// chord's rect named its neighbour's text (review of bf1474f). A chord no
/// part of which was drawn gets no rect at all.
pub fn header_hint_regions(area: Rect, facts: &HeaderFacts) -> Vec<(Rect, HintId)> {
    header_layout(facts, area).1
}

/// Issue #358 (task T6a): one harness's condensed pool status for the
/// aggregate row's own strip -- `allocator::HarnessState::as_str()`'s own
/// vocabulary (`"ready"`/`"draining"`/`"hard-blocked"`/`"unknown"`/
/// `"disabled"`), plus its binding window's raw headroom, `None` when it has
/// none. Kept as plain strings here (not the `allocator`/`fallback` types
/// themselves) so this module -- pure `ratatui` rendering, no state-dir or
/// config dependency of its own -- never has to import either.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessStrip {
    pub name: String,
    pub state: String,
    pub headroom_pct: Option<f64>,
}

/// The sidebar/grid state a row's leading glyph column renders: [`render_
/// sidebar`] picks the actual glyph character (and colour) from this plus the
/// live spinner tick, so nothing above this module needs to know the spinner
/// frame set at all.
///
/// `Unknown` is a view-only registry row this dashboard did not spawn: its
/// `PaneState` (Working/Idle) genuinely is not observable from here, only
/// that the process is still alive -- so it gets its own neutral glyph
/// (`·`), distinct from every state a real pane can report, rather than
/// guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Working,
    Idle,
    Dead,
    Unknown,
}

/// Pure: the sidebar row state a pane's own `PaneState` maps to. `Ended`
/// (any exit code) is always `Dead` -- the exit code itself is not part of
/// the glyph, exactly as the old `glyph_for` never encoded it either.
pub fn row_state_for(state: &PaneState) -> RowState {
    match state {
        PaneState::Working => RowState::Working,
        PaneState::Idle => RowState::Idle,
        PaneState::Ended(_) => RowState::Dead,
    }
}

/// The six -- and only six -- states the sidebar's glyph column can draw
/// (approved design, operator decision 3). Every one is a distinct *shape* as
/// well as a distinct colour: colour alone is never the carrier.
///
/// This is deliberately a second, narrower enum rather than more [`RowState`]
/// variants. `RowState` says what the *dashboard itself* can observe about a
/// pane (its `PaneState`, or nothing at all for a view-only row); `Glyph` is
/// what the composed [`super::super::attention`] model projects, which is a
/// strictly richer question and answerable for a session this dashboard has no
/// pane for. Keeping them apart is what makes the fallback in [`glyph_for`]
/// -- no status file, no guess -- expressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// `⠋` cyan.
    Working,
    /// `●` green.
    Idle,
    /// `▲` yellow -- [`Projection::Blocked`].
    NeedsAction,
    /// `◆` magenta -- finished, and nobody has looked at it yet.
    DoneUnread,
    /// `✗` red.
    Failed,
    /// `·` dim.
    Unknown,
}

/// The attention states, in the fixed order a rollup lists them. Any one of
/// them present in a rollup's members suppresses the quiet set entirely
/// (approved design's frame A vs. frame B).
const ATTENTION_GLYPHS: [Glyph; 3] = [Glyph::NeedsAction, Glyph::Failed, Glyph::DoneUnread];

/// The quiet states, in the fixed order a rollup lists them when nothing in
/// [`ATTENTION_GLYPHS`] is present.
const QUIET_GLYPHS: [Glyph; 3] = [Glyph::Working, Glyph::Idle, Glyph::Unknown];

/// Every glyph, attention first -- the order the dashboard inspector's own
/// counts section lists them in (issue #354 phase 5), which is the two rollup
/// tiers concatenated rather than a third hand-written order.
pub const ALL_GLYPHS: [Glyph; 6] = [
    Glyph::NeedsAction,
    Glyph::Failed,
    Glyph::DoneUnread,
    Glyph::Working,
    Glyph::Idle,
    Glyph::Unknown,
];

impl Glyph {
    /// The glyph's own word, for a surface that has room for one (the
    /// inspector) rather than a single column.
    pub const fn name(self) -> &'static str {
        match self {
            Glyph::Working => "working",
            Glyph::Idle => "idle",
            Glyph::NeedsAction => "needs action",
            Glyph::DoneUnread => "done-unread",
            Glyph::Failed => "failed",
            Glyph::Unknown => "unknown",
        }
    }

    /// The glyph's own character, spinner frame 0 for `Working` -- a static
    /// report is not animated.
    pub fn symbol(self) -> &'static str {
        glyph_char_for(self, 0)
    }
}

/// Pure: the [`Glyph`] a row draws, from its cached
/// [`SessionStatus`](super::super::attention::SessionStatus) when one exists
/// and from the pane's own [`RowState`] when it does not.
///
/// Three rules, in this order:
///
/// 1. **An ended pane's exit code decides, not the projection.**
///    `attention::project` maps every `Lifecycle::Exited` to
///    `Projection::Failed`, which would paint a worker that finished its job
///    and exited 0 red. A nonzero exit is `✗`; a clean one is `◆` until the
///    operator has actually seen it and `●` afterwards.
/// 2. **Otherwise the projection decides**, one-for-one.
/// 3. **`Projection::Unknown` -- which is exactly what a missing or
///    never-written status file reads back as -- falls through to
///    `RowState`**, so a dashboard with no issue #349 writers anywhere still
///    renders precisely the phase 1 sidebar rather than a column of `·`.
pub fn glyph_for(row: &SidebarRow) -> Glyph {
    if let Some(code) = row.exit_code {
        return if code != 0 {
            Glyph::Failed
        } else if row
            .status
            .as_ref()
            .is_some_and(|s| s.visibility == Visibility::Unseen)
        {
            Glyph::DoneUnread
        } else {
            Glyph::Idle
        };
    }
    match row.status.as_ref().map(super::super::attention::project) {
        Some(Projection::Working) => Glyph::Working,
        Some(Projection::Blocked(_)) => Glyph::NeedsAction,
        Some(Projection::DoneUnread) => Glyph::DoneUnread,
        Some(Projection::IdleSeen) => Glyph::Idle,
        Some(Projection::Failed) => Glyph::Failed,
        Some(Projection::Unknown) | None => match row.state {
            RowState::Working => Glyph::Working,
            RowState::Idle => Glyph::Idle,
            RowState::Dead => Glyph::Failed,
            RowState::Unknown => Glyph::Unknown,
        },
    }
}

/// One sidebar row: a dashboard pane (`attached: true`) or a view-only
/// registry session this dashboard did not spawn (`attached: false`).
///
/// `selected` and `focused` are deliberately two different things (F7).
/// `selected` is the sidebar cursor, which walks the *combined* row list --
/// view-only registry rows included, so a nudge can be aimed at a session
/// this dashboard does not own. `focused` is the pane whose grid is on
/// screen and whose child receives every un-prefixed keystroke, and it can
/// only ever be an attached pane. Before F7 the two were one index, so
/// selecting a view-only row blanked the grid and swallowed all typing.
///
/// `age_secs` is `None` when no registry record could be found for this row
/// (a race between a fresh spawn and its own registration, in practice) --
/// it renders as [`style::PLACEHOLDER`], never a fabricated `0s`.
///
/// `score` (issue #209/v3 §C, restored after #207 dropped it) is this row's
/// cached rot score -- `score::cached_score`'s own `None` means *unknown*,
/// never *healthy*, and renders the same dim placeholder a dead row's score
/// always does regardless of what is cached for it (a dead pane's last
/// reading is stale, not a live verdict).
/// Issue #354 added `role`, `model`, `group`, `tree` and `disclosure`: the
/// row now carries what the approved 44-column contract draws, rather than
/// the `{short} {harness}` pair it used to. `harness` stays -- the footer,
/// the focus rule and the usage lookup all still read it -- but it is no
/// longer a sidebar column of its own.
#[derive(Clone)]
pub struct SidebarRow {
    /// Short role label (`orch`, `sub-orch`, `worker`, ...), left-aligned in
    /// its 8-column field.
    pub role: String,
    /// The model this session was actually launched with, read back from the
    /// resolved adapter argv. `None` -- a restored or view-only row whose
    /// launch args this dashboard never saw -- renders the shared
    /// placeholder; it is never guessed from the request text.
    pub model: Option<String>,
    /// The work group this row belongs to, if any.
    pub group: Option<GroupRef>,
    /// Where the row sits in the tree; set by [`roster_frame`] as it lays the
    /// group out, not by whoever built the row.
    pub tree: TreePos,
    /// Ordered `(key, value)` facts the `^A i` per-row inspector reads back
    /// by key (`group`, `branch`, `since`, `budget`, `writer`, `signal`).
    /// Dash refresh PR1 replaced the sidebar's own 8-line rendering of this
    /// same vec with the 2-line fact block below (`fact_state`/
    /// `fact_since_secs`/`workflow`) -- kept here, unrendered by the sidebar
    /// now, purely because the inspector still has a live use for it; a key
    /// that fed ONLY the old sidebar block (`model`, `reason`) was dropped
    /// from this vec entirely rather than kept unused. Filled only for the
    /// selected row, and only from values already cached -- a disclosure
    /// line never costs a read.
    pub disclosure: Vec<(String, String)>,
    pub short: String,
    pub harness: String,
    pub age_secs: Option<u64>,
    pub score: Option<u32>,
    pub state: RowState,
    /// Dash refresh PR1: the fact block's own line 1, `{fact_state} ·
    /// {age}`. The state word -- the composed projection's own word
    /// (`working`, `needs approval`, ...) when a status exists, else the
    /// plain `RowState` word (`working`/`idle`/`ended`/`unknown`). Set for
    /// every row (not only the selected one): the pane header's own right
    /// segment reads it off the FOCUSED row, which need not be the row the
    /// sidebar cursor is currently on.
    pub fact_state: String,
    /// The fact block/pane-header's own elapsed clock: seconds since the
    /// state word's own last transition (or, with no status, since the row
    /// was first observed in this `RowState`). `None` when nothing has ever
    /// been recorded yet.
    pub fact_since_secs: Option<u64>,
    /// This row's OWN bound workflow (`sessions::Record::workflow_id`,
    /// resolved by `dash::mod::resolve_session_workflow`), never the
    /// repo-wide pointer. `None` when this session started no workflow, or
    /// its run could not be resolved. Read by both the fact block's line 2
    /// and the pane header's workflow segment.
    pub workflow: Option<SessionWorkflowFact>,
    /// Total unread mail (broadcast + direct) for this session, the badge
    /// column's second-priority glyph (`✉N`, `✉+` above 9) -- a workflow
    /// gate awaiting approval (`⚑`) always wins when both are true.
    pub unread_mail: usize,
    /// Issue #354 phase 2: this session's composed attention status as of the
    /// last `FactsCache` refresh (`attention::load`), never a per-frame read.
    /// `None` means this row was built before the first refresh; a status that
    /// projects `Unknown` (the shape a missing file loads back as) is treated
    /// exactly the same way -- see [`glyph_for`].
    pub status: Option<SessionStatus>,
    /// `Some(code)` for a **retained ended row**: a completed pane the roster
    /// keeps on screen after `reap_ended_panes` removed the `Pane` itself.
    /// Never set for a live pane, and it is what makes a clean exit
    /// distinguishable from a failure (see [`glyph_for`]).
    pub exit_code: Option<i32>,
    pub attached: bool,
    pub selected: bool,
    pub focused: bool,
    /// Issue #209/v3 codex review finding 5: `Pane::reachable()` for an
    /// attached row (whether this pane's own turn-signal socket bound at
    /// spawn time); `true` for a view-only registry row, which has no
    /// `Pane` of its own to ask and can never be `focused` anyway (see
    /// `focused`'s own doc comment) -- the footer is the only reader, and
    /// it only ever reads this off the focused row.
    pub supervised: bool,
}

/// The work group a row belongs to. Membership comes only from
/// `Pane::work_group_id`, never from a shared cwd (operator decision 4), and
/// `scope` is filled in from the throttled `group::load` cache -- never read
/// per frame -- so it carries the shared placeholder until that cache has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRef {
    pub id: String,
    pub scope: String,
    /// The group's lead: its sub-orchestrator, else its first member in spawn
    /// order. Rendered as the group header's first child.
    pub lead_short: String,
}

/// Where a row sits in the roster's tree, which is exactly what its
/// two-column tree prefix draws. A group *header* is not a `TreePos` at all:
/// headers are their own roster entry, built by [`roster_frame`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TreePos {
    /// Ungrouped. Keeps the same glyph column as a grouped row, so the state
    /// glyphs line up down the whole sidebar.
    #[default]
    Flat,
    Child,
    /// The last child of a group: closes the group's vertical line.
    LastChild,
}

impl TreePos {
    /// The row's own ONE-column tree prefix (dash refresh PR1 narrowed this
    /// from two columns to one, part of the 44 -> 28 column row contract).
    fn prefix(self) -> &'static str {
        match self {
            Self::Flat => " ",
            Self::Child => "\u{251c}",
            Self::LastChild => "\u{2514}",
        }
    }
}

/// A minimal draft/view struct shared by the overlay seams below. Only what
/// `render_overlay` needs to draw something today; Task 12 fills in richer
/// fields as it wires up the restore dialog's own reducer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnDraft {
    pub input: String,
    pub items: Vec<String>,
    pub cursor: usize,
}

/// Where a submitted nudge goes: an attached pane this dashboard owns
/// (`AttachedPane` -- idle-gated visible injection, or queued if the pane is
/// still `Working`), or a session this dashboard did not spawn
/// (`ViewOnlySession`, routed through `sessions::run_nudge_with`'s existing
/// headless marker+mail semantics). `None` when nothing was selected at the
/// moment `prefix,n` was pressed.
///
/// D1: `AttachedPane` names the pane by its **registry short id**, exactly as
/// `ViewOnlySession` does, not by its index in the live `panes` vector. The
/// dialog stays open across as many ticks as the operator takes to type, and a
/// pane reaped (or spawned) in the meantime shifts every index after it -- so
/// a captured index quietly re-aimed the nudge at whichever pane had slid into
/// that slot. A short id either still names a live pane at Enter time or names
/// nothing at all, and "nothing" is a notice, never a misdelivery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NudgeTarget {
    #[default]
    None,
    AttachedPane(String),
    ViewOnlySession(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NudgeDraft {
    pub target: NudgeTarget,
    pub input: String,
}

/// One message-in-progress: `to` defaults to `"any"` when left blank (the
/// same default `mail::SendArgs` uses), `body` is what the operator types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposeDraft {
    pub to: String,
    pub body: String,
}

/// The mail overlay's own state: every unread message visible to the
/// dashboard operator (`(path, from_agent, body_preview)`, oldest first --
/// the same order `mail::list` already returns), which one is selected, and
/// an in-progress compose draft when the operator is writing a new one
/// rather than browsing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailView {
    pub items: Vec<(PathBuf, String, String)>,
    pub cursor: usize,
    /// Issue #354 phase 3: the shared list viewport's first drawn row. The
    /// reducer keeps it, `list_dialog_layout` clamps it so the cursor is
    /// always visible, and it is what makes a 40-message inbox readable in a
    /// dialog with room for twelve rows.
    pub offset: usize,
    pub compose: Option<ComposeDraft>,
}

/// What executing a `MailView` reducer's emitted action actually does to
/// storage -- executed by `dash::mod`'s own `apply_mail_effect`, never by the
/// reducer itself (the reducer stays pure). `Send`'s `Message` carries
/// placeholder `from_session`/`from_agent`/`sent` fields the executor
/// overwrites right before `mail::store`, keeping the reducer itself
/// identity- and clock-free, the same discipline `rot.rs` and `prompt.rs`
/// already hold themselves to.
#[derive(Debug, Clone, PartialEq)]
pub enum MailEffect {
    Consume(PathBuf),
    Send(Message),
}

/// Issue #84: the handover picker's own state. `items` is precomputed by
/// `dash::mod` (every enabled+ready harness crossed with `handover::TIERS`,
/// each already tier-resolved to a concrete model label) as `(agent, tier,
/// resolved model)`; `target_short` names the pane the swap applies to,
/// captured once when the overlay opens the same way `NudgeTarget` is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandoverDraft {
    pub items: Vec<(String, String, String)>,
    pub cursor: usize,
    /// The shared list viewport's first drawn row (issue #354 phase 3).
    pub offset: usize,
    pub target_short: String,
}

/// The memory overlay's own state: every entry in this repo's bank
/// (`(key, age, body)`, oldest-written first -- the same order `memory::list`
/// already returns), which one is selected, and an in-progress edit buffer
/// when the operator is remembering new text for the selected key rather
/// than browsing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryView {
    pub entries: Vec<(String, String, String)>,
    pub cursor: usize,
    /// The shared list viewport's first drawn row (issue #354 phase 3).
    pub offset: usize,
    pub input: Option<String>,
}

/// What executing a `MemoryView` reducer's emitted action does to storage --
/// executed by `dash::mod`'s own `apply_memory_effect`. `Remember` carries
/// only `key`/`body`: `written_by`/timestamps/`source` are filled in by the
/// executor the same way `run_remember_with` fills them for the CLI verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryEffect {
    Remember { key: String, body: String },
    Forget(String),
    Verify(String),
}

/// One roster candidate offered in the restore dialog: a human-readable
/// `label` (title/agent/short, assembled by `dash::mod` from the roster
/// entry it mirrors) and whether the operator currently has it checked for
/// restore. Indices into `RestoreView::entries` line up 1:1 with the
/// `Vec<roster::RosterPane>` candidate list `dash::mod` keeps alongside the
/// view -- neither list is ever reordered, only toggled -- so an effect that
/// names indices is enough for the caller to find the roster data back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreEntry {
    pub label: String,
    pub checked: bool,
}

/// The startup restore dialog's own state: every worker pane offered back
/// from the previous quit's roster (the orchestrator is excluded before this
/// view is ever built -- see `dash::mod::run_dashboard`'s own doc comment),
/// each independently checked/unchecked, defaulting to checked so Enter
/// alone restores everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreView {
    pub entries: Vec<RestoreEntry>,
    pub cursor: usize,
    /// The shared list viewport's first drawn row (issue #354 phase 3).
    pub offset: usize,
}

/// One row of the `Ctrl+A e` dialog: an error message, how many times it
/// repeated consecutively, how long ago the most recent repeat was, and
/// whether the operator has acknowledged it (issue #354 phase 5).
///
/// A snapshot of `dash::mod`'s own `ErrorLog` entry, taken when the overlay
/// opens -- the dialog does not re-read the buffer while it is up, the same
/// convention every other overlay here follows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorItem {
    pub text: String,
    /// `1` for a message seen once; `3` renders as `\u{d7}3`.
    pub count: usize,
    /// Seconds since the most recent repeat.
    pub age_secs: u64,
    /// Acknowledged entries stay listed, dimmed, until the buffer cap drops
    /// them -- acknowledgement is never a delete.
    pub acked: bool,
}

/// `Ctrl+A e`'s own state: the kept errors from `push_error`'s buffer
/// (`MAX_KEPT_ERRORS`), newest first -- built once when the overlay opens
/// (`dash::mod::build_errors_view`), not re-read live while it is up, the
/// same snapshot-on-open convention every other overlay here already uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorsView {
    pub items: Vec<ErrorItem>,
    pub cursor: usize,
    /// The shared list viewport's first drawn row (issue #354 phase 3).
    pub offset: usize,
    /// A1-3: which entries this snapshot covers -- every kept error whose id
    /// is below this. Closing the dialog acknowledges exactly those, so an
    /// error that arrived while the dialog was open keeps holding the sticky
    /// header line. `0` (the `Default`) covers nothing.
    pub mark: u64,
}

/// Issue #354 phase 3: one thing the context menu can do to its target row.
///
/// Every variant maps onto machinery the dashboard already has -- an overlay
/// the keyboard can already open, the roster's own selection/focus move, the
/// pane shutdown the quit path uses, or a relaunch of the very
/// `spawnreq::SpawnRequest` that created the row. Nothing here builds an
/// argv, and nothing here is a new process-launch path (`Command Safety`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// Open the inspector on the target row.
    Inspect,
    /// Give the target row the keyboard.
    Focus,
    /// Open the nudge composer aimed at the target row.
    Nudge,
    /// Open the mail overlay.
    Mail,
    /// Open the handover picker for the target row.
    Handover,
    /// Ask the target pane's harness to quit (the quit path's own
    /// `Pane::request_quit`), after an inline confirmation.
    Stop,
    /// Relaunch an ended row from the spawn request kept for it.
    Restore,
    /// Show the target row's checkout path in the header notice, and open the
    /// inspector on it. Deliberately NOT a shell or editor launch: spawning
    /// one would be a new process-launch path outside the spawn machinery.
    OpenWorktree,
    /// Open the inspector scrolled to its evidence section.
    Evidence,
    /// Exactly [`MenuAction::Restore`], named for the failure case: an ended
    /// row whose child exited non-zero.
    Retry,
    /// Drop a retained ended row from the roster.
    Dismiss,
}

impl MenuAction {
    /// The entry's own label, which is also what its letter jump is derived
    /// from (see `menu_letters`).
    pub fn label(self) -> &'static str {
        match self {
            MenuAction::Inspect => "inspect",
            MenuAction::Focus => "focus",
            MenuAction::Nudge => "nudge",
            MenuAction::Mail => "mail",
            MenuAction::Handover => "handover",
            MenuAction::Stop => "stop",
            MenuAction::Restore => "restore",
            MenuAction::OpenWorktree => "open worktree",
            MenuAction::Evidence => "evidence",
            MenuAction::Retry => "retry",
            MenuAction::Dismiss => "dismiss",
        }
    }
}

/// One context-menu row: what it does, and -- when it cannot be done for this
/// target -- the short reason, which is rendered dim on the same row rather
/// than the entry being hidden. An operator who cannot see that `restore`
/// exists cannot learn why it is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub action: MenuAction,
    /// `None` when the entry is available.
    pub disabled: Option<String>,
    /// The letter that jumps to this entry, unique within the menu; `None`
    /// when every letter of the label was already taken.
    pub letter: Option<char>,
}

impl MenuEntry {
    pub fn enabled(&self) -> bool {
        self.disabled.is_none()
    }
}

/// Pure: one jump letter per entry, unique within the menu.
///
/// The first letter of the label wherever it is still free, otherwise the
/// next free letter of that same label -- so `restore` keeps `r` and `retry`,
/// which follows `evidence`, takes `t`. `None` only when every letter of a
/// label was already claimed, which no menu this phase builds reaches; a
/// letterless entry is still selectable with the caret.
pub fn menu_letters(actions: &[MenuAction]) -> Vec<Option<char>> {
    let mut taken: HashSet<char> = HashSet::new();
    actions
        .iter()
        .map(|action| {
            let letter = action
                .label()
                .chars()
                .find(|c| c.is_ascii_alphabetic() && !taken.contains(c));
            if let Some(c) = letter {
                taken.insert(c);
            }
            letter
        })
        .collect()
}

/// `Ctrl+A c`, a right-click on a row, or the header's `actions` hint: the
/// menu for ONE row, named in the title, which is not necessarily the
/// selected one (a right-click targets whatever it landed on).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MenuView {
    /// The target row's session short id, captured when the menu opened.
    pub target: String,
    /// `<short> · <role>` -- what the title says the menu applies to.
    pub subject: String,
    pub entries: Vec<MenuEntry>,
    pub cursor: usize,
    /// First entry drawn; moved only by the shared list viewport.
    pub offset: usize,
    /// Set while an inline confirmation is up for the entry at this index
    /// (`stop`, the one destructive action here). `y`/Enter confirms, `n`/Esc
    /// backs out; nothing is killed until it does.
    pub confirm: Option<usize>,
}

/// One titled block of the inspector. `lines` are already formatted
/// `key  value` strings -- the inspector is a read-only report, so there is
/// nothing for the renderer to decide per line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorSection {
    pub name: String,
    pub lines: Vec<String>,
}

/// `Ctrl+A i`, or the context menu's `inspect`/`evidence`: everything the
/// dashboard already knows about one row, in sections, read-only.
///
/// Every fact comes from something already cached on the `FactsCache`
/// cadence (the composed `attention::SessionStatus`, the row's own
/// disclosure values, the kept-errors buffer) -- the inspector never reads
/// the disk, never shells out, and costs nothing per frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorView {
    /// The row this inspects, by session short id.
    pub target: String,
    pub subject: String,
    pub sections: Vec<InspectorSection>,
    /// The line the caret sits on, in the flattened row list -- which is what
    /// "opened at the evidence section" means mechanically.
    pub cursor: usize,
    pub offset: usize,
}

impl InspectorView {
    /// Pure: this inspector's sections flattened into the rows the list
    /// dialog draws -- one header line per section, then its lines indented.
    /// The single source of truth for both the render and `section_start`
    /// below, so a caret aimed at a section can never land somewhere else.
    pub fn rows(&self) -> Vec<String> {
        let mut rows = Vec::new();
        for (i, section) in self.sections.iter().enumerate() {
            if i > 0 {
                rows.push(String::new());
            }
            rows.push(format!("{}:", section.name));
            if section.lines.is_empty() {
                rows.push(format!("  {}", style::PLACEHOLDER));
            } else {
                rows.extend(section.lines.iter().map(|line| format!("  {line}")));
            }
        }
        rows
    }

    /// Pure: the flattened row index a named section's header sits on, or
    /// `0` when it has none -- so `evidence` opening "scrolled to evidence"
    /// is a caret position, not a second layout pass.
    pub fn section_start(&self, name: &str) -> usize {
        let mut index = 0usize;
        for (i, section) in self.sections.iter().enumerate() {
            if i > 0 {
                index += 1;
            }
            if section.name == name {
                return index;
            }
            index += 1 + self.sections[i].lines.len().max(1);
        }
        0
    }
}

/// Which of the two faces the one palette overlay is wearing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaletteMode {
    /// `Ctrl+A p`: Enter runs the selected action.
    #[default]
    Run,
    /// `Ctrl+A ?`/`h`/`H`: the same list, the same filter, but read-only --
    /// Enter closes rather than running, so the key reference can never fire
    /// an action an operator only meant to read about.
    Help,
}

impl PaletteMode {
    pub fn title(self) -> &'static str {
        match self {
            PaletteMode::Run => "palette",
            PaletteMode::Help => "help",
        }
    }
}

/// Issue #354 phase 4: the palette/help overlay's own state.
///
/// `ctx` is the availability snapshot taken when the overlay opened -- the
/// same convention every other overlay here already follows (mail, memory and
/// restore do not live-update while open either), and what keeps the rows a
/// pure function of the view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaletteView {
    pub mode: PaletteMode,
    /// What the operator has typed. Never forwarded to the child: an open
    /// overlay owns every keystroke.
    pub query: String,
    pub ctx: ActionContext,
    pub cursor: usize,
    pub offset: usize,
}

impl PaletteView {
    /// Pure: the rows this view draws right now.
    pub fn rows(&self) -> Vec<PaletteRow> {
        actions::palette_rows(&self.ctx, &self.query)
    }

    /// Pure: the action under the caret, when there is one that can be run.
    /// A section heading, a disabled row and a read-only (help) view all
    /// yield `None` -- the caller has nothing to do.
    pub fn activated(&self) -> Option<ActionId> {
        if self.mode == PaletteMode::Help {
            return None;
        }
        match self.rows().get(self.cursor) {
            Some(row @ PaletteRow::Action { id, .. }) if row.activatable() => Some(*id),
            _ => None,
        }
    }
}

/// What, if anything, sits drawn on top of the grid right now. `QuitConfirm`
/// carries the titles of every pane still `Working`, so the confirmation
/// text can name what the operator is about to interrupt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Overlay {
    #[default]
    None,
    QuitConfirm(Vec<String>),
    Spawn(SpawnDraft),
    Nudge(NudgeDraft),
    Mail(MailView),
    Memory(MemoryView),
    /// Issue #84: `Ctrl+A o` -- picks an enabled harness/tier to swap the
    /// target pane's model or harness to in place.
    Handover(HandoverDraft),
    /// The startup restore dialog, built once from the previous quit's
    /// roster (`dash::mod::run_dashboard`); never re-opened later in a
    /// session's life the way the other overlays are.
    Restore(RestoreView),
    /// Issue #354 phase 4: `Ctrl+A p` (the searchable palette) and
    /// `Ctrl+A ?`/`h`/`H` (the same list, read-only, as the key reference).
    /// One overlay for both: the help screen IS the palette without
    /// Enter-to-run, which is strictly less code than two dialogs that have
    /// to agree about the same table.
    Palette(PaletteView),
    /// `Ctrl+A e`: the kept-errors overlay.
    Errors(ErrorsView),
    /// Issue #354 phase 3: `Ctrl+A c`, a right-click on a row, or the
    /// header's `actions` hint -- the target row's action menu.
    Menu(MenuView),
    /// Issue #354 phase 3: `Ctrl+A i`, or the menu's `inspect`/`evidence` --
    /// the per-row inspector.
    Inspector(InspectorView),
}

impl Overlay {
    /// Pure: `(cursor, offset, len)` of whichever list this overlay is
    /// currently showing, or `None` when it is not showing one (a compose or
    /// edit buffer, a free-text prompt, a cursor-less dialog like help or
    /// the quit confirmation).
    ///
    /// Issue #354 phase 3: this is what lets the shared viewport keys --
    /// PageUp/PageDown/Home/End and the wheel -- be handled ONCE, for every
    /// list dialog, without every per-dialog reducer growing a capacity
    /// argument. Each reducer keeps its own semantics for its own keys.
    pub fn list_state(&self) -> Option<(usize, usize, usize)> {
        match self {
            Overlay::Mail(view) if view.compose.is_none() => {
                Some((view.cursor, view.offset, view.items.len()))
            }
            Overlay::Memory(view) if view.input.is_none() => {
                Some((view.cursor, view.offset, view.entries.len()))
            }
            Overlay::Restore(view) => Some((view.cursor, view.offset, view.entries.len())),
            Overlay::Handover(view) => Some((view.cursor, view.offset, view.items.len())),
            Overlay::Errors(view) => Some((view.cursor, view.offset, view.items.len())),
            Overlay::Menu(view) => Some((view.cursor, view.offset, view.entries.len())),
            Overlay::Inspector(view) => Some((view.cursor, view.offset, view.rows().len())),
            Overlay::Palette(view) => Some((view.cursor, view.offset, view.rows().len())),
            _ => None,
        }
    }

    /// Pure: writes a caret/viewport pair back. A no-op for an overlay with
    /// no list of its own.
    pub fn set_list_state(&mut self, cursor: usize, offset: usize) {
        match self {
            Overlay::Mail(view) => {
                view.cursor = cursor;
                view.offset = offset;
            }
            Overlay::Memory(view) => {
                view.cursor = cursor;
                view.offset = offset;
            }
            Overlay::Restore(view) => {
                view.cursor = cursor;
                view.offset = offset;
            }
            Overlay::Handover(view) => {
                view.cursor = cursor;
                view.offset = offset;
            }
            Overlay::Errors(view) => {
                view.cursor = cursor;
                view.offset = offset;
            }
            Overlay::Menu(view) => {
                view.cursor = cursor;
                view.offset = offset;
            }
            Overlay::Inspector(view) => {
                view.cursor = cursor;
                view.offset = offset;
            }
            Overlay::Palette(view) => {
                view.cursor = cursor;
                view.offset = offset;
            }
            _ => {}
        }
    }
}

/// Pure: how many rows the header gets in an `area_height`-row frame.
///
/// Exactly one -- the focused instance's own summary, which [`render_
/// header`] guarantees fits any width.
///
/// `min` rather than arithmetic: `area.height` is genuinely `0` in real
/// frames, and the release profile is `panic = "abort"`.
pub(crate) fn header_rows(area_height: u16) -> u16 {
    1.min(area_height)
}

/// Pure: how many rows each of the frame's fixed (non-body) chrome pieces
/// get, in `(header, rule_top, rule_bottom, footer)` order -- issue #209/v3
/// §A4/§D replaces the sidebar's own full rounded box with a full-width rule
/// above and below the body plus a new footer row, mirroring the header.
///
/// Reserved in priority order, each capped at 1.min(remaining): the header
/// first (unchanged from before this phase), then the footer (the new
/// signal row this phase adds -- kept as close to the header's own
/// guarantee as the remaining height allows), then the two rules last,
/// since they are pure decoration and a terminal too short for all four
/// should lose the decoration before it loses either signal row. Every
/// subtraction is guarded (`saturating_sub`/`.min`), so a zero- or one-row
/// frame degrades to all-zero chrome rather than underflowing -- the
/// release profile is `panic = "abort"`.
pub(crate) fn chrome_rows(area_height: u16) -> (u16, u16, u16, u16) {
    let header_h = header_rows(area_height);
    let remaining = area_height.saturating_sub(header_h);
    let footer_h = 1.min(remaining);
    let remaining = remaining.saturating_sub(footer_h);
    let rule_top_h = 1.min(remaining);
    let remaining = remaining.saturating_sub(rule_top_h);
    let rule_bottom_h = 1.min(remaining);
    (header_h, rule_top_h, rule_bottom_h, footer_h)
}

/// Every rect [`layout`] hands the render loop, named rather than
/// positional: `header`/`sidebar`/`main`/`footer` are drawn into directly;
/// `rule_top`/`rule_bottom` are the full-width flat rules that replace the
/// sidebar's old box border (issue #209/v3 §A4), each zero-height on a frame
/// too short to afford it (see [`chrome_rows`]).
///
/// Dash refresh PR1 adds `sidebar_title`/`pane_header` (one shared row: the
/// sidebar's own ` SESSIONS N` title beside the focused pane's own header)
/// and `mid_rule` (the `─...┼...─` rule right below them, aligned with the
/// sidebar/main divider) -- `sidebar`/`main` themselves now start BELOW
/// those two rows, so a pane's child pty is sized to the grid it actually
/// gets, never the two rows above it. All three are zero-height on a frame
/// too short to afford them, the identical `1.min(remaining)` degrade every
/// other chrome row already uses.
pub struct DashLayout {
    pub header: Rect,
    pub rule_top: Rect,
    /// The sidebar's own title row (` SESSIONS N`), same width as `sidebar`.
    pub sidebar_title: Rect,
    /// The focused pane's own header row, same width as `main`.
    pub pane_header: Rect,
    /// Full-width rule directly below `sidebar_title`/`pane_header`, with a
    /// `┼` junction at the sidebar/main divider column.
    pub mid_rule: Rect,
    pub sidebar: Rect,
    pub main: Rect,
    pub rule_bottom: Rect,
    pub footer: Rect,
}

/// Dash refresh PR1: below 100 total columns the session column hides
/// outright (the header shows sessions as tabs instead, `render_header_
/// tabs`) -- unless the operator has toggled it back on (`^A b`,
/// `DashAction::ToggleSidebar`), which holds regardless of width until
/// toggled again. Pure: `dash::mod`'s own `effective_sidebar_cols` is the
/// ONE place this feeds `sidebar_cols` for every geometry read (`layout`,
/// `effective_main`, pty resize) from, recomputed at every point the real
/// terminal width can change and on the toggle itself, so a narrow
/// terminal and a forced-visible toggle can never disagree about how wide
/// the sidebar actually is this frame.
pub fn sidebar_hidden(frame_width: u16, forced_visible: bool) -> bool {
    frame_width < 100 && !forced_visible
}

/// Splits `area` into every chrome rect a v3 frame draws: one header row, a
/// full-width top rule, a `sidebar_cols`-wide sidebar with a one-column
/// divider before the grid, a bottom rule mirroring the top one, and one
/// footer row (§D) -- see [`DashLayout`] and [`chrome_rows`] for how each
/// piece's height is decided. Dash refresh PR1: the body's own top two rows
/// go to the title/pane-header row and the rule below it (see
/// [`DashLayout`]'s own doc comment) before `sidebar`/`main` get the rest.
pub fn layout(area: Rect, sidebar_cols: u16) -> DashLayout {
    let (header_h, rule_top_h, rule_bottom_h, footer_h) = chrome_rows(area.height);
    let header = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: header_h,
    };

    let rule_top = Rect {
        x: area.x,
        y: area.y + header_h,
        width: area.width,
        height: rule_top_h,
    };

    let body_y = area.y + header_h + rule_top_h;
    let body_h_total = area
        .height
        .saturating_sub(header_h + rule_top_h + rule_bottom_h + footer_h);
    let sidebar_w = sidebar_cols.min(area.width);
    // No sidebar at all (dash refresh PR1's own narrow-terminal hide,
    // `sidebar_cols` config'd to 0) means no separator column either --
    // `main` gets the whole frame, not `frame width - 1` for a divider with
    // nothing to divide.
    let separator = if sidebar_w > 0 && area.width > sidebar_w {
        1
    } else {
        0
    };
    let main_x = area.x + sidebar_w + separator;
    let main_w = area.width.saturating_sub(sidebar_w + separator);

    // Reserved in the same priority order every other chrome row already
    // follows: the title/pane-header row first, the rule below it second --
    // a terminal too short for both loses the rule before it loses the row
    // that actually carries information.
    let title_h = 1.min(body_h_total);
    let remaining = body_h_total.saturating_sub(title_h);
    let mid_rule_h = 1.min(remaining);
    let body_h = body_h_total.saturating_sub(title_h + mid_rule_h);
    let body_y_below_title = body_y + title_h + mid_rule_h;

    let sidebar_title = Rect {
        x: area.x,
        y: body_y,
        width: sidebar_w,
        height: title_h,
    };
    let pane_header = Rect {
        x: main_x,
        y: body_y,
        width: main_w,
        height: title_h,
    };
    let mid_rule = Rect {
        x: area.x,
        y: body_y + title_h,
        width: area.width,
        height: mid_rule_h,
    };

    let sidebar = Rect {
        x: area.x,
        y: body_y_below_title,
        width: sidebar_w,
        height: body_h,
    };

    let main = Rect {
        x: main_x,
        y: body_y_below_title,
        width: main_w,
        height: body_h,
    };

    let rule_bottom = Rect {
        x: area.x,
        y: body_y + body_h_total,
        width: area.width,
        height: rule_bottom_h,
    };

    let footer = Rect {
        x: area.x,
        y: body_y + body_h_total + rule_bottom_h,
        width: area.width,
        height: footer_h,
    };

    DashLayout {
        header,
        rule_top,
        sidebar_title,
        pane_header,
        mid_rule,
        sidebar,
        main,
        rule_bottom,
        footer,
    }
}

/// Issue #209/v3 §A4/§D: the full-width flat rule that replaces the
/// sidebar's old box border, drawn once above the body (mirroring the
/// header) and once below it (mirroring the footer). `divider_col` is the
/// sidebar's own width -- the rule draws a `┬`/`┴` junction there against
/// the sidebar/grid divider, `top` selects which junction glyph. Dim,
/// matching `--t-line` in the approved mock, same as
/// [`render_sidebar_divider`].
pub fn render_rule(f: &mut Frame, area: Rect, divider_col: u16, top: bool) {
    if area.is_empty() {
        return;
    }
    let cols = area.width as usize;
    let junction_at = (divider_col as usize).min(cols.saturating_sub(1));
    let junction = if top { '\u{252c}' } else { '\u{2534}' };
    let mut line = String::with_capacity(cols);
    for col in 0..cols {
        line.push(if col == junction_at && divider_col < area.width {
            junction
        } else {
            '\u{2500}'
        });
    }
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(line, style::tui::muted()))),
        area,
    );
}

/// Dash refresh PR1: the rule directly below the sidebar's title row and the
/// focused pane's own header row (`DashLayout::mid_rule`) -- a `┼` junction
/// at the sidebar/main divider column rather than [`render_rule`]'s own
/// `┬`/`┴`, since both sides have a row above AND below this one.
pub fn render_mid_rule(f: &mut Frame, area: Rect, divider_col: u16) {
    if area.is_empty() {
        return;
    }
    let cols = area.width as usize;
    let junction_at = (divider_col as usize).min(cols.saturating_sub(1));
    let mut line = String::with_capacity(cols);
    for col in 0..cols {
        line.push(if col == junction_at && divider_col < area.width {
            '\u{253c}'
        } else {
            '\u{2500}'
        });
    }
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(line, style::tui::muted()))),
        area,
    );
}

/// Maps a `vt100` color to its `ratatui` equivalent. The first sixteen
/// indexed colors are translated to ratatui's named ANSI variants (`Idx(1)`
/// -> `Color::Red`, not `Color::Indexed(1)`) rather than left as `Indexed`:
/// `ratatui::style::Color`'s `PartialEq` is derived, so `Indexed(1)` and
/// `Red` never compare equal even though most terminals render the same
/// palette entry for both -- anything that compares a rendered cell's color
/// against a named `Color` (see `grid_renders_vt100_cells_with_colours`
/// below) needs the named variant. Indices 16-255 have no named equivalent
/// and stay `Indexed`.
fn map_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        vt100::Color::Idx(i) => match i {
            0 => Color::Black,
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Yellow,
            4 => Color::Blue,
            5 => Color::Magenta,
            6 => Color::Cyan,
            7 => Color::Gray,
            8 => Color::DarkGray,
            9 => Color::LightRed,
            10 => Color::LightGreen,
            11 => Color::LightYellow,
            12 => Color::LightBlue,
            13 => Color::LightMagenta,
            14 => Color::LightCyan,
            15 => Color::White,
            other => Color::Indexed(other),
        },
    }
}

/// Pure: the header's spans, width-budgeted to `cols`.
///
/// Ordered exactly as the design calls for: the ` zirv ` chip, the harness
/// label (bold, standing in for the generic app name -- see [`HeaderFacts`]'s
/// own doc comment for why this is `harness` rather than a literal "dash"),
/// the live/total count (muted), then the one flexible segment -- the sticky
/// error line or a transient notice -- and finally the hint cluster, always
/// kept, always on the right. Issue #697 removed select mode (`Ctrl+A v`)
/// entirely, so there is no longer a `SELECT` marker to reserve room for
/// here -- the dashboard now keeps its own mouse reporting on for the whole
/// session (subject only to `dash.mouse`, an operator/config decision, not
/// a runtime toggle this header would need to reflect).
///
/// Only the flexible middle ever loses a character to width pressure: the
/// fixed chrome (chip, harness, live count, hints) is reserved first, and
/// what's left over is the message's own ellipsis-truncation budget. A
/// terminal narrower than the fixed chrome alone still never panics --
/// `render_header` hands whatever this returns to a `Paragraph`, which clips
/// to `area` on its own -- but is not going to look polished either; that
/// floor is well below `chrome::MIN_DASH_COLS` and is accepted the same way
/// the old header's own extreme-width tests were.
fn header_layout(facts: &HeaderFacts, area: Rect) -> (Vec<Span<'static>>, Vec<(Rect, HintId)>) {
    let cols = area.width as usize;

    let hints = header_hints(&facts.hints);
    let hints_w = hints
        .iter()
        .map(|(k, l)| k.len() + l.len() + 1)
        .sum::<usize>()
        + hints.len().saturating_sub(1) * 2;
    let gap_before_hints = 2usize;

    // `▌zirv` is the brand mark; the counts after it are `sessions` (always
    // shown) then `working`/`needs_you`, each only when nonzero -- a zero
    // count omits its whole segment (never "0 working"), so the row only
    // ever names what is actually true right now. Every segment is short
    // and digit-bounded (unlike the old free-text harness/model label this
    // replaces), so unlike that label this cluster is never itself
    // ellipsis-truncated; only the flexible middle slot gives up room.
    let mut left: Vec<(String, Style)> = vec![
        ("\u{258c}".to_string(), Style::default().fg(Color::Cyan)),
        ("zirv".to_string(), style::tui::title()),
    ];
    let mut counts: Vec<(String, Style)> =
        vec![(format!("{} sessions", facts.sessions), style::tui::muted())];
    if facts.working > 0 {
        counts.push((
            format!("{} working", facts.working),
            Style::default().fg(Color::Cyan),
        ));
    }
    if facts.needs_you > 0 {
        counts.push((
            format!("{} needs you", facts.needs_you),
            style::tui::warning(),
        ));
    }
    for (text, style) in counts {
        left.push((" \u{b7} ".to_string(), style::tui::muted()));
        left.push((text, style));
    }
    let left_w: usize = left.iter().map(|(t, _)| style::display_width(t)).sum();

    let reserved = left_w + hints_w + gap_before_hints;
    let middle_budget = cols.saturating_sub(reserved);

    let mut middle: Vec<(String, Style)> = Vec::new();
    if facts.error_count > 0 {
        let prefix = format!("  \u{26a0} {} ", facts.error_count);
        let prefix_w = style::display_width(&prefix);
        if prefix_w <= middle_budget {
            middle.push((prefix.clone(), style::tui::error()));
            if let Some(msg) = &facts.latest_error {
                let msg_budget = middle_budget - prefix_w;
                let shown = style::truncate_display_ellipsis(msg, msg_budget);
                if !shown.is_empty() {
                    middle.push((shown.into_owned(), Style::default().fg(Color::Red)));
                }
            }
        }
    } else if let Some(note) = facts.notice.as_deref().or(facts.tip) {
        let prefix = "  ".to_string();
        let prefix_w = style::display_width(&prefix);
        if prefix_w <= middle_budget {
            let msg_budget = middle_budget - prefix_w;
            let shown = style::truncate_display_ellipsis(note, msg_budget);
            if !shown.is_empty() {
                middle.push((prefix, Style::default()));
                middle.push((shown.into_owned(), style::tui::muted()));
            }
        }
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for (text, style) in left.into_iter().chain(middle) {
        used += style::display_width(&text);
        spans.push(Span::styled(text, style));
    }
    let pad = cols.saturating_sub(used + hints_w);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    // The cluster starts wherever the padding actually left it -- which is the
    // right edge only while the row had room. `regions` is built from this
    // same running column, so a click rect can never name a chord the row drew
    // somewhere else, or one it never drew at all.
    let mut column = used + pad;
    let mut regions: Vec<(Rect, HintId)> = Vec::new();
    for (i, (key, label)) in hints.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            column += 2;
        }
        let width = style::display_width(key) + 1 + style::display_width(label);
        let start = area.x.saturating_add(column.min(u16::MAX as usize) as u16);
        let drawn = area.right().saturating_sub(start).min(width as u16);
        if drawn > 0 && !area.is_empty() {
            regions.push((Rect::new(start, area.y, drawn, 1), hint_id(key)));
        }
        column += width;
        spans.push(Span::styled(key, style::tui::hint()));
        spans.push(Span::raw(format!(" {label}")));
    }
    (spans, regions)
}

pub fn render_header(f: &mut Frame, area: Rect, facts: &HeaderFacts) {
    if area.is_empty() {
        return;
    }
    let (spans, _) = header_layout(facts, area);
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect { height: 1, ..area },
    );
}

/// Dash refresh PR1: below the narrow-terminal floor the session column
/// hides and its own header shows every session as a tab instead --
/// ` {glyph} {name} {badge} `, the focused tab on the selected row's own
/// `Color::Indexed(236)` background (dash refresh PR1's own selection
/// tint, reused here rather than invented again). Same chip and hint
/// cluster as [`render_header`]; the error/notice middle segment is
/// dropped in this shape -- there is no room left for it once the tabs and
/// the hints are both on the row.
pub fn render_header_tabs(
    f: &mut Frame,
    area: Rect,
    facts: &HeaderFacts,
    rows: &[SidebarRow],
    tick: usize,
) {
    if area.is_empty() {
        return;
    }
    let cols = area.width as usize;
    let chip_text = " zirv ".to_string();
    let hints = header_hints(&facts.hints);
    let hints_w = hints
        .iter()
        .map(|(k, l)| k.len() + l.len() + 1)
        .sum::<usize>()
        + hints.len().saturating_sub(1) * 2;
    let gap_before_hints = 2usize;
    let chip_w = style::display_width(&chip_text);

    let mut tab_spans: Vec<Span<'static>> = Vec::new();
    let mut tabs_w = 0usize;
    let tabs_budget = cols.saturating_sub(chip_w + hints_w + gap_before_hints);
    for row in rows {
        let name = if row.role == "orch" {
            "orch".to_string()
        } else {
            style::truncate_display(&row.short, 4).into_owned()
        };
        let base = if row.focused {
            Style::default().bg(Color::Indexed(236))
        } else {
            Style::default()
        };
        let (badge_text, badge_style) = badge_for(row).unwrap_or_else(|| (String::new(), base));
        let mut piece = vec![
            (" ".to_string(), base),
            (
                glyph_char_for(glyph_for(row), tick).to_string(),
                glyph_style_for(glyph_for(row)).patch(base),
            ),
            (format!(" {name}"), style::tui::muted().patch(base)),
        ];
        if !badge_text.is_empty() {
            piece.push((format!(" {badge_text}"), badge_style.patch(base)));
        }
        piece.push((" ".to_string(), base));
        let piece_w: usize = piece.iter().map(|(t, _)| style::display_width(t)).sum();
        if tabs_w + piece_w > tabs_budget {
            break;
        }
        tabs_w += piece_w;
        tab_spans.extend(piece.into_iter().map(|(t, s)| Span::styled(t, s)));
    }

    let mut spans = vec![Span::styled(chip_text, style::tui::chip())];
    spans.extend(tab_spans);
    let used = chip_w + tabs_w;
    let pad = cols.saturating_sub(used + hints_w);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    for (i, (key, label)) in hints.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(key, style::tui::hint()));
        spans.push(Span::raw(format!(" {label}")));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect { height: 1, ..area },
    );
}

/// Dash refresh PR1: the sidebar's own title row (`DashLayout::sidebar_
/// title`) -- ` SESSIONS` left, the row count right-aligned. Replaces the
/// old summary line's `N live` plus rollup cluster; the per-glyph counts
/// still live in each group header's own rollup.
pub fn render_sidebar_title(f: &mut Frame, area: Rect, count: usize) {
    if area.is_empty() {
        return;
    }
    let text = aligned_rollup(" SESSIONS", &count.to_string(), area.width);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(text, style::tui::title()))),
        area,
    );
}

/// Pure: the pane header's own workflow segment text and style -- `▸ {kind}
/// › {step} {i}/{n}`, or `▸ {kind} › ✓ done` once `completed`; bold yellow
/// while `awaiting_approval`, muted otherwise. Shared with nothing else:
/// unlike the old footer segment this replaces, there is exactly one reader.
fn pane_header_workflow_span(fact: &SessionWorkflowFact) -> (String, Style) {
    let text = if fact.completed {
        format!("\u{25b8} {} \u{203a} \u{2713} done", fact.kind)
    } else {
        format!(
            "\u{25b8} {} \u{203a} {} {}/{}",
            fact.kind, fact.step, fact.index, fact.total
        )
    };
    let style = if fact.awaiting_approval {
        style::tui::warning().add_modifier(Modifier::BOLD)
    } else {
        style::tui::muted()
    };
    (text, style)
}

/// Pure: `word`'s first character upper-cased, the rest untouched -- the
/// pane header's own `{State word}` segment (`working` -> `Working`); the
/// sidebar's fact block keeps the plain lowercase word.
fn capitalize_first(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Everything [`render_pane_header`] needs for the focused pane's own header
/// row -- one row plus the `mid_rule` below it (see [`DashLayout`]).
pub struct PaneHeaderFacts {
    pub harness: String,
    /// `display_role`'s own short spelling (`orch`, `sub-orch`, `worker`).
    pub role: String,
    pub model: Option<String>,
    /// Already `~`-shortened by the caller (`dash::mod`'s existing cwd
    /// display convention) -- this renderer does no path math of its own.
    pub cwd: String,
    /// This pane's OWN bound workflow (never the repo-wide pointer).
    pub workflow: Option<SessionWorkflowFact>,
    pub glyph: Glyph,
    /// The fact block's own state word (`working`, `needs approval`, ...),
    /// capitalized here (`capitalize_first`) -- the sidebar keeps it plain.
    pub state_word: String,
    pub age_secs: Option<u64>,
}

/// Dash refresh PR1: the focused pane's own header row (`DashLayout::
/// pane_header`) -- left ` {harness} ▸ {role} · {model} · {cwd}`, right
/// `▸ {workflow} › {step} {i}/{n}  {glyph} {State word} {age}` (the workflow
/// segment absent without a bound workflow). Truncates the left side before
/// ever touching the right, since the right is the part naming what is
/// actually happening right now.
pub fn render_pane_header(f: &mut Frame, area: Rect, facts: &PaneHeaderFacts, tick: usize) {
    if area.is_empty() {
        return;
    }
    let cols = area.width as usize;
    let left: Vec<(String, Style)> = vec![
        (format!(" {}", facts.harness), style::tui::accent()),
        (" \u{25b8} ".to_string(), style::tui::muted()),
        (facts.role.clone(), style::tui::muted()),
        (
            format!(
                " \u{b7} {}",
                facts.model.as_deref().unwrap_or(style::PLACEHOLDER)
            ),
            style::tui::muted(),
        ),
        (format!(" \u{b7} {}", facts.cwd), style::tui::muted()),
    ];

    let mut right: Vec<(String, Style)> = Vec::new();
    if let Some(workflow) = &facts.workflow {
        let (text, style) = pane_header_workflow_span(workflow);
        right.push((format!("{text}  "), style));
    }
    let glyph_style = glyph_style_for(facts.glyph);
    right.push((
        format!("{} ", glyph_char_for(facts.glyph, tick)),
        glyph_style,
    ));
    right.push((
        format!("{} ", capitalize_first(&facts.state_word)),
        glyph_style,
    ));
    if let Some(age) = facts.age_secs {
        right.push((style::format_age(age), style::tui::muted()));
    }
    let right_w: usize = right.iter().map(|(t, _)| style::display_width(t)).sum();

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut remaining = cols.saturating_sub(right_w);
    let mut used = 0usize;
    for (text, style) in left {
        let text = style::truncate_display(&text, remaining).into_owned();
        let w = style::display_width(&text);
        remaining = remaining.saturating_sub(w);
        used += w;
        spans.push(Span::styled(text, style));
    }
    let pad = cols.saturating_sub(used + right_w);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    for (text, style) in right {
        spans.push(Span::styled(text, style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ---------------------------------------------------------------------
// Dash refresh PR1: the LIMITS block pinned to the bottom of the session
// column -- per harness with usage data, two rows per window (5h, wk).
// ---------------------------------------------------------------------

/// Converts a unix timestamp to this offset's own wall-clock time. A
/// `FixedOffset` covers every whole-minute UTC offset there is, so
/// `timestamp_opt` is always `Single` -- the fallback only guards a type
/// that cannot actually fail, never a real "ambiguous local time" case (that
/// belongs to `TimeZone`s with DST, which `FixedOffset` is not).
fn to_local(offset: FixedOffset, ts: u64) -> DateTime<FixedOffset> {
    offset
        .timestamp_opt(ts as i64, 0)
        .single()
        .unwrap_or_else(|| offset.timestamp_opt(0, 0).single().unwrap_or_default())
}

/// `HH:MM` at `offset` from a unix timestamp -- the LIMITS block's own
/// "exactly when it resets, on the operator's own clock" reading. Distinct
/// from `attention::utc_hhmm`, which is a different feature and stays UTC.
fn limits_hhmm(offset: FixedOffset, ts: u64) -> String {
    to_local(offset, ts).format("%H:%M").to_string()
}

/// Pure: the reset line's own `{time}` half -- today's bare `HH:MM`, `Ddd
/// HH:MM` within the next 7 days, else `D Mon`, all read at `offset` so a
/// reset that lands after local midnight (but before UTC midnight, or vice
/// versa) still reports the day the operator's own clock sees.
fn limits_reset_time(offset: FixedOffset, resets_at: u64, now: u64) -> String {
    let reset_local = to_local(offset, resets_at);
    let today = to_local(offset, now).date_naive();
    let reset_day = reset_local.date_naive();
    let hhmm = reset_local.format("%H:%M").to_string();
    let delta = (reset_day - today).num_days();
    if delta <= 0 {
        hhmm
    } else if delta < 7 {
        format!("{} {hhmm}", reset_local.format("%a"))
    } else {
        format!("{}", reset_local.format("%-d %b"))
    }
}

/// Pure: a duration in `resets_at - now` seconds, spelled `XhYYm` at an hour
/// or more and `Nm` (never `0m`) under one.
fn limits_countdown(resets_at: u64, now: u64) -> String {
    let remaining = resets_at.saturating_sub(now);
    if remaining < 3600 {
        format!("{}m", (remaining / 60).max(1))
    } else {
        format!("{}h{:02}m", remaining / 3600, (remaining % 3600) / 60)
    }
}

/// One harness window's own LIMITS row pair. Built by
/// [`limits_blocks_from_usage`]; `show_harness` is `true` only for the
/// first block of each harness (its 5h window), so the second row's own
/// harness column stays blank, matching the mock.
#[derive(Clone)]
pub struct LimitsBlock {
    pub harness: &'static str,
    pub show_harness: bool,
    pub window_label: &'static str,
    pub pct: f64,
    pub detail: WindowDetail,
}

/// Pure: every LIMITS block worth drawing, one per window that has usage
/// data at all -- a harness with no vendor reading for a window (`None` on
/// `HarnessUsage`) contributes nothing for it, and one with neither window
/// contributes nothing at all. `usages`' own order is kept, 5h before wk
/// within a harness.
pub fn limits_blocks_from_usage(usages: &[HarnessUsage]) -> Vec<LimitsBlock> {
    let mut blocks = Vec::new();
    for usage in usages {
        let mut show_harness = true;
        if let (Some(pct), Some(detail)) = (usage.five_hour, usage.five_hour_detail) {
            blocks.push(LimitsBlock {
                harness: usage.name,
                show_harness,
                window_label: "5h",
                pct,
                detail,
            });
            show_harness = false;
        }
        if let (Some(pct), Some(detail)) = (usage.seven_day, usage.seven_day_detail) {
            blocks.push(LimitsBlock {
                harness: usage.name,
                show_harness,
                window_label: "wk",
                pct,
                detail,
            });
        }
    }
    blocks
}

/// Pure: how many whole blocks (2 rows each, behind a 2-row title+rule)
/// fit in `rows` -- "session rows win vertical space: when rows would
/// collide, drop the limits block from the bottom a whole window at a
/// time" (never a half-drawn block). `0` when there is not even room for
/// the title and rule.
pub fn limits_blocks_fitting(block_count: usize, rows: u16) -> usize {
    if rows < 2 {
        return 0;
    }
    let available = ((rows - 2) / 2) as usize;
    block_count.min(available)
}

/// Pure: the total rows [`render_limits`] draws for `shown` blocks --
/// `0` when `shown` is `0` (no title/rule with nothing to show under it
/// either), else `2 + 2 * shown`.
pub fn limits_rows_for(shown: usize) -> u16 {
    if shown == 0 { 0 } else { 2 + 2 * shown as u16 }
}

/// Pure: the bar+percentage row's own tone -- red when the limit is hit,
/// magenta when overage is covered by credits, yellow at 80% or more,
/// else cyan. The unfilled bar cells are always dim, regardless.
fn limits_tone(pct: f64, detail: &WindowDetail) -> Style {
    if detail.limit_reached {
        style::tui::error()
    } else if detail.overage_covered {
        Style::default().fg(Color::Magenta)
    } else if pct >= 80.0 {
        style::tui::warning()
    } else {
        style::tui::accent()
    }
}

/// Pure: the reset line's own text and tone -- `back at HH:MM · Nm` (red)
/// when the limit is hit, `credits until HH:MM` (magenta) when overage is
/// covered, `reset at HH:MM · stale` (muted) when `resets_at` is already in
/// the past (a stale vendor reading, round 2 coordinator review), else
/// `resets {time}` with `· in {countdown}` appended only for a reset later
/// today (yellow at 80% or more, else muted).
fn limits_reset_line(
    offset: FixedOffset,
    pct: f64,
    detail: &WindowDetail,
    now: u64,
) -> (String, Style) {
    if detail.limit_reached {
        let countdown = limits_countdown(detail.resets_at, now);
        (
            format!(
                "back at {} \u{b7} {countdown}",
                limits_hhmm(offset, detail.resets_at)
            ),
            style::tui::error(),
        )
    } else if detail.overage_covered {
        (
            format!("credits until {}", limits_hhmm(offset, detail.resets_at)),
            Style::default().fg(Color::Magenta),
        )
    } else if detail.resets_at < now {
        // Round 2 coordinator review, CONFIRMED: a `resets_at` already in
        // the past is a stale vendor reading (the window rolled over but a
        // fresh one has not been reported yet), never a future time to
        // render as an ordinary `resets HH:MM` -- that reads as if the
        // reset is still ahead. Always muted, regardless of `pct`: this is
        // "the number is old", not a percentage-driven state. Kept to
        // `reset at HH:MM \u{b7} stale` (well under the 28-col sidebar
        // width, with its own 3-column indent) rather than the longer
        // `waiting for update` phrasing, which does not fit that row at
        // all -- `render_limits`'s own `truncate_display` is a last-resort
        // safety net, never the intended way this line gets short enough.
        (
            format!(
                "reset at {} \u{b7} stale",
                limits_hhmm(offset, detail.resets_at)
            ),
            style::tui::muted(),
        )
    } else {
        let time = limits_reset_time(offset, detail.resets_at, now);
        let style = if pct >= 80.0 {
            style::tui::warning()
        } else {
            style::tui::muted()
        };
        let today =
            to_local(offset, now).date_naive() == to_local(offset, detail.resets_at).date_naive();
        if today {
            let countdown = limits_countdown(detail.resets_at, now);
            (format!("resets {time} \u{b7} in {countdown}"), style)
        } else {
            (format!("resets {time}"), style)
        }
    }
}

/// Dash refresh PR1: draws the LIMITS block -- ` LIMITS` title, a rule,
/// then as many `blocks` as `limits_blocks_fitting` said would fit (the
/// caller is the one that drops blocks under height pressure; this draws
/// whatever it is handed and nothing more, even if more rows were left
/// over -- never guesses at the cut itself).
///
/// No `~` estimate marker anywhere: the only estimator source
/// (`window::estimate_windows`, via `pace::current_windows`) sums
/// transcripts, which is exactly the scan/poll the dashboard's own
/// throttled tick must never do (see `WindowDetail`'s own doc comment) --
/// so every value shown here is vendor-reported, never zirv's own guess,
/// and the `~` prefix the design calls for never has anything to attach to
/// in this build.
pub fn render_limits(
    f: &mut Frame,
    area: Rect,
    blocks: &[LimitsBlock],
    now: u64,
    offset: FixedOffset,
) {
    if area.is_empty() || blocks.is_empty() {
        return;
    }
    let cols = area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(" LIMITS", style::tui::title())));
    // Confined to the sidebar's own width -- the vertical divider against
    // the grid is a separate draw (`render_sidebar_divider`) spanning the
    // whole body height already, so this rule needs no junction of its own.
    lines.push(Line::from(Span::styled(
        "\u{2500}".repeat(cols),
        style::tui::muted(),
    )));
    for block in blocks {
        let tone = limits_tone(block.pct, &block.detail);
        let harness = if block.show_harness {
            block.harness.to_string()
        } else {
            String::new()
        };
        let bar_filled = ((block.pct / 100.0) * 6.0).round().clamp(0.0, 6.0) as usize;
        let bar_filled_text = "\u{25b0}".repeat(bar_filled);
        let bar_empty_text = "\u{25b1}".repeat(6 - bar_filled);
        let pct_text = format!("{:>3.0}%", block.pct.round());
        let bar_row = Line::from(vec![
            Span::styled(
                style::truncate_display(&format!(" {harness:<6} {:>2} ", block.window_label), cols)
                    .into_owned(),
                style::tui::muted(),
            ),
            Span::styled(bar_filled_text, tone),
            Span::styled(bar_empty_text, style::tui::muted()),
            Span::styled(format!(" {pct_text}"), tone),
        ]);
        lines.push(bar_row);
        let (reset_text, reset_style) = limits_reset_line(offset, block.pct, &block.detail, now);
        lines.push(Line::from(Span::styled(
            style::truncate_display(&format!("   {reset_text}"), cols).into_owned(),
            reset_style,
        )));
    }
    let height = lines.len().min(area.height as usize) as u16;
    f.render_widget(Paragraph::new(Text::from(lines)), Rect { height, ..area });
}

/// Issue #209/v3 §D: the focused session's active `zirv workflow` position,
/// as much as the footer needs -- the rest of `workflow::ActiveWorkflowSummary`
/// (attempts, artifacts, review evidence, ...) has no footer segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterWorkflow {
    /// This session's repo has no active workflow at all.
    None,
    Active {
        kind: String,
        step: String,
        /// The current step is gated on the operator's own approval
        /// (`WorkflowStatus::AwaitingApproval`) -- Q4: escalates to
        /// yellow-bold, the same weight unread mail already gets.
        gated: bool,
    },
}

/// The footer's own facts for the focused pane (Q1: focused-only, never one
/// row per live harness) -- issue #209/v3 §D's new signal row, one below
/// the grid. `None` when nothing is focused at all (an empty dashboard);
/// nothing draws.
pub enum FooterFacts {
    None,
    Alive(FooterAliveFacts),
    Dead(FooterDeadFacts),
}

/// The healthy/attention footer shapes (mock §04's first two examples) --
/// they differ only in *values*, not in which fields exist.
///
/// Dash refresh PR1 dropped `harness`, `usage_five_hour`/`usage_seven_day`
/// and `workflow` from this struct: the harness/model/workflow facts moved
/// to the pane header (`ui::PaneHeaderFacts`), and PR2 replaces the usage
/// pair with the next-action forecast track. The footer keeps only the rot
/// verdict, mail and supervision now.
pub struct FooterAliveFacts {
    /// `None` when no cached score exists yet for this session -- renders
    /// the same `✻ –` unknown placeholder the wrap bar's own `BarState`
    /// uses for the identical case.
    pub score: Option<u32>,
    /// Total unread mail (broadcast + direct) for this session. The mock's
    /// footer shows one unlabeled number, unlike the wrap bar's own
    /// broadcast/direct `+`-split -- `0` renders the dim placeholder.
    pub unread_mail: usize,
    /// `Pane::reachable()` for the focused pane (issue #209/v3 codex review
    /// finding 5): whether its own turn-signal socket bound successfully at
    /// spawn time. `false` is the same "degrades to unsupervised" case
    /// `Pane::spawn`'s own doc comment describes for a failed bind -- a
    /// dashboard pane that cannot act on a wake-up is still legitimate and
    /// visible, but the footer must say so rather than assume every alive
    /// pane is fine. Not the same signal as `wrap`'s own in-process
    /// `chrome::BarState::degraded` (there is no dash-observable analogue
    /// of THAT one -- a supervising *loop* going bad mid-session, as
    /// opposed to never having bound in the first place), which stays out
    /// of this issue's scope (§E).
    pub supervised: bool,
    /// Issue #310: this pane's stall latch is currently armed
    /// (`sessions::stall_marker`, via `DiskFacts::stalled`) -- overrides the
    /// supervision segment with a `stalled` badge instead of `supervised`/
    /// `unsupervised` while it holds, and reverts the instant the latch
    /// clears (observed progress, or the session ended). Takes priority over
    /// `supervised`: a session can be both reachable and stalled at once,
    /// and the operator needs to see the more urgent fact.
    pub stalled: bool,
}

/// The dead-pane-focused footer shape (mock §04's third example): a
/// different message entirely, not just different values -- there is no
/// verdict, usage or mail segment for a session that has already exited.
pub struct FooterDeadFacts {
    pub harness: String,
    pub exited_age_secs: Option<u64>,
    pub workflow: FooterWorkflow,
}

/// One footer segment, already styled per-piece (a segment is sometimes
/// more than one span -- the verdict's glyph+word and its dim score number,
/// for instance) but not yet joined to its neighbours. Named to keep every
/// footer-assembly signature below readable, the same reason `chrome.rs`
/// names its own `Segments` alias.
type FooterSeg = Vec<(String, Style)>;

/// Pure: `workflow`'s own footer text, in its full and width-compressed
/// forms (§D's drop order compresses the workflow segment before dropping
/// it outright) -- `(full, compressed)`, both already styled. `None` has
/// only the one dim `▸ –` form; a gated step's compressed form gains the
/// `!` suffix mentioned nowhere but the mock's own 44-column example.
fn footer_workflow_spans(workflow: &FooterWorkflow) -> (FooterSeg, FooterSeg) {
    match workflow {
        FooterWorkflow::None => {
            let dim = vec![(
                format!("\u{25b8} {}", style::PLACEHOLDER),
                style::tui::muted(),
            )];
            (dim.clone(), dim)
        }
        FooterWorkflow::Active { kind, step, gated } => {
            if *gated {
                let style = style::tui::warning().add_modifier(Modifier::BOLD);
                let full = vec![(format!("\u{25b8} {step} awaits approval"), style)];
                let compressed = vec![(format!("\u{25b8} {step}!"), style)];
                (full, compressed)
            } else {
                let style = style::tui::muted();
                let full = vec![(format!("\u{25b8} {kind} \u{b7} {step}"), style)];
                let compressed = vec![(format!("\u{25b8} {step}"), style)];
                (full, compressed)
            }
        }
    }
}

/// Three plain spaces between top-level footer segments, mirroring
/// `chrome::BAR_SEGMENT_GAP` -- the footer reuses the wrap bar's own
/// grammar verbatim (minus the chip, per the mock's own note).
const FOOTER_SEGMENT_GAP: &str = "   ";

/// Pure: the alive-pane footer's spans, width-budgeted to `cols`. Dash
/// refresh PR1 dropped the harness/usage/workflow segments entirely (the
/// first two moved to the pane header; PR2 replaces usage with the
/// next-action forecast) -- what is left is the rot verdict (`✻ NN {band}`),
/// mail, and supervision, which shrinks to nothing at all while the pane is
/// healthy and reachable (`Remove the healthy ● supervised segment`) and
/// only ever shows `▲ unsupervised` or `◆ stalled`. Only the verdict's own
/// score number is ever dropped, under real width pressure -- the word,
/// mail and supervision are never dropped.
fn footer_alive_spans(
    facts: &FooterAliveFacts,
    advise_at: u32,
    compact_at: u32,
    cols: u16,
) -> Vec<Span<'static>> {
    let cols = cols as usize;

    let (verdict_full, verdict_reduced): (FooterSeg, FooterSeg) = match facts.score {
        Some(score) => {
            let band = rot_band_for(score, advise_at, compact_at);
            let word = match band {
                RotBand::Fresh => "fresh",
                RotBand::Warming => "warming",
                RotBand::Rotting => "rotting",
            };
            let style = footer_rot_style(band);
            let full = vec![(format!("{ROT_GLYPH} {score} {word}"), style)];
            let reduced = vec![(format!("{ROT_GLYPH} {word}"), style)];
            (full, reduced)
        }
        None => {
            let unknown = vec![(
                format!("{ROT_GLYPH} {}", style::PLACEHOLDER),
                style::tui::muted(),
            )];
            (unknown.clone(), unknown)
        }
    };

    let mail: FooterSeg = if facts.unread_mail == 0 {
        vec![(
            format!("\u{2709} {}", style::PLACEHOLDER),
            style::tui::muted(),
        )]
    } else {
        let style = style::tui::warning().add_modifier(Modifier::BOLD);
        vec![(format!("\u{2709} {}", facts.unread_mail), style)]
    };

    // Issue #310: a stalled latch takes priority over the ordinary
    // unsupervised segment -- see `FooterAliveFacts::stalled`'s own doc
    // comment. Healthy and reachable renders NOTHING at all now (dash
    // refresh PR1): only a problem is worth a segment.
    let supervision: FooterSeg = if facts.stalled {
        vec![(
            "\u{25c6} stalled".to_string(),
            style::tui::warning().add_modifier(Modifier::BOLD),
        )]
    } else if !facts.supervised {
        vec![("\u{25b2} unsupervised".to_string(), style::tui::error())]
    } else {
        Vec::new()
    };

    let tiers = [
        join_footer_segments(&[&verdict_full, &mail, &supervision]),
        join_footer_segments(&[&verdict_reduced, &mail, &supervision]),
    ];
    choose_footer_tier(&tiers, cols)
}

/// Pure: joins `segments` with [`FOOTER_SEGMENT_GAP`] between each one
/// present, in order -- an empty segment (dash refresh PR1's healthy-and-
/// reachable supervision, which is nothing at all) contributes neither text
/// nor a gap of its own.
fn join_footer_segments(segments: &[&FooterSeg]) -> FooterSeg {
    let mut out = FooterSeg::new();
    for seg in segments.iter().filter(|seg| !seg.is_empty()) {
        if !out.is_empty() {
            out.push((FOOTER_SEGMENT_GAP.to_string(), Style::default()));
        }
        out.extend(seg.iter().cloned());
    }
    out
}

/// Pure: total display width of a joined footer segment.
fn footer_seg_width(pieces: &[(String, Style)]) -> usize {
    pieces.iter().map(|(t, _)| style::display_width(t)).sum()
}

/// Pure: the first `tiers` entry (most generous first) that fits `cols`, or
/// -- if even the narrowest tier overflows -- that narrowest tier hard-
/// truncated to `cols` with no ellipsis, exactly `chrome::status_bar`'s own
/// last resort. `Paragraph` would clip silently past `area`'s own width
/// regardless; this keeps the pure function itself honest about `cols` for
/// its own tests.
fn choose_footer_tier(tiers: &[FooterSeg], cols: usize) -> Vec<Span<'static>> {
    let chosen = match tiers
        .iter()
        .find(|tier| footer_seg_width(tier) <= cols)
        .or_else(|| tiers.last())
    {
        Some(tier) => tier,
        // An empty tier list renders as an empty row rather than panicking
        // the render loop; every current caller passes a non-empty literal.
        None => return Vec::new(),
    };

    if footer_seg_width(chosen) <= cols {
        return chosen
            .iter()
            .map(|(text, style)| Span::styled(text.clone(), *style))
            .collect();
    }
    let plain: String = chosen.iter().map(|(t, _)| t.as_str()).collect();
    vec![Span::raw(
        style::truncate_display(&plain, cols).into_owned(),
    )]
}

/// Pure: the dead-pane-focused footer's spans (mock §04's third example) --
/// a different message entirely, not the alive shape with blanks. The
/// exited notice and the restore hint are never dropped; `harness` and the
/// workflow segment are the only droppable pieces, in that order (the
/// restore hint is the one actionable thing this state exists to tell the
/// operator, so it survives longest).
fn footer_dead_spans(facts: &FooterDeadFacts, cols: u16) -> Vec<Span<'static>> {
    let cols = cols as usize;

    let harness: FooterSeg = vec![(facts.harness.clone(), Style::default())];

    let exited: FooterSeg = {
        let age = facts
            .exited_age_secs
            .map(style::format_age)
            .unwrap_or_else(|| style::PLACEHOLDER.to_string());
        vec![
            ("\u{2717} exited".to_string(), style::tui::error()),
            (format!(" {age} ago"), style::tui::muted()),
        ]
    };

    let (workflow_full, _) = footer_workflow_spans(&facts.workflow);

    let restore_hint: FooterSeg = vec![
        ("\u{21ba} ".to_string(), style::tui::accent()),
        (
            "^A r".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        (" restore".to_string(), style::tui::hint()),
    ];

    let tiers = [
        join_footer_segments(&[&harness, &exited, &workflow_full, &restore_hint]),
        join_footer_segments(&[&harness, &exited, &restore_hint]),
        join_footer_segments(&[&exited, &restore_hint]),
    ];
    choose_footer_tier(&tiers, cols)
}

/// Issue #209/v3 §D: the footer signal row, describing whichever pane is
/// focused. `advise_at`/`compact_at` are `rot::ScoreConfig`'s own
/// thresholds, threaded through exactly as [`render_sidebar`] takes them.
pub fn render_footer(
    f: &mut Frame,
    area: Rect,
    facts: &FooterFacts,
    advise_at: u32,
    compact_at: u32,
) {
    if area.is_empty() {
        return;
    }
    let spans = match facts {
        FooterFacts::None => return,
        FooterFacts::Alive(alive) => footer_alive_spans(alive, advise_at, compact_at, area.width),
        FooterFacts::Dead(dead) => footer_dead_spans(dead, area.width),
    };
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect { height: 1, ..area },
    );
}

/// Dash refresh PR1: below the narrow-terminal floor the footer shows the
/// FOCUSED pane's own harness usage instead of the ordinary verdict/mail/
/// supervision row -- `5h {pct}% · resets {time}`, since the LIMITS block
/// that would otherwise carry this is gone along with the rest of the
/// sidebar. `None` (nothing focused, or that harness has no 5h reading at
/// all yet) draws nothing, same as [`render_footer`]'s own `FooterFacts::
/// None`.
pub fn render_footer_narrow_usage(
    f: &mut Frame,
    area: Rect,
    usage: Option<&HarnessUsage>,
    now: u64,
    offset: FixedOffset,
) {
    if area.is_empty() {
        return;
    }
    let Some(usage) = usage else {
        return;
    };
    let Some(pct) = usage.five_hour else {
        return;
    };
    let text = match usage.five_hour_detail {
        Some(detail) => format!(
            "5h {pct:.0}% \u{b7} resets {}",
            limits_reset_time(offset, detail.resets_at, now)
        ),
        None => format!("5h {pct:.0}%"),
    };
    f.render_widget(
        Paragraph::new(Span::styled(text, style::tui::muted())),
        Rect { height: 1, ..area },
    );
}

/// Pure: the roster viewport's first drawn entry, given how many entries the
/// tree has, how many fit, which one must be visible, and where the viewport
/// currently sits.
///
/// The sidebar used to draw from entry `0` unconditionally, so once the
/// combined row count (panes plus view-only registry rows) outgrew the
/// sidebar's height the cursor could walk onto a row that was never drawn --
/// an invisible selection, and with arrow navigation now moving focus too, a
/// keyboard that moved to a pane the operator could not see listed.
///
/// Issue #354 gives the roster its own viewport: the wheel scrolls `offset`
/// without touching the selection, so the two genuinely drift apart, and
/// every keyboard navigation re-reveals the selection through here. The
/// offset moves the *minimum* distance that brings `index` back inside the
/// window (so a scrolled-then-navigated roster does not jump), and is always
/// clamped so the last screenful is as far as it can scroll.
///
/// Every subtraction is guarded: `dash.sidebar_cols` and a two-row terminal
/// both genuinely reach here, and the release profile is `panic = "abort"`, so
/// an underflow would take the operator's terminal with it.
pub(crate) fn reveal_offset(total: usize, visible: usize, index: usize, offset: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    let max = total - visible;
    if index < offset {
        index.min(max)
    } else if index >= offset.saturating_add(visible) {
        index.saturating_add(1).saturating_sub(visible).min(max)
    } else {
        offset.min(max)
    }
}

/// The glyph character (never coloured on its own -- see [`glyph_style_for`])
/// a sidebar row's state renders as. A working pane's own glyph advances one
/// [`style::tui::SPINNER_FRAMES`] frame per render tick.
fn glyph_char_for(glyph: Glyph, tick: usize) -> &'static str {
    match glyph {
        Glyph::Working => style::tui::SPINNER_FRAMES[tick % style::tui::SPINNER_FRAMES.len()],
        Glyph::Idle => "\u{25cf}",
        Glyph::NeedsAction => "\u{25b2}",
        Glyph::DoneUnread => "\u{25c6}",
        Glyph::Failed => "\u{2717}",
        Glyph::Unknown => "\u{00b7}",
    }
}

/// The glyph's own colour: cyan for a working pane's spinner, green for a
/// live-but-idle one, yellow for one waiting on the operator, magenta for one
/// that finished unnoticed, red for exited/dead, and no colour at all (default
/// monochrome, matching every view-only row before this phase) for a row
/// whose state cannot be observed from here. Colour is never the only carrier
/// -- every glyph above is its own shape too.
fn glyph_style_for(glyph: Glyph) -> Style {
    match glyph {
        Glyph::Working => style::tui::accent(),
        Glyph::Idle => style::tui::ok(),
        Glyph::NeedsAction => style::tui::warning(),
        Glyph::DoneUnread => style::tui::unread(),
        Glyph::Failed => style::tui::error(),
        Glyph::Unknown => Style::default(),
    }
}

/// The colour band a rot score falls into, mirroring the same three-band
/// collapse `chrome::verdict_paint` draws for the wrap status bar (healthy
/// vs. advise vs. compact-or-restart) but keyed on the score alone: the
/// dashboard does not track each pane's live context-token count the way
/// `wrap` does, so there is no token-gate escalation (`rot::verdict_for`) to
/// fold in here -- see [`rot_band_for`]'s own doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotBand {
    Fresh,
    Warming,
    Rotting,
}

/// Pure: which [`RotBand`] a raw rot `score` falls into, given the same
/// `advise_at`/`compact_at` thresholds `rot::ScoreConfig` already carries
/// (passed through rather than re-read, so this stays a plain function of
/// its arguments). `restart_at` collapses into `Rotting` alongside
/// `compact_at`, matching `chrome::verdict_paint`'s own three-colour band.
pub fn rot_band_for(score: u32, advise_at: u32, compact_at: u32) -> RotBand {
    if score >= compact_at {
        RotBand::Rotting
    } else if score >= advise_at {
        RotBand::Warming
    } else {
        RotBand::Fresh
    }
}

/// The sidebar's own rot-glyph style: warming is yellow, rotting is
/// red-bold, and -- per the approved mock (§03) -- fresh gets no colour of
/// its own at all, inheriting whatever tone the row it sits in already
/// carries. Never call this for a selected row: see `render_sidebar`'s own
/// selected-row handling, which drops every glyph's own colour outright
/// (§B) rather than composing it with `Modifier::REVERSED`.
fn sidebar_rot_style(band: RotBand) -> Style {
    match band {
        RotBand::Fresh => Style::default(),
        RotBand::Warming => style::tui::warning(),
        RotBand::Rotting => style::tui::error(),
    }
}

/// The footer's own rot-verdict style (§D): unlike the sidebar, fresh gets
/// an explicit green here -- the footer has no surrounding row tone to
/// inherit the way a sidebar row does, so it colours all three bands
/// outright, exactly `chrome::verdict_paint` does for the wrap bar's own
/// verdict segment.
fn footer_rot_style(band: RotBand) -> Style {
    match band {
        RotBand::Fresh => style::tui::ok(),
        RotBand::Warming => style::tui::warning(),
        RotBand::Rotting => style::tui::error(),
    }
}

/// The rot glyph itself -- the same star `chrome::status_bar` already draws
/// for the wrap bar's own verdict segment, so a session reads identically
/// whether it is shown from inside (wrap) or from the dash.
const ROT_GLYPH: &str = "\u{273b}";

/// Pure: a sidebar row's rot column text, colourless -- [`render_sidebar`]
/// paints it. `None` (dead pane, or no cached score at all) is always the
/// shared placeholder, never a fabricated reading: a dead pane's last cached
/// score is stale, not a live verdict, so it renders the same as "unknown"
/// regardless of what happens to still be cached for it.
fn rot_text(row: &SidebarRow) -> String {
    if row.state == RowState::Dead {
        return format!("{ROT_GLYPH} {}", style::PLACEHOLDER);
    }
    match row.score {
        Some(score) => format!("{ROT_GLYPH}{score}"),
        None => format!("{ROT_GLYPH} {}", style::PLACEHOLDER),
    }
}

/// Pure: `text` fitted to exactly `width` display columns -- truncated with
/// `truncate_display` when it is too wide, space-padded (left or right) when
/// it is too narrow. Every fixed column of the sidebar row contract goes
/// through here, so a row's total width is the sum of its column widths no
/// matter what a session is called or which model it runs.
fn column(text: &str, width: usize, right: bool) -> String {
    let text = style::truncate_display(text, width);
    let pad = " ".repeat(width.saturating_sub(style::display_width(&text)));
    if right {
        format!("{pad}{text}")
    } else {
        format!("{text}{pad}")
    }
}

/// The dash refresh (PR1) 28-column row contract's fixed prefix, in display
/// columns: `tree(1) glyph(1) sp name(N) sp harness(6) sp rot(3) sp badge(2)
/// sp` = 18 + N. `name` takes whatever is left (10 at the default
/// `sidebar_cols` of 28), so a row always fills `cols` exactly and a
/// selected row's own background reaches the divider.
const SIDEBAR_FIXED_COLS: usize = 18;

/// Pure: the badge column's own text and style, highest priority first -- a
/// workflow gate awaiting approval (`⚑ `) outranks unread mail (`✉N`, `✉+`
/// above 9, matching the wrap bar's own convention for an unbounded count).
/// `None` (rendered as two blank columns) when neither applies. PR2 adds the
/// lifecycle badges (`⟳ ⇢ ⤓ ⏸`) above mail in this same priority order.
fn badge_for(row: &SidebarRow) -> Option<(String, Style)> {
    if row
        .status
        .as_ref()
        .is_some_and(|s| s.attention == Attention::WorkflowGate)
    {
        return Some((
            "\u{2691} ".to_string(),
            style::tui::warning().add_modifier(Modifier::BOLD),
        ));
    }
    if row.unread_mail > 0 {
        let count = if row.unread_mail > 9 {
            "+".to_string()
        } else {
            row.unread_mail.to_string()
        };
        return Some((
            format!("\u{2709}{count}"),
            style::tui::warning().add_modifier(Modifier::BOLD),
        ));
    }
    None
}

/// Pure: one sidebar row's styled spans under the 28-column contract above.
/// Colours follow the dash refresh's replacement for #209 §B: a selected
/// row gets a subtle `Color::Indexed(236)` background rather than REVERSED,
/// so every glyph (state, rot and badge alike) keeps its own colour under
/// the tint; keyboard focus adds BOLD; a view-only (unattached) row is DIM.
fn sidebar_row_parts(
    row: &SidebarRow,
    tick: usize,
    cols: u16,
    advise_at: u32,
    compact_at: u32,
) -> Vec<Span<'static>> {
    let mut base = Style::default();
    if !row.attached {
        base = base.add_modifier(Modifier::DIM);
    }
    if row.focused {
        base = base.add_modifier(Modifier::BOLD);
    }
    if row.selected {
        base = base.bg(Color::Indexed(236));
    }
    let muted = style::tui::muted().patch(base);
    let glyph = glyph_style_for(glyph_for(row)).patch(base);
    let rot = row
        .score
        .filter(|_| row.state != RowState::Dead)
        .map(|score| sidebar_rot_style(rot_band_for(score, advise_at, compact_at)))
        .unwrap_or_else(style::tui::muted)
        .patch(base);
    // `name` is `orch` for the orchestrator (`display_role` already shortens
    // its role to that exact string), otherwise the 8-char short id.
    let name = if row.role == "orch" {
        "orch".to_string()
    } else {
        row.short.clone()
    };
    let name_width = (cols as usize).saturating_sub(SIDEBAR_FIXED_COLS);
    let (badge_text, badge_style) = badge_for(row).unwrap_or_else(|| (String::new(), muted));
    let parts = [
        (row.tree.prefix().to_string(), muted),
        (glyph_char_for(glyph_for(row), tick).to_string(), glyph),
        (format!(" {} ", column(&name, name_width, false)), base),
        (column(&row.harness, 6, false), muted),
        (format!(" {} ", column(&rot_text(row), 3, false)), rot),
        (column(&badge_text, 2, false), badge_style.patch(base)),
        (" ".to_string(), base),
    ];
    let mut remaining = cols as usize;
    parts
        .into_iter()
        .map(|(text, tone)| {
            let text = style::truncate_display(&text, remaining).into_owned();
            remaining = remaining.saturating_sub(style::display_width(&text));
            Span::styled(text, tone)
        })
        .collect()
}

#[cfg(test)]
fn sidebar_row_text(row: &SidebarRow, tick: usize, cols: u16) -> String {
    sidebar_row_parts(row, tick, cols, 40, 70)
        .into_iter()
        .map(|s| s.content.into_owned())
        .collect()
}

/// Issue #209/v3 §A4: the sidebar's own flat divider column -- what used to
/// be the right edge of its full rounded `Block::bordered()` -- against the
/// grid. Dim, matching `--t-line` in the approved mock; a full box is now
/// reserved for the banner and overlays only.
pub fn render_sidebar_divider(f: &mut Frame, area: Rect) {
    if area.is_empty() {
        return;
    }
    let line = "\u{2502}".repeat(area.height as usize);
    let lines: Vec<Line> = line
        .chars()
        .map(|c| Line::from(Span::styled(c.to_string(), style::tui::muted())))
        .collect();
    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Test-only: draws just the session-row band of the sidebar -- no summary
/// line, no group headers, no disclosure. The production path is
/// [`roster_frame`] + [`render_roster`]; this shares its exact row spans
/// (`sidebar_row_parts`) and viewport rule ([`reveal_offset`]), so the
/// per-row styling assertions below stay readable without every one of them
/// having to account for the roster's own chrome rows.
///
/// `advise_at`/`compact_at` are `rot::ScoreConfig`'s own thresholds, passed
/// through by the caller (`dash::mod` already has the loaded `CtxConfig`)
/// rather than duplicated here as defaults -- an operator's configured
/// thresholds drive the sidebar's rot colour exactly as they drive
/// `rot::verdict_for`'s own bands.
#[cfg(test)]
pub fn render_sidebar(
    f: &mut Frame,
    area: Rect,
    rows: &[SidebarRow],
    tick: usize,
    advise_at: u32,
    compact_at: u32,
) {
    let selected = rows.iter().position(|r| r.selected).unwrap_or(0);
    let offset = reveal_offset(rows.len(), area.height as usize, selected, 0);
    for (y, row) in rows
        .iter()
        .skip(offset)
        .take(area.height as usize)
        .enumerate()
    {
        f.render_widget(
            Paragraph::new(Line::from(sidebar_row_parts(
                row, tick, area.width, advise_at, compact_at,
            ))),
            Rect {
                y: area.y + y as u16,
                height: 1,
                ..area
            },
        );
    }
}

/// One drawn roster: its lines, and the pointer geometry of exactly those
/// lines. Produced together by [`roster_frame`] so a click can never address
/// a row that height pressure or a collapsed group kept off the screen.
pub struct RosterFrame {
    lines: Vec<Line<'static>>,
    /// Screen rects, in draw order, each covering one entry *and* whatever
    /// disclosure lines were drawn under it.
    pub hits: Vec<(Rect, Hit)>,
    /// Every entry the tree has this frame, in order, drawn or not: the
    /// keyboard's navigation order and the viewport's own index space.
    pub row_ids: Vec<Hit>,
}

/// Pure: the glyph counts a summary line or group header shows on its right,
/// one term per glyph that has any members, in a fixed order so the cluster
/// does not reshuffle as sessions change state.
///
/// Two tiers, exactly as the approved design's two reference frames show them:
/// as soon as anything among its members needs attention the cluster reports
/// only that -- `▲1  ✗1  ◆1` (frame A) -- and the ordinary working/idle counts
/// are suppressed; with nothing waiting it reports the quiet set instead --
/// `⠋3  ●1` (frame B). The counts are the part an operator scans for, so the
/// scarce right-hand columns go to the states that mean something is owed.
fn rollup(rows: &[&SidebarRow], tick: usize) -> String {
    let render = |set: &[Glyph]| -> Vec<String> {
        set.iter()
            .filter_map(|glyph| {
                let count = rows.iter().filter(|r| glyph_for(r) == *glyph).count();
                (count > 0).then(|| format!("{}{count}", glyph_char_for(*glyph, tick)))
            })
            .collect()
    };
    let attention = render(&ATTENTION_GLYPHS);
    if attention.is_empty() {
        render(&QUIET_GLYPHS).join("  ")
    } else {
        attention.join("  ")
    }
}

/// Pure: `left` at the left edge, `right` at the right, padded to exactly
/// `width`. The rollup wins the space when the two cannot both fit -- the
/// counts are the part an operator scans for.
fn aligned_rollup(left: &str, right: &str, width: u16) -> String {
    let right = style::truncate_display(right, width as usize);
    let room = (width as usize).saturating_sub(style::display_width(&right));
    format!("{}{right}", column(left, room, false))
}

/// The roster's own view state, separate from the session rows themselves:
/// which groups are folded shut, where the wheel left the viewport, which
/// non-session entry (if any) the cursor is parked on, and the tick and rot
/// bands every row needs to draw itself.
pub struct RosterView<'a> {
    /// Work-group ids the operator has collapsed.
    pub collapsed: &'a HashSet<String>,
    /// The summary line or a group header, when the cursor is on one of them
    /// rather than on a session row. A chrome selection suppresses the
    /// session rows' own REVERSED band, so the roster only ever shows one
    /// cursor.
    pub chrome_selection: Option<&'a Hit>,
    /// First tree entry drawn; the summary line above it never scrolls.
    pub offset: usize,
    /// Render tick, for the working spinner.
    pub tick: usize,
    /// `(advise_at, compact_at)` -- the operator's own rot thresholds.
    pub bands: (u32, u32),
}

/// Pure: lays the whole roster out -- group tree, session rows and the
/// selected row's 2-line fact block -- and returns the lines together with
/// the pointer geometry of exactly those lines. `area` is the session-rows
/// area alone (`DashLayout::sidebar`): the title row lives in its own rect
/// now (`DashLayout::sidebar_title`, drawn by `render_sidebar_title`), not
/// as this function's own row 0.
///
/// Session order is spawn order, never re-sorted; a work group takes one
/// header at the position of its first member, with the lead (its
/// sub-orchestrator, else the first member) as the first child. The two
/// outputs are produced in one pass on purpose: geometry derived separately
/// from the drawing is how a click ends up addressing a row that a collapsed
/// group or height pressure kept off the screen.
///
/// Under height pressure fact-block lines drop before any session row, and
/// group headers never drop at all.
pub fn roster_frame(area: Rect, rows: &[SidebarRow], view: &RosterView<'_>) -> RosterFrame {
    let RosterView {
        collapsed,
        chrome_selection,
        offset,
        tick,
        bands,
    } = *view;
    let mut entries: Vec<(Hit, Line<'static>, Vec<Line<'static>>)> = Vec::new();
    let mut seen = HashSet::new();
    for row in rows {
        if let Some(group) = &row.group {
            if !seen.insert(group.id.clone()) {
                continue;
            }
            let mut members: Vec<_> = rows
                .iter()
                .filter(|r| r.group.as_ref().is_some_and(|g| g.id == group.id))
                .collect();
            if let Some(lead) = members.iter().position(|r| r.short == group.lead_short) {
                let lead = members.remove(lead);
                members.insert(0, lead);
            }
            let closed = collapsed.contains(&group.id);
            let id = Hit::GroupToggle(group.id.clone());
            // Dash refresh PR1: `▾ {scope}` alone -- the lead/worker-count
            // text the 44-column contract used to spell out is gone; the
            // rollup cluster (right-aligned to the row's own badge column)
            // already says how many members and what state they are in.
            let text = aligned_rollup(
                &format!(
                    "{} {}",
                    if closed { "\u{25b8}" } else { "\u{25be}" },
                    group.scope
                ),
                &rollup(&members, tick),
                area.width,
            );
            let tone = if chrome_selection == Some(&id) {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                style::tui::muted()
            };
            entries.push((id, Line::from(Span::styled(text, tone)), Vec::new()));
            if closed {
                continue;
            }
            for (i, row) in members.iter().enumerate() {
                let mut row = (*row).clone();
                row.tree = if i + 1 == members.len() {
                    TreePos::LastChild
                } else {
                    TreePos::Child
                };
                entries.push(roster_entry(
                    &row,
                    area.width,
                    tick,
                    bands,
                    chrome_selection.is_none(),
                ));
            }
        } else {
            entries.push(roster_entry(
                row,
                area.width,
                tick,
                bands,
                chrome_selection.is_none(),
            ));
        }
    }
    let row_ids = entries.iter().map(|(id, _, _)| id.clone()).collect();
    let mut result = RosterFrame {
        lines: Vec::new(),
        hits: Vec::new(),
        row_ids,
    };
    if area.is_empty() {
        return result;
    }
    // Dash refresh PR1: the old summary line (`N live` plus its own rollup)
    // is gone -- `DashLayout::sidebar_title` now carries the row count
    // (`render_sidebar_title`, drawn from a separate rect above `area`, one
    // row up), and `Hit::SidebarSummary`'s own click target is added
    // straight onto that rect by `frame_snapshot` rather than through this
    // frame's own `hits`. `area` is therefore ALL session rows now -- no
    // row reserved for a title this function no longer draws.
    let capacity = area.height as usize;
    let offset = offset.min(entries.len().saturating_sub(capacity));
    let mut detail_room = capacity.saturating_sub(entries.len());
    for (id, line, disclosure) in entries.into_iter().skip(offset).take(capacity) {
        if result.lines.len() >= area.height as usize {
            break;
        }
        let y = area.y + result.lines.len() as u16;
        result.lines.push(line);
        let detail_count = disclosure.len().min(detail_room);
        detail_room -= detail_count;
        result
            .lines
            .extend(disclosure.into_iter().take(detail_count));
        result.hits.push((
            Rect {
                y,
                height: 1 + detail_count as u16,
                ..area
            },
            id,
        ));
    }
    result
}

/// Pure: one session row as a roster entry -- its hit id, its own line, and
/// the fact-block lines that belong under it.
///
/// Dash refresh PR1: at most 2 lines (replacing the old 8-line disclosure
/// dump) -- `{fact_state} · {age}`, then `▸ {workflow} › {step} {i}/{n}`
/// only when this session has a bound workflow. Both hang off the tree's own
/// `│` for a row that has more siblings below it and off plain indentation
/// otherwise, so the group's vertical line is never broken by a fact.
/// `selected_session` is false while the cursor is parked on the summary or
/// a group header: the roster shows one cursor, so a session row must drop
/// its selected band (and its fact block with it) while something else owns
/// it.
fn roster_entry(
    row: &SidebarRow,
    width: u16,
    tick: usize,
    bands: (u32, u32),
    selected_session: bool,
) -> (Hit, Line<'static>, Vec<Line<'static>>) {
    let mut row = row.clone();
    row.selected &= selected_session;
    let line = Line::from(sidebar_row_parts(&row, tick, width, bands.0, bands.1));
    let prefix = if row.tree == TreePos::Child {
        "\u{2502}  "
    } else {
        "   "
    };
    let mut fact_lines = Vec::new();
    if row.selected {
        let age = row
            .fact_since_secs
            .map(style::format_age)
            .unwrap_or_else(|| style::PLACEHOLDER.into());
        fact_lines.push(Line::from(Span::styled(
            style::truncate_display(
                &format!("{prefix}{} \u{b7} {age}", row.fact_state),
                width as usize,
            )
            .into_owned(),
            style::tui::muted(),
        )));
        if let Some(workflow) = &row.workflow {
            let (text, style) = pane_header_workflow_span(workflow);
            fact_lines.push(Line::from(Span::styled(
                style::truncate_display(&format!("{prefix}{text}"), width as usize).into_owned(),
                style,
            )));
        }
    }
    (Hit::SidebarRow(row.short), line, fact_lines)
}

/// Draws a [`roster_frame`] result. The lines were already fitted to `area`
/// when they were laid out, so this is deliberately nothing but the blit --
/// no second layout pass that could disagree with the geometry a click will
/// be tested against.
pub fn render_roster(f: &mut Frame, area: Rect, roster: &RosterFrame) {
    f.render_widget(Paragraph::new(Text::from(roster.lines.clone())), area);
}

impl RosterFrame {
    /// How many rows this frame actually drew -- dash refresh PR1's own
    /// LIMITS block reads this to find out how much of the sidebar's own
    /// height the roster left blank (session rows always win the space; the
    /// caller never shortens `roster.lines` to make room for LIMITS).
    pub fn drawn_rows(&self) -> usize {
        self.lines.len()
    }
}

/// Pure: the geometry of the frame about to be drawn, in the shape
/// [`hit::hit_test`] answers questions about. Captured at draw time and kept
/// until the next successful draw replaces it, so a pointer event is always
/// resolved against the frame the operator was actually looking at when they
/// clicked -- not against a layout that has moved on since.
///
/// A zoomed frame has no chrome at all: the sidebar and its divider are empty
/// rects (which hit nothing) and the grid is the whole frame.
pub fn frame_snapshot(
    frame: Rect,
    layout: &DashLayout,
    zoomed: bool,
    roster: &RosterFrame,
    header: &HeaderFacts,
    overlay: &Overlay,
    tick: usize,
) -> FrameSnapshot {
    // Phase 3: the open dialog's own rows and hints, from the same spec and
    // the same layout function `render_overlay` draws through.
    let overlay_geom = overlay_geometry(
        frame,
        if zoomed { frame } else { layout.main },
        overlay,
        tick,
    );
    FrameSnapshot {
        frame,
        sidebar: if zoomed {
            Rect::default()
        } else {
            layout.sidebar
        },
        // Exactly the rect the divider is drawn into -- zero-width whenever
        // the layout left no column between the sidebar and the grid.
        divider: if zoomed {
            Rect::default()
        } else {
            Rect::new(
                layout.sidebar.right(),
                layout.sidebar.y,
                layout.main.x.saturating_sub(layout.sidebar.right()).min(1),
                layout.sidebar.height,
            )
        },
        grid: if zoomed { frame } else { layout.main },
        // Dash refresh PR1: the sidebar's title row is drawn from its own
        // rect now (`render_sidebar_title`, not part of `roster.lines`), so
        // its `Hit::SidebarSummary` click target is added here rather than
        // coming from `roster.hits` itself.
        rows: if zoomed {
            roster.hits.clone()
        } else {
            let mut hits = vec![(layout.sidebar_title, Hit::SidebarSummary)];
            hits.extend(roster.hits.clone());
            hits
        },
        roster: roster.row_ids.clone(),
        // Straight from the same layout pass `render_header` draws from, so
        // the click rects and the drawn chords can never describe different
        // columns (the same discipline the divider already follows).
        header_hints: header_hint_regions(layout.header, header),
        // The footer carries no `^A x` chords today (the spend segment and
        // the status grammar are both read-only), so it owns no hit regions
        // yet; `Hit::FooterHint` exists for the phases that add them.
        footer_hints: Vec::new(),
        zoomed,
        overlay: overlay_geom.as_ref().map(|(rect, ..)| *rect),
        overlay_rows: overlay_geom
            .as_ref()
            .map(|(_, rows, ..)| rows.clone())
            .unwrap_or_default(),
        overlay_hints: overlay_geom
            .as_ref()
            .map(|(_, _, hints, _)| hints.clone())
            .unwrap_or_default(),
        overlay_capacity: overlay_geom
            .map(|(_, _, _, capacity)| capacity)
            .unwrap_or_default(),
    }
}

/// Pure: whether visible-grid cell `(row, col)` falls inside a selection
/// already normalized to `start <= end` in row-major (row, then col) order
/// (`dash::normalize_selection` does the normalizing; this only ever sees the
/// result). Mirrors exactly the span `vt100::Screen::contents_between(
/// start.0, start.1, end.0, end.1)` copies, so the highlighted cells and the
/// copied text always agree: the whole of every row strictly between the
/// two, the tail of the start row from `start.1` onward, and the head of the
/// end row up to but not including `end.1` -- the same "up until end_col"
/// vt100 itself documents on `contents_between`.
fn cell_in_selection(row: u16, col: u16, start: (u16, u16), end: (u16, u16)) -> bool {
    if row < start.0 || row > end.0 {
        return false;
    }
    if start.0 == end.0 {
        return col >= start.1 && col < end.1;
    }
    if row == start.0 {
        col >= start.1
    } else if row == end.0 {
        col < end.1
    } else {
        true
    }
}

/// Walks every `vt100` cell in `screen` into `area`'s buffer, cell for cell.
/// A wide cell's own contents are drawn once and the following column is
/// skipped, matching how `vt100` itself represents double-width glyphs (the
/// continuation cell carries no contents of its own).
///
/// `selection`, when given, is a normalized `(start, end)` pair of
/// visible-grid `(row, col)` cells (`dash::mod`'s own click-drag selection,
/// already ordered by `normalize_selection`) drawn with `Modifier::REVERSED`
/// layered on top of the cell's own style -- the same tmux-style highlight a
/// terminal's native selection would have shown, now that mouse reporting
/// has displaced it (see `term::dash_mouse_on_bytes`).
///
/// Deliberately outside this phase's theme migration: this mirrors a live
/// child terminal's own colours and attributes verbatim, which is not
/// "dashboard chrome" in the sense the rest of this module's design language
/// applies to.
pub fn render_grid(
    f: &mut Frame,
    area: Rect,
    screen: &vt100::Screen,
    selection: Option<((u16, u16), (u16, u16))>,
) {
    let (rows, cols) = screen.size();
    let buf = f.buffer_mut();

    for row in 0..rows.min(area.height) {
        let mut skip_next = false;
        for col in 0..cols.min(area.width) {
            if skip_next {
                skip_next = false;
                continue;
            }
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };

            let x = area.x + col;
            let y = area.y + row;
            if x >= area.x + area.width || y >= area.y + area.height {
                continue;
            }
            let Some(target) = buf.cell_mut((x, y)) else {
                continue;
            };

            let symbol = cell.contents();
            target.set_symbol(if symbol.is_empty() { " " } else { symbol });

            let mut modifiers = Modifier::empty();
            if cell.bold() {
                modifiers |= Modifier::BOLD;
            }
            if cell.dim() {
                modifiers |= Modifier::DIM;
            }
            if cell.italic() {
                modifiers |= Modifier::ITALIC;
            }
            if cell.underline() {
                modifiers |= Modifier::UNDERLINED;
            }
            if cell.inverse() {
                modifiers |= Modifier::REVERSED;
            }
            if let Some((start, end)) = selection
                && cell_in_selection(row, col, start, end)
            {
                modifiers |= Modifier::REVERSED;
            }

            target.set_style(
                Style::default()
                    .fg(map_color(cell.fgcolor()))
                    .bg(map_color(cell.bgcolor()))
                    .add_modifier(modifiers),
            );

            if cell.is_wide() {
                skip_next = true;
            }
        }
    }
}

/// Pure: the marker a pane's title carries while its viewport is scrolled
/// back, or `None` when it is live.
///
/// A scrolled-back pane looks exactly like a wedged one -- the child keeps
/// producing output and the grid does not move -- so the operator needs to be
/// told which of the two they are looking at. `-N` reads as "N rows behind
/// live", the same sign convention tmux's own copy-mode indicator uses.
pub fn scroll_marker(scrollback: usize) -> Option<String> {
    if scrollback == 0 {
        None
    } else {
        Some(format!("SCROLL -{scrollback}"))
    }
}

/// Draws [`scroll_marker`] over the top-right corner of a pane's grid, in
/// reverse video so it reads as chrome rather than as something the child
/// printed. A no-op for a live pane.
///
/// Deliberately on the grid rather than in the header or the sidebar: `Ctrl+A
/// z` hides both of those, and a zoomed pane is exactly the one an operator is
/// most likely to be scrolling through. Clipped to `area`, so a grid too
/// narrow to hold the marker simply does not get one.
pub fn render_scroll_marker(f: &mut Frame, area: Rect, scrollback: usize) {
    let Some(marker) = scroll_marker(scrollback) else {
        return;
    };
    let width = marker.chars().count() as u16;
    if area.is_empty() || area.width < width {
        return;
    }
    let rect = Rect {
        x: area.x + area.width - width,
        y: area.y,
        width,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(marker).style(style::tui::selected_strong()),
        rect,
    );
}

/// Pure: the absolute frame position of `screen`'s text cursor when it should
/// be shown, given the `area` its grid was rendered into. `render_grid` draws
/// cell `(row, col)` at `(area.x + col, area.y + row)` with no border of its
/// own, so the cursor translates the same way. Returns `None` when the cursor
/// is hidden (`vt100::Screen::hide_cursor`) or falls outside `area` -- a caller
/// that gets `None` leaves the frame cursor unset, which ratatui renders as no
/// caret at all.
///
/// Only the *focused* pane's screen is ever passed here: a non-focused pane's
/// grid is not on screen, so its cursor contributes nothing.
pub fn grid_cursor_position(area: Rect, screen: &vt100::Screen) -> Option<Position> {
    if screen.hide_cursor() {
        return None;
    }
    let (row, col) = screen.cursor_position();
    if col >= area.width || row >= area.height {
        return None;
    }
    Some(Position::new(area.x + col, area.y + row))
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// The dialog box's own width inside `area`: four columns of margin when
/// there is room for them, never zero, and never wider than `area` itself.
///
/// Deliberately **not** `area.width.saturating_sub(4).clamp(1, area.width)`:
/// `Ord::clamp` asserts `min <= max` and panics when `area.width == 0`, which
/// a real session reaches (a terminal narrowed to at most the sidebar width
/// mid-session, or a `dash.sidebar_cols` wider than the terminal, both of
/// which make `ui::layout`'s own `main` rect zero-width) -- and the release
/// profile is `panic = "abort"`, so that takes the whole dashboard down with
/// it. Pure, so the degenerate widths are testable without a frame.
fn dialog_width(area_width: u16) -> u16 {
    area_width.saturating_sub(4).max(1).min(area_width.max(1))
}

/// A dialog's own row count (content rows plus `extra_rows` of chrome --
/// border/footer/blank lines) as a `u16`, clamped rather than cast: `rows`
/// comes from caller-controlled data (mail, memory, restore lists) with no
/// upper bound, so a plain `rows as u16` would silently wrap on a five-digit
/// row count, and `rows as u16 + extra_rows` (`+` on `u16`, not
/// `saturating_add`) can then overflow -- which panics in every debug/test
/// build (this profile has no `overflow-checks` override, so the default
/// `true` for dev/test applies) and silently wraps in release. Saturating
/// both the cast and the add means an enormous row count degrades to
/// "as tall as it can possibly be", clamped again by the caller's own
/// `.min(area.height)`, instead of corrupting the layout or aborting the
/// whole dashboard.
fn dialog_row_count(rows: usize, extra_rows: u16) -> u16 {
    u16::try_from(rows)
        .unwrap_or(u16::MAX)
        .saturating_add(extra_rows)
}

/// Splits a draft/input string on `\n` into one dialog-line entry per visual
/// row, so a multi-line draft renders one dialog row per visual line: the
/// first row keeps the `> ` prompt prefix, every continuation row gets a
/// two-space prefix that lines up under it.
fn draft_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                format!("> {line}")
            } else {
                format!("  {line}")
            }
        })
        .collect()
}

/// The border colour every dialog frame draws in (except the warn variant,
/// which keeps yellow): dim cyan, per the approved v3 mock (§A2). Shared by
/// [`render_dialog`] and [`render_list_dialog`] so the two frame primitives
/// can never drift from each other.
fn dialog_border_style() -> Style {
    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)
}

/// The plain (non-list) dialog frame: rounded, one column of horizontal
/// interior padding, opaque (`Clear` first, or the pane grid underneath
/// bleeds through every cell neither the border nor the text reaches -- see
/// the regression test below). Used directly by the two overlays the list
/// primitive does not fit (Nudge's free-text target+body, and a mail/memory
/// compose-or-edit buffer) so every dialog in the dashboard shares the same
/// frame treatment even where [`render_list_dialog`] itself is the wrong
/// shape.
fn render_dialog(f: &mut Frame, area: Rect, title: &str, lines: &[String]) {
    // Nothing to draw into: every renderer below would either be a no-op or
    // have to reason about a zero-sized rect. One guard, at the one place
    // every dialog funnels through.
    if area.is_empty() {
        return;
    }
    let h = dialog_row_count(lines.len(), 2).min(area.height);
    let w = dialog_width(area.width);
    let rect = centered(area, w, h);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        // Issue #209/v3 §A2: the shared dialog frame's border was colourless
        // (the terminal default) -- the approved mock gives every dialog a
        // dim-cyan border, matching the list-dialog primitive right below.
        // Not `tui::accent()` itself: that is now bold (§A1), and a bold
        // border would out-shout the title it is meant to frame.
        .border_style(dialog_border_style())
        .padding(Padding::horizontal(1))
        .title(Span::styled(title.to_string(), style::tui::accent()));
    // `Clear` first, or the dialog is transparent: a `Block` paints only its
    // border and a `Paragraph` only the cells its text reaches, so every
    // other cell inside `rect` keeps whatever the pane grid drew underneath.
    // Over a live harness screen that renders as a bordered box with the
    // child's output bleeding through it -- which is not merely ugly. An open
    // overlay swallows every keystroke (`filter_key` is not even called while
    // one is up), so a modal the operator cannot see is a dashboard that
    // appears to have stopped responding to `Ctrl+A` entirely. That is
    // exactly how this was reported from a real session.
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines.join("\n")).block(block), rect);
}

fn render_draft_dialog(
    f: &mut Frame,
    area: Rect,
    title: &str,
    input: &str,
    items: &[String],
    cursor: usize,
) {
    let mut lines = draft_lines(input);
    for (i, item) in items.iter().enumerate() {
        let marker = if i == cursor { '>' } else { ' ' };
        lines.push(format!("{marker} {item}"));
    }
    render_dialog(f, area, title, &lines);
}

/// Truncates a body preview to a single-line, human-scanning length, on
/// display width and with an ellipsis -- the display-width-aware equivalent
/// of the old byte/char-counting local `preview()`. Shared by the mail and
/// memory dialogs so a long message or entry never blows out a dialog row.
fn preview(text: &str, max_cols: usize) -> String {
    let first_line = text.lines().next().unwrap_or("");
    style::truncate_display_ellipsis(first_line, max_cols).into_owned()
}

/// One list-dialog row: plain text, an optional leading colour glyph (the
/// QuitConfirm spinner, in practice -- nothing else in this phase needs
/// one), an optional checkbox (`Some(_)` shows `[x]`/`[ ]`, `None` shows
/// neither -- most dialogs have no checkbox at all), and -- phase 3 -- an
/// optional dim `reason` drawn on the same row after the text, which is how
/// the context menu says an entry exists but cannot be used right now
/// without hiding it.
pub struct ListDialogRow {
    pub text: String,
    pub checked: Option<bool>,
    pub glyph: Option<(String, Style)>,
    /// Rendered dim, after `text`, on the same row. Never hidden and never
    /// its own line: an operator scanning the menu must see the entry and
    /// its reason together.
    pub reason: Option<String>,
    /// Whether the whole row renders dim (an unavailable menu entry). The
    /// cursor row still reverses uniformly on top of this, exactly as every
    /// other row does.
    pub dim: bool,
}

impl ListDialogRow {
    fn plain(text: String) -> Self {
        Self {
            text,
            checked: None,
            glyph: None,
            reason: None,
            dim: false,
        }
    }
}

/// The shared list-dialog primitive's own input: everything [`render_list_
/// dialog`] needs to draw one dialog, decoupled from which real overlay it
/// is drawing -- QuitConfirm, mail/memory browsing, restore, handover, the
/// help overlay, and phase 3's context menu and inspector all build one of
/// these rather than each hand-rolling its own `Block`/`Paragraph` pair.
pub struct ListDialogSpec<'a> {
    /// Owned rather than borrowed since phase 3: [`list_spec_for`] builds
    /// every dialog's spec in one place and some titles are formatted
    /// (`handover → pane a0000003`), which a `&str` field cannot outlive.
    pub title: String,
    pub count: Option<usize>,
    pub rows: Vec<ListDialogRow>,
    /// The row rendered REVERSED across the full row width. `None` when
    /// nothing in the dialog is cursor-addressable (the help overlay, an
    /// empty list).
    pub cursor: Option<usize>,
    /// First row of `rows` drawn. Phase 3: every list dialog scrolls, so a
    /// 60-entry mail list is no longer a dialog whose bottom half is off the
    /// screen. Clamped by [`list_dialog_layout`] so `cursor` is always
    /// visible however stale this is.
    pub offset: usize,
    /// `(key, action)` pairs, rendered two-tone (key bold, action dim,
    /// three spaces between pairs) on the dialog's own PINNED last row, one
    /// blank row below the list viewport -- neither scrolls away.
    pub footer: &'a [(&'a str, &'a str)],
    /// The warn variant: border and title render in `style::tui::warning()`
    /// (yellow) instead of `style::tui::accent()` (cyan).
    pub warn: bool,
    /// Shown, cursor-less, in place of `rows` when it is empty -- "(no
    /// mail)", "(nothing to restore)", and the like.
    pub empty_message: &'a str,
    /// Issue #354 phase 4: a one-line query input PINNED between the title
    /// and the list viewport, mirroring the pinned hint row at the bottom.
    /// `None` for every dialog that has no query of its own -- which is all
    /// of them except the palette/help overlay.
    pub input: Option<String>,
}

/// The exact `Block` every list dialog draws, with the title and border
/// style it was given. Shared by the renderer and [`list_dialog_layout`] so
/// the interior rect a hit is tested against is the *same* rect the rows are
/// drawn into -- computing it twice by hand is how a click ends up one
/// column off.
fn list_dialog_block(title: Line<'static>, border_style: Style) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .padding(Padding::horizontal(1))
        .title(title)
}

/// Where one list dialog's pieces land: the frame, the interior, which slice
/// of the list is on screen, the rect of each visible row (paired with its
/// index into the FULL row list, not its screen line), and the rect of each
/// hint span on the pinned hint row.
///
/// Pure, and the single source of truth for both drawing and hit-testing.
pub struct ListDialogLayout {
    pub rect: Rect,
    pub inner: Rect,
    /// How many list rows fit between the pinned title and the pinned hint
    /// row. Zero on a dialog with no room for a single row.
    pub capacity: usize,
    /// `1` when this dialog's pinned query input was actually given a row,
    /// `0` otherwise -- including a dialog that HAS an input but had no room
    /// left for it. The renderer reads this rather than re-deriving it, so
    /// the drawn line count and the row rects can never disagree.
    pub input_rows: u16,
    pub rows: Vec<(Rect, usize)>,
    pub hints: Vec<(Rect, HintId)>,
    /// `3–12 of 40`, only when the list does not fit.
    pub indicator: Option<String>,
}

/// Pure: `first–last of total`, or `None` when the whole list is on screen.
/// An en dash, matching the approved mock's own typography.
fn scroll_indicator(offset: usize, capacity: usize, total: usize) -> Option<String> {
    if capacity == 0 || total <= capacity {
        return None;
    }
    let first = offset.saturating_add(1);
    let last = offset.saturating_add(capacity).min(total);
    Some(format!("{first}\u{2013}{last} of {total}"))
}

/// Pure: the `KeyCode` a dialog hint's key label stands for, or `None` when
/// it stands for more than one key (`j/k`, `↑↓`) or for none at all
/// (`any key`). A hint with no single key gets no clickable region rather
/// than a guessed one -- a pointer must never reach a code path the keyboard
/// could not.
fn footer_key_code(key: &str) -> Option<KeyCode> {
    match key {
        "\u{23ce}" => Some(KeyCode::Enter),
        "esc" | "esc/q" => Some(KeyCode::Esc),
        "space" => Some(KeyCode::Char(' ')),
        _ => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphanumeric() => Some(KeyCode::Char(c)),
                _ => None,
            }
        }
    }
}

/// Pure: the whole geometry of one list dialog inside `area`. `None` when
/// there is nothing to draw into at all.
pub fn list_dialog_layout(area: Rect, spec: &ListDialogSpec) -> Option<ListDialogLayout> {
    if area.is_empty() {
        return None;
    }
    let content_rows = spec.rows.len().max(1);
    // +1 blank row above the hint row, +1 the hint row itself, +2 for the
    // block's own top/bottom border, and +1 more for a pinned query input
    // when the dialog has one (issue #354 phase 4).
    let extra = 2 + 2 + u16::from(spec.input.is_some());
    let h = dialog_row_count(content_rows, extra).min(area.height);
    let w = dialog_width(area.width);
    let rect = centered(area, w, h);
    let inner = list_dialog_block(Line::default(), Style::default()).inner(rect);
    if inner.is_empty() {
        return Some(ListDialogLayout {
            rect,
            inner,
            capacity: 0,
            input_rows: 0,
            rows: Vec::new(),
            hints: Vec::new(),
            indicator: None,
        });
    }

    // Reserved in the same priority order `chrome_rows` uses: the hint row
    // first (it is the dialog's own key list, and a modal whose keys are not
    // on screen is a modal an operator is stuck in), then the blank spacer,
    // and whatever is left is the list viewport.
    let hint_h = 1.min(inner.height);
    let blank_h = 1.min(inner.height.saturating_sub(hint_h));
    // Phase 4: the query input is pinned directly under the title, reserved
    // after the hint row and its spacer -- a palette whose keys are off
    // screen is the same trap as a modal whose keys are, and the query line
    // is what the operator is looking at while they type.
    let input_h = if spec.input.is_some() {
        1.min(inner.height.saturating_sub(hint_h + blank_h))
    } else {
        0
    };
    let capacity = inner.height.saturating_sub(hint_h + blank_h + input_h) as usize;

    let total = spec.rows.len();
    // Selection is always kept visible: the caller's own offset is honoured
    // only as far as it does not hide the caret.
    let offset = reveal_offset(
        total,
        capacity,
        spec.cursor.unwrap_or(spec.offset),
        spec.offset,
    );

    let mut rows = Vec::new();
    for slot in 0..capacity {
        let Some(index) = offset.checked_add(slot).filter(|i| *i < total) else {
            break;
        };
        rows.push((
            Rect {
                x: inner.x,
                y: inner.y.saturating_add(input_h).saturating_add(slot as u16),
                width: inner.width,
                height: 1,
            },
            index,
        ));
    }

    let mut hints = Vec::new();
    if hint_h > 0 {
        let hint_y = inner.y.saturating_add(inner.height.saturating_sub(1));
        let mut x = inner.x;
        for (i, (key, action)) in spec.footer.iter().enumerate() {
            if i > 0 {
                x = x.saturating_add(3);
            }
            let width = u16::try_from(style::display_width(key) + 1 + style::display_width(action))
                .unwrap_or(u16::MAX);
            let clipped = width.min(inner.right().saturating_sub(x.min(inner.right())));
            if clipped > 0
                && let Some(code) = footer_key_code(key)
            {
                hints.push((
                    Rect {
                        x,
                        y: hint_y,
                        width: clipped,
                        height: 1,
                    },
                    HintId::DialogKey(code),
                ));
            }
            x = x.saturating_add(width);
        }
    }

    Some(ListDialogLayout {
        rect,
        inner,
        capacity,
        input_rows: input_h,
        rows,
        hints,
        indicator: scroll_indicator(offset, capacity, total),
    })
}

/// The shared list-dialog primitive: a rounded, opaque, `Clear`-first frame
/// with one column of horizontal interior padding, a pinned title (accent,
/// or warning-yellow for the warn variant) with an optional muted count
/// beside it, a scrolling viewport of `spec.rows` (the cursor row REVERSED
/// across the full row width, always kept visible), a blank row, and a
/// pinned two-tone hint row carrying the dialog's own keys plus, when the
/// list does not fit, a right-aligned `3–12 of 40` scroll indicator.
///
/// Every dialog in the dashboard except Nudge's free-text prompt and a
/// mail/memory compose-or-edit buffer (see [`render_dialog`]'s own doc
/// comment) is built through this.
pub fn render_list_dialog(f: &mut Frame, area: Rect, spec: &ListDialogSpec) {
    let Some(geom) = list_dialog_layout(area, spec) else {
        return;
    };

    let title_style = if spec.warn {
        style::tui::warning()
    } else {
        style::tui::accent()
    };
    let border_style = if spec.warn {
        style::tui::warning()
    } else {
        // Issue #209/v3 §A2: dim-cyan, not the terminal-default colourless
        // border every dialog drew before.
        dialog_border_style()
    };
    // Issue #209/v3 §A3: `{title} · {n}` -- the title keeps its own accent
    // (or warning) weight and the count is a separate, dim span, replacing
    // the old single-span `{title} (n)` where the count read at the same
    // weight as the title itself.
    let title_spans: Vec<Span<'static>> = match spec.count {
        Some(n) => vec![
            Span::styled(spec.title.clone(), title_style),
            Span::styled(format!(" \u{b7} {n}"), style::tui::muted()),
        ],
        None => vec![Span::styled(spec.title.clone(), title_style)],
    };

    let block = list_dialog_block(Line::from(title_spans), border_style);
    f.render_widget(Clear, geom.rect);
    f.render_widget(block, geom.rect);
    let inner = geom.inner;
    if inner.is_empty() {
        return;
    }
    let inner_width = inner.width as usize;

    let mut lines: Vec<Line> = Vec::new();
    // Phase 4: the pinned query line, drawn before the viewport and counted
    // by `list_dialog_layout` so the row rects below it are the rects a
    // click is tested against.
    if let Some(query) = &spec.input
        && geom.input_rows > 0
    {
        lines.push(Line::from(vec![
            Span::styled("\u{203a} ", style::tui::accent()),
            Span::raw(query.clone()),
            Span::styled("\u{2588}", style::tui::muted()),
        ]));
    }
    if spec.rows.is_empty() {
        if geom.capacity > 0 {
            lines.push(Line::from(Span::styled(
                spec.empty_message.to_string(),
                style::tui::muted(),
            )));
        }
    } else {
        for (_, i) in &geom.rows {
            let row = &spec.rows[*i];
            let is_cursor = spec.cursor == Some(*i);
            let base = if row.dim {
                style::tui::muted()
            } else {
                Style::default()
            };
            let mut spans: Vec<Span> = Vec::new();
            if let Some((glyph, glyph_style)) = &row.glyph {
                spans.push(Span::styled(format!("{glyph} "), *glyph_style));
            }
            if let Some(checked) = row.checked {
                spans.push(Span::styled(
                    if checked { "[x] " } else { "[ ] " }.to_string(),
                    base,
                ));
            }
            spans.push(Span::styled(row.text.clone(), base));
            if let Some(reason) = &row.reason {
                spans.push(Span::styled(
                    format!("  \u{b7} {reason}"),
                    style::tui::muted(),
                ));
            }

            if is_cursor {
                let raw_width: usize = spans
                    .iter()
                    .map(|s| style::display_width(s.content.as_ref()))
                    .sum();
                // Bug fix (issue #209/v3 §B): the same defect as the
                // sidebar's selected row (see `render_sidebar`'s own doc
                // comment) -- patching REVERSED onto a span's own colour
                // (the QuitConfirm spinner glyph, in practice) swapped that
                // colour into the background and left the glyph rendered in
                // the terminal's default foreground instead of reversing
                // uniformly. Every span on the cursor row drops its own
                // style and takes the same plain reversed one.
                let reversed_style = Style::default().add_modifier(Modifier::REVERSED);
                let mut reversed: Vec<Span> = spans
                    .into_iter()
                    .map(|s| Span::styled(s.content, reversed_style))
                    .collect();
                let pad = inner_width.saturating_sub(raw_width);
                if pad > 0 {
                    reversed.push(Span::styled(" ".repeat(pad), reversed_style));
                }
                lines.push(Line::from(reversed));
            } else {
                lines.push(Line::from(spans));
            }
        }
    }

    // The viewport is padded out to its full height so the hint row stays
    // pinned to the bottom of the frame rather than floating up under a
    // short list.
    let body_rows = geom.capacity + geom.input_rows as usize;
    while lines.len() < body_rows {
        lines.push(Line::from(""));
    }
    // A3-3: the blank spacer only exists when the interior has room for it
    // ABOVE the hint row -- exactly the `blank_h` [`list_dialog_layout`]
    // reserves. On a one-row interior the layout puts the hints on that row;
    // drawing a spacer first pushed them off the bottom, so every click on
    // the only visible row resolved as a hint that was never on screen.
    // A3-3: the blank spacer exists only when the interior has room for it
    // ABOVE the hint row -- exactly the `blank_h` [`list_dialog_layout`]
    // reserves. On a one-row interior the layout puts the hints on that very
    // row; drawing a spacer first pushed them off the bottom, so a click on
    // the only visible row resolved as a hint that was never on screen.
    if body_rows + 1 < inner.height as usize {
        lines.push(Line::from(""));
    }
    let mut hint_spans: Vec<Span> = Vec::new();
    let mut hint_width = 0usize;
    for (i, (key, action)) in spec.footer.iter().enumerate() {
        if i > 0 {
            hint_spans.push(Span::raw("   "));
            hint_width += 3;
        }
        hint_spans.push(Span::styled(
            (*key).to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        hint_spans.push(Span::raw(" "));
        hint_spans.push(Span::styled((*action).to_string(), style::tui::hint()));
        hint_width += style::display_width(key) + 1 + style::display_width(action);
    }
    if let Some(indicator) = &geom.indicator {
        let pad = inner_width
            .saturating_sub(hint_width)
            .saturating_sub(style::display_width(indicator));
        if pad > 0 {
            hint_spans.push(Span::raw(" ".repeat(pad)));
        } else {
            hint_spans.push(Span::raw("  "));
        }
        hint_spans.push(Span::styled(indicator.clone(), style::tui::muted()));
    }
    lines.push(Line::from(hint_spans));

    f.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Which way one of the SHARED viewport keys moves a list dialog's caret.
/// Single-row motion is deliberately absent: `j/k` and the arrows belong to
/// each dialog's own reducer (`move_cursor`), and mean something else again
/// in a compose buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMove {
    PageUp,
    PageDown,
    Home,
    End,
}

/// Pure: the viewport move a key means for EVERY list dialog alike, or
/// `None` when the key is that dialog's own business.
///
/// Deliberately only the paging keys: `j/k` and the arrows already move the
/// caret inside each dialog's own reducer (and mean something else entirely
/// in a compose buffer), while PageUp/PageDown/Home/End were unbound in
/// every dialog before phase 3 and are pure viewport motion everywhere.
pub fn list_page_move(key: KeyEvent) -> Option<ListMove> {
    match key.code {
        KeyCode::PageUp => Some(ListMove::PageUp),
        KeyCode::PageDown => Some(ListMove::PageDown),
        KeyCode::Home => Some(ListMove::Home),
        KeyCode::End => Some(ListMove::End),
        _ => None,
    }
}

/// Pure: where a caret at `cursor` lands in a `len`-row list with `capacity`
/// rows on screen. Every arithmetic step is saturating -- `capacity` is `0`
/// on a dialog with no room and the release profile is `panic = "abort"`.
pub fn list_move(cursor: usize, len: usize, capacity: usize, mv: ListMove) -> usize {
    if len == 0 {
        return 0;
    }
    let last = len - 1;
    let page = capacity.max(1);
    match mv {
        ListMove::PageUp => cursor.saturating_sub(page),
        ListMove::PageDown => cursor.saturating_add(page).min(last),
        ListMove::Home => 0,
        ListMove::End => last,
    }
    .min(last)
}

/// Pure: the offset a wheel notch or a caret move leaves the viewport at --
/// [`reveal_offset`]'s own clamp, exposed for the overlay reducers so a
/// dialog's scroll state can never run past the end of its list.
pub fn list_scroll(len: usize, capacity: usize, cursor: usize, offset: usize) -> usize {
    reveal_offset(len, capacity, cursor, offset)
}

/// Pure: the one place every list-shaped overlay's [`ListDialogSpec`] is
/// built. `None` for an overlay that is not list-shaped at all -- Nudge's
/// free-text prompt, and a mail/memory compose-or-edit buffer, which go
/// through [`render_dialog`] instead.
///
/// Phase 3 exists because the spec is now needed TWICE per frame: once to
/// draw the dialog, and once (through [`list_dialog_layout`], from
/// [`frame_snapshot`]) to say where its rows and hints landed so a click can
/// address them. Building it in one function is what keeps the drawn dialog
/// and the clickable dialog the same dialog.
pub fn list_spec_for(overlay: &Overlay, tick: usize) -> Option<ListDialogSpec<'static>> {
    let cursor_of = |len: usize, cursor: usize| if len == 0 { None } else { Some(cursor) };
    match overlay {
        Overlay::None | Overlay::Spawn(_) | Overlay::Nudge(_) => None,
        Overlay::Mail(view) if view.compose.is_some() => None,
        Overlay::Memory(view) if view.input.is_some() => None,
        Overlay::QuitConfirm(working) => Some(ListDialogSpec {
            title: "\u{26a0} quit zirv dash".to_string(),
            count: Some(working.len()),
            rows: working
                .iter()
                .map(|title| ListDialogRow {
                    text: title.clone(),
                    checked: None,
                    glyph: Some((
                        style::tui::SPINNER_FRAMES[tick % style::tui::SPINNER_FRAMES.len()]
                            .to_string(),
                        style::tui::accent(),
                    )),
                    reason: None,
                    dim: false,
                })
                .collect(),
            // No cursor: nothing here is keyboard-navigable (there is no
            // j/k on this dialog, only Enter/Esc), so reversing a row would
            // read as a selection that does not exist.
            cursor: None,
            offset: 0,
            footer: QUIT_FOOTER,
            warn: true,
            empty_message: "nothing is still working",
            input: None,
        }),
        Overlay::Mail(view) => Some(ListDialogSpec {
            title: "mail".to_string(),
            count: Some(view.items.len()),
            rows: view
                .items
                .iter()
                .map(|(_, from, body)| {
                    ListDialogRow::plain(format!("{from}: {}", preview(body, 60)))
                })
                .collect(),
            cursor: cursor_of(view.items.len(), view.cursor),
            offset: view.offset,
            // Issue #209/v3 §A5: shares `MAIL_FOOTER` with the help overlay's
            // own "dialogs:" listing rather than a second, easily-drifting
            // copy of the same four hints.
            footer: MAIL_FOOTER,
            warn: false,
            empty_message: "(no mail)",
            input: None,
        }),
        Overlay::Memory(view) => Some(ListDialogSpec {
            title: "memory".to_string(),
            count: Some(view.entries.len()),
            rows: view
                .entries
                .iter()
                .map(|(key, age, body)| {
                    ListDialogRow::plain(format!("{key} ({age}) {}", preview(body, 40)))
                })
                .collect(),
            cursor: cursor_of(view.entries.len(), view.cursor),
            offset: view.offset,
            footer: MEMORY_FOOTER,
            warn: false,
            empty_message: "(no memory entries)",
            input: None,
        }),
        Overlay::Restore(view) => Some(ListDialogSpec {
            title: "restore".to_string(),
            count: Some(view.entries.len()),
            rows: view
                .entries
                .iter()
                .map(|entry| ListDialogRow {
                    text: entry.label.clone(),
                    checked: Some(entry.checked),
                    glyph: None,
                    reason: None,
                    dim: false,
                })
                .collect(),
            cursor: cursor_of(view.entries.len(), view.cursor),
            offset: view.offset,
            footer: RESTORE_FOOTER,
            warn: false,
            empty_message: "(nothing to restore)",
            input: None,
        }),
        // Issue #84: `draft.items` is already fully resolved
        // (agent/tier/model), so this only ever formats and marks the cursor
        // row -- no tier resolution or config reads happen here, matching
        // this module's own no-I/O contract. The swap's target pane goes in
        // the title rather than a trailing body row, so it stays visible even
        // once the item list scrolls.
        Overlay::Handover(draft) => Some(ListDialogSpec {
            title: format!("handover \u{2192} pane {}", draft.target_short),
            count: None,
            rows: draft
                .items
                .iter()
                .map(|(agent, tier, model)| {
                    ListDialogRow::plain(format!("{agent} / {tier} ({model})"))
                })
                .collect(),
            cursor: cursor_of(draft.items.len(), draft.cursor),
            offset: draft.offset,
            footer: HANDOVER_FOOTER,
            warn: false,
            empty_message: "no enabled, ready harness available to swap to",
            input: None,
        }),
        // Issue #354 phase 4: the palette and the help screen are one dialog
        // over one table -- the only difference is whether Enter runs the
        // selected row or just closes.
        Overlay::Palette(view) => {
            let rows = view.rows();
            Some(ListDialogSpec {
                title: view.mode.title().to_string(),
                count: Some(rows.iter().filter(|r| r.selectable()).count()),
                rows: rows.iter().map(palette_dialog_row).collect(),
                cursor: rows
                    .get(view.cursor)
                    .filter(|row| row.selectable())
                    .map(|_| view.cursor),
                offset: view.offset,
                footer: match view.mode {
                    PaletteMode::Run => PALETTE_FOOTER,
                    PaletteMode::Help => HELP_FOOTER,
                },
                warn: false,
                empty_message: "(nothing matches)",
                input: Some(view.query.clone()),
            })
        }
        // Issue #354 phase 5: each entry carries its own repeat count and the
        // age of its most recent repeat in the same dim trailing slot the
        // context menu uses for a disable reason, and an acknowledged entry
        // renders dim rather than disappearing.
        Overlay::Errors(view) => Some(ListDialogSpec {
            title: "errors".to_string(),
            count: Some(view.items.len()),
            rows: view.items.iter().map(error_dialog_row).collect(),
            cursor: cursor_of(view.items.len(), view.cursor),
            offset: view.offset,
            footer: ERRORS_FOOTER,
            warn: false,
            empty_message: "no recent errors",
            input: None,
        }),
        Overlay::Menu(view) => Some(ListDialogSpec {
            title: format!("actions \u{b7} {}", view.subject),
            count: None,
            rows: view
                .entries
                .iter()
                .enumerate()
                .map(|(i, entry)| ListDialogRow {
                    text: match entry.letter {
                        Some(letter) => format!("{}  {}", letter, entry.action.label()),
                        None => format!("   {}", entry.action.label()),
                    },
                    checked: None,
                    glyph: None,
                    reason: if view.confirm == Some(i) {
                        Some("confirm? y / n".to_string())
                    } else {
                        entry.disabled.clone()
                    },
                    dim: !entry.enabled(),
                })
                .collect(),
            cursor: cursor_of(view.entries.len(), view.cursor),
            offset: view.offset,
            footer: MENU_FOOTER,
            warn: false,
            empty_message: "(nothing to do here)",
            input: None,
        }),
        Overlay::Inspector(view) => {
            let rows = view.rows();
            Some(ListDialogSpec {
                title: format!("inspect \u{b7} {}", view.subject),
                count: None,
                rows: rows.iter().cloned().map(ListDialogRow::plain).collect(),
                cursor: cursor_of(rows.len(), view.cursor),
                offset: view.offset,
                footer: INSPECTOR_FOOTER,
                warn: false,
                empty_message: "(nothing recorded for this row)",
                input: None,
            })
        }
    }
}

/// What [`overlay_geometry`] hands `frame_snapshot`: the dialog's own rect,
/// the rect of each VISIBLE list row paired with its index into the full
/// list, the rect of each clickable hint on the pinned hint row, and how many
/// rows the viewport had room for (its page size).
pub type OverlayGeometry = (Rect, Vec<(Rect, usize)>, Vec<(Rect, HintId)>, usize);

/// Pure: the geometry of whatever overlay is open, in the shape
/// [`hit::hit_test`] answers questions about.
///
/// A non-list overlay (a compose buffer, the nudge prompt) has a rect and
/// nothing addressable inside it, which is exactly `Hit::Overlay`: consumed,
/// and a no-op.
pub fn overlay_geometry(
    frame: Rect,
    main: Rect,
    overlay: &Overlay,
    tick: usize,
) -> Option<OverlayGeometry> {
    if matches!(overlay, Overlay::None) {
        return None;
    }
    let area = overlay_area(frame, main);
    let Some(spec) = list_spec_for(overlay, tick) else {
        return Some((area, Vec::new(), Vec::new(), 0));
    };
    match list_dialog_layout(area, &spec) {
        Some(geom) => Some((geom.rect, geom.rows, geom.hints, geom.capacity)),
        None => Some((area, Vec::new(), Vec::new(), 0)),
    }
}

fn render_mail_compose(f: &mut Frame, area: Rect, draft: &ComposeDraft) {
    let mut lines = vec![format!(
        "compose to: {}",
        if draft.to.trim().is_empty() {
            "any"
        } else {
            draft.to.as_str()
        }
    )];
    lines.extend(draft_lines(&draft.body));
    lines.push("Enter to send, Esc to cancel".to_string());
    render_dialog(f, area, "mail", &lines);
}

fn render_memory_edit(f: &mut Frame, area: Rect, input: &str) {
    let mut lines = draft_lines(input);
    lines.push("Enter to save, Esc to cancel".to_string());
    render_dialog(f, area, "memory", &lines);
}

fn render_nudge_dialog(f: &mut Frame, area: Rect, draft: &NudgeDraft) {
    let target = match &draft.target {
        NudgeTarget::AttachedPane(short) => format!("pane {short}"),
        NudgeTarget::ViewOnlySession(short) => format!("session {short} (headless)"),
        NudgeTarget::None => "(no target selected)".to_string(),
    };
    let mut lines = vec![format!("to: {target}")];
    lines.extend(draft_lines(&draft.input));
    lines.push("Enter to send, Esc to cancel".to_string());
    render_dialog(f, area, "nudge", &lines);
}

/// A single centered status line over `area`. Used by the dashboard to show
/// "shutting down N pane(s)…" the instant a quit begins, so the operator is
/// not left staring at a frozen alternate screen while each pane's quit grace
/// elapses (M9).
pub fn render_center_message(f: &mut Frame, area: Rect, message: &str) {
    if area.is_empty() {
        return;
    }
    let rect = centered(area, area.width, 1.min(area.height));
    f.render_widget(
        Paragraph::new(message.to_string()).style(style::tui::title()),
        rect,
    );
}

/// The smallest main rect that can host a dialog and have it read as one: a
/// bordered box needs two columns and two rows of border before a single cell
/// of text, and anything narrower than this is a box the operator cannot
/// recognise as a modal.
const MIN_OVERLAY_COLS: u16 = 8;
const MIN_OVERLAY_ROWS: u16 = 3;

/// Pure: where an open overlay is actually drawn.
///
/// Normally the main rect. But an open overlay swallows every keystroke --
/// `filter_key` is not even called while one is up -- so an overlay that
/// declines to draw is a dashboard that has stopped responding with nothing on
/// screen to say why. `main` genuinely goes to zero (a terminal narrowed to at
/// most `dash.sidebar_cols`, or a `dash.sidebar_cols` wider than the
/// terminal), and returning early there is the same class of bug as the
/// transparent dialog: the modal is invisible and still modal. So when the
/// main rect is too small to host a dialog, the whole frame is used instead --
/// over the header and sidebar, which is the right trade for a modal.
fn overlay_area(frame: Rect, main: Rect) -> Rect {
    if main.width >= MIN_OVERLAY_COLS && main.height >= MIN_OVERLAY_ROWS {
        main
    } else {
        frame
    }
}

/// How wide the palette's own label column is before the chord starts.
/// Every label in [`actions::ACTIONS`] fits inside it, which a test pins; a
/// longer one simply pushes its chord one column right rather than
/// overlapping it.
const PALETTE_LABEL_COLS: usize = 16;

/// Pure: one errors-dialog row (issue #354 phase 5). The `\u{d7}n` repeat
/// count is part of the message itself -- it is the news -- while the age
/// sits in the dim trailing slot; an acknowledged entry dims whole.
fn error_dialog_row(item: &ErrorItem) -> ListDialogRow {
    let repeats = if item.count > 1 {
        format!(" \u{d7}{}", item.count)
    } else {
        String::new()
    };
    ListDialogRow {
        text: format!("\u{26a0} {}{repeats}", item.text),
        checked: None,
        glyph: None,
        reason: Some(style::format_age(item.age_secs)),
        dim: item.acked,
    }
}

/// Pure: the sticky header line's own text for one unacknowledged error --
/// the message, plus its repeat count when it has repeated. Shared by the
/// header and by `dash::mod`'s own `ErrorLog`, so the two can never disagree
/// about how a repeat is spelled.
pub fn error_line(text: &str, count: usize) -> String {
    if count > 1 {
        format!("{text} \u{d7}{count}")
    } else {
        text.to_string()
    }
}

/// Pure: one palette/help row as the shared list dialog draws it --
/// `label  chord`, with a section heading rendered as a dim standalone line
/// and a disabled action carrying its reason in the muted trailing slot every
/// other list dialog already uses.
fn palette_dialog_row(row: &PaletteRow) -> ListDialogRow {
    match row {
        PaletteRow::Section(name) => ListDialogRow {
            text: format!("{name}:"),
            checked: None,
            glyph: None,
            reason: None,
            dim: true,
        },
        // Review of cc92a56 (finding 1): drawn exactly like an action row so
        // the listing keeps one shape, but dim and with no reason -- it is
        // documentation, not a disabled binding.
        PaletteRow::Note { label, chord } => {
            let pad = PALETTE_LABEL_COLS.saturating_sub(style::display_width(label));
            ListDialogRow {
                text: format!("  {label}{}{chord}", " ".repeat(pad)),
                checked: None,
                glyph: None,
                reason: None,
                dim: true,
            }
        }
        PaletteRow::Action {
            label,
            chord,
            disabled,
            ..
        } => {
            let pad = PALETTE_LABEL_COLS.saturating_sub(style::display_width(label));
            ListDialogRow {
                text: format!("  {label}{}{chord}", " ".repeat(pad)),
                checked: None,
                glyph: None,
                reason: disabled.map(|reason| format!("disabled: {reason}")),
                dim: disabled.is_some(),
            }
        }
    }
}
/// Every other dialog's own footer hints, named once here and reused both to
/// build that dialog's [`ListDialogSpec`] and to list it in the help
/// overlay's own "dialogs:" section (`help_dialog_rows`) -- a static table
/// kept adjacent to the specs, per the phase's own design note, rather than
/// deriving it at runtime from a spec nothing keeps around between frames.
const QUIT_FOOTER: &[(&str, &str)] = &[("\u{23ce}", "quit and shut down"), ("esc", "stay")];
const MAIL_FOOTER: &[(&str, &str)] = &[
    ("\u{23ce}", "read+consume"),
    ("c", "compose"),
    // Issue #209/v3 §A5: `j/k` already moved the cursor (shared with every
    // other list dialog's up/down handling); this only documents it, per
    // the approved mock's footer.
    ("j/k", "move"),
    ("esc", "close"),
];
const MEMORY_FOOTER: &[(&str, &str)] = &[
    ("r", "remember"),
    ("d", "forget"),
    ("v", "verify"),
    ("esc", "close"),
];
const RESTORE_FOOTER: &[(&str, &str)] = &[
    ("space", "toggle"),
    ("\u{23ce}", "restore checked"),
    ("esc", "skip"),
];
const HANDOVER_FOOTER: &[(&str, &str)] = &[("\u{23ce}", "swap"), ("esc", "cancel")];
/// Issue #354 phase 5: `a` acknowledges in place (the entries stay, dimmed,
/// and the sticky header line clears); `esc`/`enter`/`q` acknowledge and
/// close, which is what an operator who has read the list has done.
const ERRORS_FOOTER: &[(&str, &str)] =
    &[("j/k", "scroll"), ("a", "acknowledge"), ("esc/q", "close")];
/// Issue #354 phase 3: the context menu's own keys. `esc` says `back`
/// rather than `close` because that is what it does -- the previously
/// focused pane keeps the keyboard, and nothing about the row changed.
const MENU_FOOTER: &[(&str, &str)] = &[
    ("\u{23ce}", "do"),
    ("esc", "back"),
    ("j/k", "move"),
    ("a-z", "jump"),
];
const INSPECTOR_FOOTER: &[(&str, &str)] = &[("j/k", "scroll"), ("esc", "back")];

/// Issue #354 phase 4. The palette runs what the caret is on; the help
/// screen is the same list read-only, so its Enter closes instead. Both say
/// outright that typing filters -- the one thing an operator cannot guess
/// from a list of rows, and finding F08's whole complaint about the old
/// static help screen.
const PALETTE_FOOTER: &[(&str, &str)] = &[
    ("\u{23ce}", "run"),
    ("esc", "close"),
    ("\u{2191}\u{2193}", "move"),
    ("type", "to filter"),
];
const HELP_FOOTER: &[(&str, &str)] = &[
    ("esc", "close"),
    ("\u{2191}\u{2193}", "move"),
    ("type", "to filter"),
];

pub fn render_overlay(f: &mut Frame, area: Rect, overlay: &Overlay, tick: usize) {
    // Never an early return on a too-small `area`: see `overlay_area`. Only a
    // frame with no cells at all leaves nothing to draw into, and
    // `render_dialog`/`render_list_dialog`'s own guards cover that.
    let area = overlay_area(f.area(), area);
    // Phase 3: every list-shaped overlay goes through the one shared
    // scrollable viewport, built from the one shared spec factory -- which
    // is also what `frame_snapshot` hit-tests against.
    if let Some(spec) = list_spec_for(overlay, tick) {
        render_list_dialog(f, area, &spec);
        return;
    }
    match overlay {
        Overlay::Spawn(d) => render_draft_dialog(f, area, "spawn", &d.input, &d.items, d.cursor),
        Overlay::Nudge(d) => render_nudge_dialog(f, area, d),
        Overlay::Mail(view) => {
            if let Some(draft) = &view.compose {
                render_mail_compose(f, area, draft);
            }
        }
        Overlay::Memory(view) => {
            if let Some(input) = &view.input {
                render_memory_edit(f, area, input);
            }
        }
        // Every other variant is list-shaped and was handled above.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::actions::ActionSection;
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Issue #354: real current-renderer frames with deterministic synthetic facts.
    /// The audit embeds this artifact; no PTY, harness, or state directory is used.
    #[test]
    fn capture_current_dashboard_for_354_audit() {
        let mut captures = String::new();
        for (width, height) in [(80, 20), (120, 40), (200, 50)] {
            for scenario in [
                "normal",
                "nine-panes",
                "zoomed",
                "help",
                "restore",
                "empty",
                "dead-footer",
            ] {
                let area = Rect::new(0, 0, width, height);
                let layout = layout(area, 44);
                let zoomed = scenario == "zoomed";
                let empty = matches!(scenario, "empty" | "dead-footer");
                let main = if zoomed { area } else { layout.main };
                let count = if empty {
                    0
                } else if scenario == "nine-panes" {
                    9
                } else {
                    3
                };
                let mut rows: Vec<SidebarRow> = (0..count)
                    .map(|i| SidebarRow {
                        role: "worker".into(),
                        model: None,
                        group: None,
                        tree: TreePos::Flat,
                        disclosure: Vec::new(),
                        short: format!("a{:07}", i + 1),
                        harness: if i % 2 == 0 { "claude" } else { "codex" }.to_string(),
                        age_secs: Some(90 + i * 60),
                        score: Some(12 + i as u32 * 9),
                        state: if scenario == "nine-panes" && i == 7 {
                            RowState::Dead
                        } else if i == 2 {
                            RowState::Idle
                        } else {
                            RowState::Working
                        },
                        status: None,
                        exit_code: None,
                        attached: true,
                        selected: i == 0,
                        focused: i == 0,
                        supervised: true,
                        fact_state: "working".into(),
                        fact_since_secs: Some(90 + i * 60),
                        workflow: None,
                        unread_mail: 0,
                    })
                    .collect();
                if scenario == "nine-panes" {
                    let mut external = sidebar_row("b0000010", "codex", RowState::Unknown);
                    external.attached = false;
                    rows.push(external);
                }
                let mut header = base_facts();
                header.hints.alive = !empty;
                header.sessions = rows.iter().filter(|r| r.state != RowState::Dead).count();
                header.working = rows.iter().filter(|r| r.state == RowState::Working).count();
                header.needs_you = rows
                    .iter()
                    .filter(|r| glyph_for(r) == Glyph::NeedsAction)
                    .count();
                let footer = if scenario == "dead-footer" {
                    FooterFacts::Dead(FooterDeadFacts {
                        harness: "claude".to_string(),
                        exited_age_secs: Some(42),
                        workflow: FooterWorkflow::None,
                    })
                } else if empty {
                    FooterFacts::None
                } else {
                    let mut facts = alive_footer_facts();
                    facts.score = rows[0].score;
                    facts.unread_mail = 3;
                    FooterFacts::Alive(facts)
                };
                let overlay = match scenario {
                    "help" => Overlay::Palette(PaletteView {
                        mode: PaletteMode::Help,
                        ..PaletteView::default()
                    }),
                    "restore" => Overlay::Restore(RestoreView {
                        entries: (1..=18)
                            .map(|i| RestoreEntry {
                                label: format!("worker {i:02} codex resume saved session"),
                                checked: i != 2,
                            })
                            .collect(),
                        cursor: 17,
                        offset: 0,
                    }),
                    _ => Overlay::None,
                };
                let mut parser = vt100::Parser::new(main.height, main.width, 100);
                parser.process(b"Harness terminal (synthetic audit fixture)\r\n\r\nTask: review dashboard interaction\r\nReading source files...\r\n\r\n> ");
                let nothing_collapsed = HashSet::new();
                let roster =
                    roster_frame(layout.sidebar, &rows, &test_roster_view(&nothing_collapsed));
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|f| {
                        if !zoomed {
                            render_header(f, layout.header, &header);
                            render_rule(f, layout.rule_top, layout.sidebar.width, true);
                            render_sidebar_title(f, layout.sidebar_title, rows.len());
                            render_mid_rule(f, layout.mid_rule, layout.sidebar.width);
                            render_roster(f, layout.sidebar, &roster);
                            render_sidebar_divider(
                                f,
                                Rect {
                                    x: layout.sidebar.x + layout.sidebar.width,
                                    width: 1,
                                    ..layout.sidebar
                                },
                            );
                            render_rule(f, layout.rule_bottom, layout.sidebar.width, false);
                            render_footer(f, layout.footer, &footer, 40, 70);
                        }
                        if !empty {
                            render_grid(f, main, parser.screen(), None);
                            render_scroll_marker(f, main, 0);
                            if matches!(overlay, Overlay::None)
                                && let Some(pos) = grid_cursor_position(main, parser.screen())
                            {
                                f.set_cursor_position(pos);
                            }
                        }
                        render_overlay(f, main, &overlay, 0);
                    })
                    .unwrap();
                captures.push_str(&format!(
                    "### CURRENT {width}x{height} — {scenario}\n\n```text\n"
                ));
                let buffer = terminal.backend().buffer();
                let mut frame_text = String::new();
                for y in 0..height {
                    let mut line = String::new();
                    for x in 0..width {
                        line.push_str(buffer[(x, y)].symbol());
                    }
                    assert_eq!(style::display_width(&line), usize::from(width));
                    frame_text.push_str(&line);
                    frame_text.push('\n');
                    captures.push_str(&line);
                    captures.push('\n');
                }
                // Issue #354 phase 3 closed the audit's own dialog findings:
                // an 18-entry restore dialog in a 20-row terminal used to
                // draw its list straight off the bottom of the frame, taking
                // both the caret's own row and the dialog's key hints with
                // it. The shared viewport scrolls to the caret and pins the
                // hint row, so both are on screen now.
                if width == 80 && scenario == "restore" {
                    assert!(
                        frame_text.contains("worker 18"),
                        "the caret row must be revealed: {frame_text}"
                    );
                    // The hint row is pinned but still only as wide as the
                    // dialog: in an 80-column frame with the 44-column
                    // sidebar the dialog's interior is ~27 columns, so the
                    // row is clipped after its first hint rather than
                    // wrapping or pushing the list around.
                    assert!(
                        frame_text.contains("space toggle"),
                        "the hint row must stay pinned: {frame_text}"
                    );
                }
                if width == 80 && scenario == "help" {
                    // Issue #354 phase 4: help is the palette in read-only
                    // mode, so its pinned hint row is `esc close` plus the
                    // `type to filter` affordance -- clipped to the dialog's
                    // ~27-column interior at this width, exactly as the
                    // restore dialog above is.
                    assert!(
                        frame_text.contains("esc close"),
                        "the hint row must stay pinned: {frame_text}"
                    );
                }
                captures.push_str("```\n\n");
            }
        }
        let output = std::path::Path::new("target/dash-ux-current-captures.md");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(output, captures).unwrap();
    }

    #[test]
    fn phase_one_render_matrix_and_exact_group_columns() {
        for (width, height) in [(80, 20), (120, 40), (200, 50)] {
            for scenario in ["quiet", "nine", "group", "pressure", "zoom"] {
                let mut rows: Vec<_> = (0..if scenario == "nine" { 9 } else { 3 })
                    .map(|i| {
                        let mut row = sidebar_row(
                            &format!("a{i:07}"),
                            "codex",
                            if i == 2 {
                                RowState::Idle
                            } else {
                                RowState::Working
                            },
                        );
                        row.age_secs = Some(540);
                        row.score = Some(21);
                        row.model = Some("gpt-6-astra".into());
                        row.selected = i == 1;
                        row.focused = i == 1;
                        if i == 1 {
                            row.role = "sub-orch".into();
                            row.fact_state = "working".into();
                            row.fact_since_secs = Some(540);
                        }
                        row
                    })
                    .collect();
                if scenario == "nine" {
                    rows[7].state = RowState::Dead;
                    let mut external = sidebar_row("external", "codex", RowState::Unknown);
                    external.attached = false;
                    rows.push(external);
                }
                if matches!(scenario, "group" | "pressure") {
                    for row in &mut rows {
                        row.group = Some(GroupRef {
                            id: "g".into(),
                            scope: "audit".into(),
                            lead_short: "a0000001".into(),
                        });
                    }
                }
                let area = Rect::new(0, 0, width, height);
                let layout = layout(area, 28);
                // Height pressure: 4 rows exactly fill a 4-tall roster (the
                // group header plus its 3 members), leaving no room at all
                // for the selected member's own fact-block line -- it must
                // drop before any session row does.
                let roster_area = if scenario == "pressure" {
                    Rect {
                        height: 4,
                        ..layout.sidebar
                    }
                } else {
                    layout.sidebar
                };
                let nothing_collapsed = HashSet::new();
                let roster =
                    roster_frame(roster_area, &rows, &test_roster_view(&nothing_collapsed));
                let zoomed = scenario == "zoom";
                let mut header = base_facts();
                header.hints.alive = true;
                let snapshot =
                    frame_snapshot(area, &layout, zoomed, &roster, &header, &Overlay::None, 0);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|f| {
                        if !zoomed {
                            render_header(f, layout.header, &header);
                            render_rule(f, layout.rule_top, layout.sidebar.width, true);
                            render_sidebar_title(f, layout.sidebar_title, rows.len());
                            render_mid_rule(f, layout.mid_rule, layout.sidebar.width);
                            render_roster(f, roster_area, &roster);
                            render_sidebar_divider(f, snapshot.divider);
                            render_rule(f, layout.rule_bottom, layout.sidebar.width, false);
                            render_footer(
                                f,
                                layout.footer,
                                &FooterFacts::Alive(alive_footer_facts()),
                                40,
                                70,
                            );
                        }
                    })
                    .unwrap();
                if zoomed {
                    assert!(snapshot.sidebar.is_empty());
                    assert_eq!(super::super::hit::hit_test(&snapshot, 0, 2), Hit::Grid);
                    continue;
                }
                assert_eq!(
                    roster
                        .hits
                        .iter()
                        .filter(|(_, hit)| matches!(hit, Hit::SidebarRow(_)))
                        .count(),
                    rows.len()
                );
                if scenario == "pressure" {
                    // 4 entries (1 group header + 3 members), no room left
                    // for the selected member's own fact line.
                    assert_eq!(roster.lines.len(), 4);
                    assert!(
                        !roster
                            .lines
                            .iter()
                            .any(|line| line.to_string().contains("working \u{b7}"))
                    );
                }
                if scenario == "group" {
                    // Entry 0 is the group header; entry 1 is the lead
                    // (`a0000001`, reordered to the front) -- the old
                    // `SidebarSummary` title-line hit that used to sit in
                    // front of both is gone (dash refresh PR1 moved it to
                    // its own rect, see `frame_snapshot`).
                    assert_eq!(roster.hits[1].1, Hit::SidebarRow("a0000001".into()));
                    // 1 row + 1 fact-block line (state+since; no bound
                    // workflow, so line 2 never appears).
                    assert_eq!(roster.hits[1].0.height, 2);
                    if width == 200 {
                        // Built from the 28-column row contract itself --
                        // `tree(1) glyph(1) sp name(10) sp harness(6) sp
                        // rot(3) sp badge(2) sp` -- not copied out of the
                        // mock, so a drifted renderer cannot be made to pass
                        // by editing a literal to match it.
                        let name_cols = 28 - SIDEBAR_FIXED_COLS;
                        let expected = format!(
                            "{tree}{glyph} {name:<name_cols$} {harness:<6} {rot:<3}    ",
                            tree = "\u{251c}",
                            glyph = style::tui::SPINNER_FRAMES[0],
                            name = "a0000001",
                            harness = "codex",
                            rot = format!("{ROT_GLYPH}21"),
                        );
                        assert_eq!(style::display_width(&expected), 28, "got {expected:?}");
                        let buffer = terminal.backend().buffer();
                        let sidebar_x0 = layout.sidebar.x;
                        // Entry 0 is the group header; the lead row is entry 1.
                        let row_y = layout.sidebar.y + 1;
                        let line: String = (0..28)
                            .map(|x| buffer[(sidebar_x0 + x, row_y)].symbol())
                            .collect();
                        assert_eq!(line, expected);
                        // The fact block, one line below: `{state} · {age}`,
                        // indented under the tree's own continuation.
                        let fact: String = (0..28)
                            .map(|x| buffer[(sidebar_x0 + x, row_y + 1)].symbol())
                            .collect();
                        assert!(fact.contains("working \u{b7} 9m"), "got {fact:?}");
                        // Dash refresh PR1 (§B replacement): a subtle
                        // background rather than REVERSED, so the glyph
                        // keeps its own colour under the selected band.
                        for x in 0..28 {
                            assert_eq!(
                                buffer[(sidebar_x0 + x, row_y)].bg,
                                Color::Indexed(236),
                                "col {x}"
                            );
                        }
                        assert_eq!(
                            buffer[(sidebar_x0 + 1, row_y)].fg,
                            Color::Cyan,
                            "the working glyph keeps its own colour"
                        );
                        assert!(
                            buffer[(sidebar_x0, row_y)]
                                .modifier
                                .contains(Modifier::BOLD),
                            "the focused+selected row is still bold"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn collapsed_group_keeps_rollup_and_viewport_does_not_move_selection() {
        let mut rows: Vec<_> = (0..20)
            .map(|i| sidebar_row(&format!("a{i:07}"), "claude", RowState::Working))
            .collect();
        rows[0].selected = true;
        for row in &mut rows[..3] {
            row.group = Some(GroupRef {
                id: "g".into(),
                scope: "audit".into(),
                lead_short: "a0000000".into(),
            });
        }
        let collapsed = HashSet::from(["g".into()]);
        let on_header = Hit::GroupToggle("g".into());
        let roster = roster_frame(
            Rect::new(0, 2, 44, 6),
            &rows,
            &RosterView {
                collapsed: &collapsed,
                chrome_selection: Some(&on_header),
                ..test_roster_view(&collapsed)
            },
        );
        // A folded group keeps its own header, its rollup and its place in
        // spawn order; its 3 members collapse into that one entry. Dash
        // refresh PR1: this is now the roster's own line 0 -- the title
        // line that used to sit in front of it lives in a separate rect
        // now (`DashLayout::sidebar_title`), not as part of `roster.lines`.
        assert!(roster.lines[0].to_string().starts_with("\u{25b8} audit"));
        assert!(roster.lines[0].to_string().contains("⠋3"));
        assert_eq!(roster.row_ids.len(), 18);
        // The wheel moves the viewport only: the selection is still row 0,
        // which the scrolled frame simply does not draw.
        let scrolled = roster_frame(
            Rect::new(0, 2, 44, 6),
            &rows,
            &RosterView {
                offset: 4,
                ..test_roster_view(&collapsed)
            },
        );
        assert_eq!(scrolled.hits[0].1, Hit::SidebarRow("a0000006".into()));
        assert!(rows[0].selected);
    }

    #[test]
    fn alive_header_hints_have_two_tones_and_matching_hits() {
        let mut facts = base_facts();
        facts.hints.alive = true;
        assert_eq!(header_hints(&facts.hints).len(), 4);
        assert_eq!(
            header_hints(&HintContext::default()),
            vec![("^A e", "errors"), ("^A ?", "help")]
        );
        let mut terminal = Terminal::new(TestBackend::new(200, 1)).unwrap();
        terminal
            .draw(|f| render_header(f, f.area(), &facts))
            .unwrap();
        for (rect, _) in header_hint_regions(Rect::new(0, 0, 200, 1), &facts) {
            assert_eq!(terminal.backend().buffer()[(rect.x, 0)].symbol(), "^");
            assert!(
                terminal.backend().buffer()[(rect.x, 0)]
                    .modifier
                    .contains(Modifier::DIM)
            );
            assert!(
                !terminal.backend().buffer()[(rect.x + 5, 0)]
                    .modifier
                    .contains(Modifier::DIM)
            );
        }
    }

    /// A3-3: at a one-row interior [`list_dialog_layout`] puts the hint row
    /// on `inner.y` itself. The renderer drew a blank spacer there and let
    /// the hint row clip off the bottom, so a click on the only visible row
    /// resolved as an `OverlayHint` that was never on screen.
    #[test]
    fn a_one_row_list_dialog_draws_its_hints_on_the_row_the_layout_hit_tests() {
        let footer: &[(&str, &str)] = &[("esc", "close")];
        let spec = ListDialogSpec {
            title: "errors".to_string(),
            count: None,
            rows: vec![ListDialogRow::plain("only row".to_string())],
            cursor: Some(0),
            offset: 0,
            footer,
            warn: false,
            empty_message: "(none)",
            input: None,
        };
        let area = Rect::new(0, 0, 20, 3);
        let geom = list_dialog_layout(area, &spec).expect("layout");
        let (hint_rect, _) = geom.hints.first().expect("a clickable hint rect");

        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        term.draw(|f| render_list_dialog(f, area, &spec)).unwrap();
        let buf = term.backend().buffer();
        let drawn: String = (hint_rect.x..hint_rect.right())
            .map(|x| buf[(x, hint_rect.y)].symbol())
            .collect();

        assert!(
            drawn.contains("esc"),
            "the row the layout hit-tests as a hint must be the row the renderer draws hints \
             on, got {drawn:?}"
        );
    }

    /// A dialog must be opaque. `Block` paints only its border and
    /// `Paragraph` only the cells its text reaches, so without a `Clear` the
    /// pane grid underneath bleeds through every other cell -- and because an
    /// open overlay swallows every keystroke before `filter_key` is reached, a
    /// modal that reads as pane content is a dashboard that looks like it has
    /// stopped responding to `Ctrl+A`. Reported from a real session; this
    /// pins the fix by drawing a dialog over a screen full of `X`.
    #[test]
    fn a_dialog_is_opaque_and_never_lets_the_pane_bleed_through() {
        let mut parser = vt100::Parser::new(8, 40, 0);
        for _ in 0..8 {
            parser.process(&[b'X'; 40]);
        }
        let backend = TestBackend::new(40, 8);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| {
            let area = f.area();
            render_grid(f, area, parser.screen(), None);
            render_dialog(f, area, "spawn", &["> prompt".to_string()]);
        })
        .expect("draw");

        let buf = term.backend().buffer();
        // The dialog is centred, so the row through its middle must contain
        // its border and text but no surviving grid character.
        let w = dialog_width(40);
        let rect = centered(Rect::new(0, 0, 40, 8), w, 3);
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                assert_ne!(
                    buf[(x, y)].symbol(),
                    "X",
                    "the pane grid bled through the dialog at ({x},{y})"
                );
            }
        }
        // Sanity: the grid is still painted outside the dialog, so the test
        // would fail for the right reason rather than because nothing drew.
        assert_eq!(buf[(0, 0)].symbol(), "X");
    }

    /// The list-dialog primitive must be just as opaque as the plain one.
    #[test]
    fn a_list_dialog_is_opaque_and_never_lets_the_pane_bleed_through() {
        let mut parser = vt100::Parser::new(10, 40, 0);
        for _ in 0..10 {
            parser.process(&[b'X'; 40]);
        }
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).expect("terminal");
        let spec = ListDialogSpec {
            title: "mail".to_string(),
            count: Some(1),
            rows: vec![ListDialogRow::plain("claude: hi".to_string())],
            cursor: Some(0),
            offset: 0,
            footer: MAIL_FOOTER,
            warn: false,
            empty_message: "(no mail)",
            input: None,
        };
        term.draw(|f| {
            let area = f.area();
            render_grid(f, area, parser.screen(), None);
            render_list_dialog(f, area, &spec);
        })
        .expect("draw");

        let buf = term.backend().buffer();
        let w = dialog_width(40);
        let rect = centered(Rect::new(0, 0, 40, 10), w, 5);
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                assert_ne!(
                    buf[(x, y)].symbol(),
                    "X",
                    "the pane grid bled through the dialog at ({x},{y})"
                );
            }
        }
        assert_eq!(buf[(0, 0)].symbol(), "X");
    }

    #[test]
    fn grid_renders_vt100_cells_with_colours() {
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(b"\x1b[31mred\x1b[0m plain");
        let backend = TestBackend::new(20, 4);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_grid(f, f.area(), parser.screen(), None))
            .unwrap();
        let cell = &term.backend().buffer()[(0, 0)];
        assert_eq!(cell.symbol(), "r");
        assert_eq!(cell.fg, Color::Red);
    }

    #[test]
    fn grid_skips_the_continuation_cell_of_a_wide_glyph() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        // A CJK wide glyph occupies two terminal columns; vt100 marks the
        // first cell wide and leaves the second as a plain continuation.
        parser.process("\u{4e2d}ab".as_bytes());
        let backend = TestBackend::new(10, 2);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_grid(f, f.area(), parser.screen(), None))
            .unwrap();
        let buf = term.backend().buffer();
        assert_eq!(buf[(0, 0)].symbol(), "\u{4e2d}");
        // Column 1 is the wide glyph's continuation cell -- skipped by the
        // renderer, so it keeps the backend's blank default rather than
        // showing "a" a column early.
        assert_eq!(buf[(1, 0)].symbol(), " ");
        assert_eq!(buf[(2, 0)].symbol(), "a");
        assert_eq!(buf[(3, 0)].symbol(), "b");
    }

    /// `cell_in_selection` mirrors `vt100::Screen::contents_between`'s own
    /// span exactly, cell for cell: the tail of the start row, every column
    /// of a middle row, and the head of the end row up to but not including
    /// `end.1`. Pinned against a multi-row selection first.
    #[test]
    fn cell_in_selection_spans_a_multi_row_selection_like_contents_between() {
        let start = (1, 5);
        let end = (3, 2);

        // Row above the selection: nothing is selected.
        assert!(!cell_in_selection(0, 0, start, end));
        assert!(!cell_in_selection(0, 40, start, end));

        // Start row: only from col 5 onward.
        assert!(!cell_in_selection(1, 4, start, end));
        assert!(cell_in_selection(1, 5, start, end));
        assert!(cell_in_selection(1, 999, start, end));

        // A middle row: every column, including 0.
        assert!(cell_in_selection(2, 0, start, end));
        assert!(cell_in_selection(2, 999, start, end));

        // End row: only up to (not including) col 2.
        assert!(cell_in_selection(3, 0, start, end));
        assert!(cell_in_selection(3, 1, start, end));
        assert!(!cell_in_selection(3, 2, start, end));
        assert!(!cell_in_selection(3, 40, start, end));

        // Row below the selection: nothing is selected.
        assert!(!cell_in_selection(4, 0, start, end));
    }

    /// A single-row selection is the half-open `[start.1, end.1)` range on
    /// that one row and nothing on any other row -- the `Ordering::Equal`
    /// branch `contents_between` itself takes.
    #[test]
    fn cell_in_selection_on_a_single_row_is_the_half_open_column_range() {
        let start = (2, 3);
        let end = (2, 7);

        assert!(!cell_in_selection(2, 2, start, end));
        assert!(cell_in_selection(2, 3, start, end));
        assert!(cell_in_selection(2, 6, start, end));
        assert!(!cell_in_selection(2, 7, start, end), "end col is exclusive");

        // Same row, but outside the selection's own row range.
        assert!(!cell_in_selection(1, 5, start, end));
        assert!(!cell_in_selection(3, 5, start, end));

        // A zero-width same-row "selection" (anchor == end, i.e. a click
        // rather than a drag) selects nothing at all.
        assert!(!cell_in_selection(2, 3, (2, 3), (2, 3)));
    }

    /// `render_grid` layers `Modifier::REVERSED` onto exactly the cells
    /// `cell_in_selection` reports, leaving every other cell's style alone.
    #[test]
    fn render_grid_highlights_only_the_selected_cells() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"aaaaaaaaaa\r\nbbbbbbbbbb\r\ncccccccccc");
        let backend = TestBackend::new(10, 3);
        let mut term = Terminal::new(backend).unwrap();
        // Row 1, cols 2..5 selected (single row).
        term.draw(|f| render_grid(f, f.area(), parser.screen(), Some(((1, 2), (1, 5)))))
            .unwrap();
        let buf = term.backend().buffer();
        assert!(!buf[(1, 1)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(2, 1)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(4, 1)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(5, 1)].modifier.contains(Modifier::REVERSED));
        // Untouched rows carry no highlight at all.
        assert!(!buf[(2, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(2, 2)].modifier.contains(Modifier::REVERSED));
    }

    /// No selection at all leaves every cell exactly as it was rendered
    /// before this feature existed -- the `None` default every other caller
    /// still passes.
    #[test]
    fn render_grid_with_no_selection_reverses_nothing_the_cell_itself_did_not_ask_for() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"plain text");
        let backend = TestBackend::new(10, 2);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_grid(f, f.area(), parser.screen(), None))
            .unwrap();
        let buf = term.backend().buffer();
        for x in 0..10 {
            assert!(!buf[(x, 0)].modifier.contains(Modifier::REVERSED));
        }
    }

    /// HIGH-1: the focused pane's cursor translates from a screen-relative
    /// `(row, col)` to an absolute frame position by adding the grid area's
    /// own origin, so a caret rendered at the pane's inner offset lands where
    /// the child actually put it.
    #[test]
    fn grid_cursor_translates_screen_position_into_the_grid_area() {
        let mut parser = vt100::Parser::new(10, 40, 0);
        // Two lines then a partial third: the cursor sits at row 2, col 5.
        parser.process(b"line one\r\nline two\r\nabcde");
        let (row, col) = parser.screen().cursor_position();
        assert_eq!((row, col), (2, 5), "sanity: vt100 cursor is where we typed");

        let area = Rect::new(3, 1, 40, 10);
        let pos = grid_cursor_position(area, parser.screen()).expect("cursor is visible");
        assert_eq!(pos, Position::new(3 + 5, 1 + 2));
    }

    /// A hidden cursor contributes no caret, and a cursor past the grid area's
    /// own bounds is not drawn outside it.
    #[test]
    fn grid_cursor_is_none_when_hidden_or_out_of_bounds() {
        let mut parser = vt100::Parser::new(10, 40, 0);
        // DECTCEM off: the harness has hidden its cursor.
        parser.process(b"\x1b[?25l");
        assert!(grid_cursor_position(Rect::new(0, 0, 40, 10), parser.screen()).is_none());

        let mut visible = vt100::Parser::new(10, 40, 0);
        visible.process(b"\x1b[?25h");
        // A grid area narrower/shorter than the cursor's own coordinates: the
        // caret would land outside the drawn cells, so it is suppressed.
        visible.process(b"\r\n\r\n\r\nx");
        assert!(grid_cursor_position(Rect::new(0, 0, 40, 2), visible.screen()).is_none());
    }

    /// A live pane carries no marker at all; a scrolled-back one says how far
    /// back it is, so "the grid is not updating" reads as a viewport the
    /// operator moved rather than as a wedged child.
    #[test]
    fn the_scroll_marker_appears_only_while_a_pane_is_scrolled_back() {
        assert_eq!(scroll_marker(0), None);
        assert_eq!(scroll_marker(1).as_deref(), Some("SCROLL -1"));
        assert_eq!(scroll_marker(42).as_deref(), Some("SCROLL -42"));
        assert_eq!(scroll_marker(1000).as_deref(), Some("SCROLL -1000"));
    }

    #[test]
    fn the_scroll_marker_renders_in_the_grids_top_right_corner() {
        let area = Rect::new(0, 0, 40, 6);
        let text = render_and_capture_text(area, |f, area| render_scroll_marker(f, area, 12));
        assert!(
            text.contains("SCROLL-12") || text.contains("SCROLL -12"),
            "got {text}"
        );
        // Top row, flush right: the last cell of row 0 is the marker's last
        // character, and nothing at all is drawn on any other row.
        let backend = TestBackend::new(40, 6);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_scroll_marker(f, area, 12)).unwrap();
        let buf = term.backend().buffer();
        assert_eq!(buf[(39, 0)].symbol(), "2");
        assert!(
            (0..40).all(|x| buf[(x, 1)].symbol() == " "),
            "the marker is one row tall"
        );
    }

    /// Never draws outside its area, and never panics on the degenerate
    /// geometries `layout` genuinely produces (a terminal narrowed to at most
    /// the sidebar width leaves a zero-sized main rect).
    #[test]
    fn the_scroll_marker_is_skipped_when_there_is_no_room_for_it() {
        let backend = TestBackend::new(20, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 20, 0),
            Rect::new(0, 0, 0, 4),
            Rect::new(0, 0, 5, 4),
            Rect::new(0, 0, 1, 1),
        ] {
            term.draw(|f| render_scroll_marker(f, area, 12))
                .expect("draw");
        }
        // A live pane draws nothing even with all the room in the world.
        let text = render_and_capture_text(Rect::new(0, 0, 20, 2), |f, area| {
            render_scroll_marker(f, area, 0)
        });
        assert!(text.trim().is_empty(), "got {text:?}");
    }

    /// Issue #209/v3 §A4/§D: the sidebar's own box border becomes a
    /// full-width top rule and bottom rule around the body, plus a new
    /// footer row -- one of each on a frame with room for all four.
    #[test]
    fn layout_reserves_the_header_rules_footer_and_the_sidebar() {
        let l = layout(Rect::new(0, 0, 100, 30), 24);
        assert_eq!(l.header.height, 1, "one header row at every height");
        assert_eq!(l.rule_top.height, 1);
        assert_eq!(l.rule_bottom.height, 1);
        assert_eq!(l.footer.height, 1);
        // Dash refresh PR1: the title/pane-header row and the rule below it
        // (`sidebar_title`/`pane_header`/`mid_rule`) are each one more row
        // `sidebar`/`main` no longer include.
        assert_eq!(l.sidebar_title.height, 1);
        assert_eq!(l.pane_header.height, 1);
        assert_eq!(l.mid_rule.height, 1);
        assert_eq!(l.sidebar.width, 24);
        assert_eq!(l.main.width, 100 - 24 - 1);
        assert_eq!(l.sidebar.height, 30 - 1 - 1 - 1 - 1 - 1 - 1);
        assert_eq!(l.main.height, l.sidebar.height);
    }

    #[test]
    fn layout_is_stable_on_a_tiny_area_without_underflow() {
        let l = layout(Rect::new(0, 0, 10, 2), 24);
        assert_eq!(l.header.height, 1);
        // Only the header and the footer fit in a two-row frame -- the
        // rules are the first chrome dropped (`chrome_rows`'s own priority
        // order), and the body has nothing left at all.
        assert_eq!(l.footer.height, 1);
        assert_eq!(l.rule_top.height, 0);
        assert_eq!(l.rule_bottom.height, 0);
        assert_eq!(l.sidebar.width, 10);
        assert_eq!(l.main.width, 0);
        assert_eq!(l.main.height, 0);
    }

    /// Dash refresh PR1: below 100 columns the session column hides, unless
    /// the operator has forced it back on -- and forcing it holds regardless
    /// of width until toggled again.
    #[test]
    fn sidebar_hides_under_100_cols_unless_forced_visible() {
        assert!(sidebar_hidden(99, false));
        assert!(!sidebar_hidden(100, false), "100 is the floor, not hidden");
        assert!(
            !sidebar_hidden(80, true),
            "forced visible holds at any width"
        );
        assert!(!sidebar_hidden(200, false));
    }

    /// `layout(area, 0)` -- what the caller passes once `sidebar_hidden`
    /// says to -- gives `main` the WHOLE frame: no sidebar width and no
    /// separator column wasted on a divider with nothing to divide.
    #[test]
    fn layout_with_no_sidebar_gives_main_the_whole_frame() {
        let l = layout(Rect::new(0, 0, 80, 24), 0);
        assert_eq!(l.sidebar.width, 0);
        assert_eq!(l.main.x, 0);
        assert_eq!(l.main.width, 80, "no separator column wasted");
    }

    /// Dash refresh PR1: below the narrow-terminal floor the header shows
    /// every session as a tab -- the focused one tinted with the same
    /// `Color::Indexed(236)` background the selected sidebar row uses, an
    /// unread-mail badge included, and the chip/hint cluster unchanged.
    #[test]
    fn render_header_tabs_shows_every_session_and_tints_the_focused_one() {
        let mut orch = sidebar_row("orch0001", "claude", RowState::Working);
        orch.role = "orch".into();
        orch.focused = true;
        let mut worker = sidebar_row("f7e21a90", "claude", RowState::Idle);
        worker.unread_mail = 1;
        let rows = vec![orch, worker];
        let mut facts = base_facts();
        facts.hints.alive = true;
        let backend = TestBackend::new(80, 1);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_header_tabs(f, f.area(), &facts, &rows, 0))
            .expect("draw");
        let buf = term.backend().buffer();
        let text: String = (0..80).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(text.contains("zirv"), "got {text:?}");
        assert!(text.contains("orch"), "got {text:?}");
        assert!(text.contains("f7e2"), "truncated to 4 chars: got {text:?}");
        assert!(
            text.contains("\u{2709}1"),
            "the unread-mail badge: got {text:?}"
        );
        assert!(
            text.contains("help"),
            "the hint cluster survives: got {text:?}"
        );
        // The focused tab's own glyph cell carries the selected-row tint.
        let orch_glyph_x = text
            .find('\u{280b}')
            .map(|byte_idx| text[..byte_idx].chars().count() as u16);
        if let Some(x) = orch_glyph_x {
            assert_eq!(buf[(x, 0)].bg, Color::Indexed(236));
        }
    }

    /// A very narrow area still never overflows its own column budget --
    /// tabs are dropped whole, never half-drawn, once the hints/chip alone
    /// do not leave room for one.
    #[test]
    fn render_header_tabs_never_overflows_a_narrow_area() {
        let rows: Vec<SidebarRow> = (0..8)
            .map(|i| sidebar_row(&format!("a{i:07}"), "claude", RowState::Working))
            .collect();
        let mut facts = base_facts();
        facts.hints.alive = true;
        for width in 0..=80u16 {
            let text = render_and_capture_text(Rect::new(0, 0, width, 1), |f, area| {
                render_header_tabs(f, area, &facts, &rows, 0)
            });
            assert!(
                style::display_width(&text) <= width as usize,
                "width {width} overflowed: {text:?}"
            );
        }
    }

    /// Dash refresh PR1: below the narrow-terminal floor the footer shows
    /// the focused pane's own harness usage instead of the ordinary
    /// verdict/mail row -- `5h {pct}% · resets {time}`.
    #[test]
    fn render_footer_narrow_usage_shows_the_five_hour_reading_and_its_reset() {
        let now = 100 * 86_400 + 14 * 3600;
        let usage = HarnessUsage {
            name: "claude",
            five_hour: Some(41.0),
            seven_day: None,
            five_hour_detail: Some(WindowDetail {
                resets_at: now + 2 * 3600 + 13 * 60,
                limit_reached: false,
                overage_covered: false,
            }),
            seven_day_detail: None,
            credits: false,
        };
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer_narrow_usage(f, area, Some(&usage), now, utc())
        });
        assert!(text.contains("5h 41%"), "got {text:?}");
        assert!(text.contains("resets"), "got {text:?}");
    }

    /// No usage at all for the focused harness (or nothing focused) draws
    /// nothing, never a fabricated reading.
    #[test]
    fn render_footer_narrow_usage_draws_nothing_with_no_reading() {
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer_narrow_usage(f, area, None, 0, utc())
        });
        assert!(text.trim().is_empty(), "got {text:?}");
    }

    /// The header is one row at every height. A zero/one-row frame never
    /// underflows on the way there (the release profile is `panic =
    /// "abort"`).
    #[test]
    fn the_header_is_one_row_at_every_terminal_height() {
        assert_eq!(header_rows(0), 0);
        assert_eq!(header_rows(1), 1);
        assert_eq!(header_rows(2), 1);
        assert_eq!(
            header_rows(super::super::super::chrome::MIN_DASH_ROWS - 1),
            1
        );
        assert_eq!(header_rows(super::super::super::chrome::MIN_DASH_ROWS), 1);
        assert_eq!(header_rows(200), 1);
        // And the body never loses more rows than the fixed chrome gained --
        // every row of `height` is accounted for by exactly one of the six
        // rects `layout` hands back.
        for height in 0..=64u16 {
            let l = layout(Rect::new(0, 0, 80, height), 24);
            assert_eq!(
                l.header.height
                    + l.rule_top.height
                    + l.sidebar_title.height
                    + l.mid_rule.height
                    + l.main.height
                    + l.rule_bottom.height
                    + l.footer.height,
                height,
                "height {height} lost a row"
            );
        }
    }

    fn base_facts() -> HeaderFacts {
        HeaderFacts {
            hints: HintContext::default(),
            sessions: 1,
            working: 0,
            needs_you: 0,
            error_count: 0,
            latest_error: None,
            notice: None,
            tip: None,
        }
    }

    #[test]
    fn header_shows_the_brand_mark_and_the_session_count() {
        let mut facts = base_facts();
        facts.sessions = 3;
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(text.contains("zirv"), "brand mark missing: {text}");
        assert!(text.contains("3 sessions"), "got {text:?}");
    }

    /// A zero `working`/`needs_you` count omits its whole segment -- never a
    /// literal "0 working" or "0 needs you".
    #[test]
    fn header_omits_zero_count_segments() {
        let facts = base_facts();
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(!text.contains("working"), "got {text:?}");
        assert!(!text.contains("needs you"), "got {text:?}");
    }

    /// The mock's own worked example (§01): `3 sessions · 1 working · 1
    /// needs you`, in that order, once both counts are nonzero.
    #[test]
    fn header_shows_working_and_needs_you_when_nonzero() {
        let mut facts = base_facts();
        facts.sessions = 3;
        facts.working = 1;
        facts.needs_you = 1;
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(
            text.contains("3 sessions \u{b7} 1 working \u{b7} 1 needs you"),
            "got {text:?}"
        );
    }

    #[test]
    fn header_shows_the_errors_hint_and_help_hint() {
        let facts = base_facts();
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(text.contains("errors"), "got {text}");
        assert!(text.contains("help"), "got {text}");
    }

    /// Issue #697 removed select mode (`Ctrl+A v`) entirely -- the dashboard
    /// keeps its own mouse reporting on for the whole session now, so there
    /// is no runtime state left for a `SELECT` marker to report, and
    /// `HeaderFacts` no longer even carries a field for it. This pins the
    /// negative: whatever else the header shows, `SELECT` never appears.
    #[test]
    fn header_never_shows_a_select_mode_marker() {
        let facts = base_facts();
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(!text.contains("SELECT"), "header text was: {text}");
    }

    /// The count cluster is short and digit-bounded (unlike the old
    /// free-text harness/model label it replaces), but the hint cluster
    /// must still win the row over it when every segment is showing at
    /// once, at a typical terminal width.
    #[test]
    fn header_keeps_the_hint_cluster_visible_with_every_count_segment_shown() {
        let mut facts = base_facts();
        facts.sessions = 42;
        facts.working = 17;
        facts.needs_you = 9;
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(text.contains("errors"), "hints missing: {text}");
        assert!(text.contains("help"), "hints missing: {text}");
        // The brand mark stays intact alongside every count segment.
        assert!(text.contains("zirv"), "brand mark missing: {text}");
    }

    /// Issue #354 phase 2 (review of bf1474f): the header's click rects come
    /// from the same layout pass that drew the chords, so at a width where the
    /// fixed left content plus the cluster no longer fit -- the cluster is
    /// pushed right, off the row, instead of staying right-aligned -- every
    /// rect still lands exactly on its own chord's `^`, and a chord that was
    /// not drawn at all has no rect to click. The old right-aligned
    /// recomputation put rects on top of the chip and the harness label.
    /// Issue #354 phase 5 extends the same sweep over the three things the
    /// header's middle slot can carry: an attention notice, a sticky error
    /// line with a repeat count, and the first-run tip. None of them may push
    /// a chord off the row or land a click rect on anything but its own `^`.
    #[test]
    fn header_hint_rects_land_on_the_chords_that_were_actually_drawn() {
        /// One way of filling the header's flexible middle slot.
        type FillMiddle = fn(&mut HeaderFacts);
        let middles: [(&str, FillMiddle); 4] = [
            ("nothing", |_| {}),
            ("a notice", |f| {
                f.notice = Some("\u{25b2} a0000002 needs approval".to_string());
            }),
            ("a sticky error", |f| {
                f.error_count = 3;
                f.latest_error = Some(error_line("mail send: disk full", 4));
            }),
            ("the first-run tip", |f| {
                f.tip = Some(FIRST_RUN_TIP.as_str());
            }),
        ];
        for (what, fill) in middles {
            let mut facts = base_facts();
            facts.hints.alive = true;
            fill(&mut facts);
            for width in 1..=90u16 {
                let area = Rect::new(0, 0, width, 1);
                let backend = TestBackend::new(width, 1);
                let mut term = Terminal::new(backend).unwrap();
                term.draw(|f| render_header(f, area, &facts)).unwrap();
                let buf = term.backend().buffer().clone();
                let regions = header_hint_regions(area, &facts);
                for (rect, id) in &regions {
                    assert!(
                        rect.right() <= area.right(),
                        "{what}, width {width}: {id:?} rect {rect:?} runs past the header"
                    );
                    assert_eq!(
                        buf[(rect.x, 0)].symbol(),
                        "^",
                        "{what}, width {width}: {id:?} rect {rect:?} must start on its own chord"
                    );
                }
                // And no rect may claim a column the brand mark owns: at
                // every width the leftmost rect starts at or after `▌zirv`
                // itself (the left cluster's own fixed-shape prefix).
                if let Some((first, _)) = regions.first() {
                    assert!(
                        first.x >= style::display_width("\u{258c}zirv") as u16,
                        "{what}, width {width}: a hint rect overlapped the brand mark"
                    );
                }
            }
        }
    }

    /// The cluster is chosen by the selected row's own state, most specific
    /// first, and never exceeds the approved four-hint cap.
    #[test]
    fn header_hints_follow_the_selected_rows_state() {
        let needs_action = HintContext {
            alive: true,
            needs_action: true,
            ended: false,
            restorable: false,
            summary: false,
        };
        assert_eq!(
            header_hints(&needs_action),
            vec![
                ("^A i", "inspect"),
                ("^A c", "actions"),
                ("^A n", "nudge"),
                ("^A ?", "help"),
            ]
        );
        let ended = HintContext {
            alive: false,
            needs_action: false,
            ended: true,
            restorable: false,
            summary: false,
        };
        // Issue #354 phase 3: an ended row whose spawn request the dashboard
        // no longer holds is still never offered `^A r` -- the hint would do
        // nothing, and the context menu is where the reason lives.
        assert_eq!(
            header_hints(&ended),
            vec![("^A i", "inspect"), ("^A c", "actions"), ("^A ?", "help")]
        );
        assert!(!header_hints(&ended).iter().any(|(_, l)| *l == "restore"));
        // With the request kept, the approved ended cluster is all four.
        let restorable = HintContext {
            restorable: true,
            ..ended
        };
        assert_eq!(
            header_hints(&restorable),
            vec![
                ("^A i", "inspect"),
                ("^A r", "restore"),
                ("^A c", "actions"),
                ("^A ?", "help"),
            ]
        );
        for context in [
            needs_action,
            ended,
            restorable,
            HintContext::default(),
            HintContext {
                alive: true,
                ..HintContext::default()
            },
        ] {
            assert!(header_hints(&context).len() <= 4);
            // Every drawn chord but `^A ?` itself resolves to a hit id of its
            // own, so a click can never fall through to the always-safe
            // `Help` by accident.
            for (key, _) in header_hints(&context)
                .into_iter()
                .filter(|(k, _)| *k != "^A ?")
            {
                assert_ne!(hint_id(key), HintId::Help, "{key} has no hit id of its own");
            }
        }
    }

    fn render_and_capture_text(area: Rect, draw: impl FnOnce(&mut Frame, Rect)) -> String {
        let backend = TestBackend::new(area.width, area.height);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, area)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        out
    }

    /// When errors exist, the header shows the count and the latest message
    /// in red, and it takes precedence over a stale-but-not-yet-expired
    /// notice's own text no longer being relevant -- `HeaderFacts` itself
    /// encodes the precedence (the caller only ever fills one of the two).
    #[test]
    fn header_shows_the_error_count_and_latest_message() {
        let mut facts = base_facts();
        facts.error_count = 3;
        facts.latest_error = Some("mail send: disk full".to_string());
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(text.contains("3"), "got {text}");
        assert!(
            text.contains("disksfull") || text.contains("disk full"),
            "got {text}"
        );
    }

    #[test]
    fn header_shows_no_error_segment_when_there_are_no_errors() {
        let facts = base_facts();
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(!text.contains('\u{26a0}'), "got {text}");
    }

    /// A non-error notice shows muted, the same as before this phase.
    #[test]
    fn header_shows_a_notice_when_there_are_no_errors() {
        let mut facts = base_facts();
        facts.notice = Some("spawned claude as wrk-2".to_string());
        let area = Rect::new(0, 0, 80, 1);
        let text = render_and_capture_text(area, |f, area| render_header(f, area, &facts));
        assert!(
            text.contains("spawnedclaude") || text.contains("spawned claude"),
            "got {text}"
        );
    }

    /// A very short terminal: the header renders its one line and nothing
    /// else, and every degenerate rect a resize can produce is a no-op rather
    /// than a panic.
    #[test]
    fn the_header_never_panics_on_a_degenerate_area() {
        let facts = base_facts();
        let backend = TestBackend::new(80, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 80, 0),
            Rect::new(0, 0, 0, 4),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 3, 2),
            Rect::new(0, 0, 80, 1),
        ] {
            term.draw(|f| render_header(f, area, &facts)).expect("draw");
        }
        // And every width from 0 to 200 columns, including the pathological
        // ones the flexible-middle budget cannot make room for.
        for w in 0..=200u16 {
            term.draw(|f| render_header(f, Rect::new(0, 0, w.min(80), 1), &facts))
                .expect("draw");
        }
    }

    /// The header draws its one line and nothing below it: a taller header
    /// rect (which `layout` no longer produces, but a caller could still pass)
    /// must not paint over the sidebar's own top border.
    #[test]
    fn the_header_draws_one_row_and_never_a_second() {
        let facts = base_facts();
        for height in [1u16, 2, 3] {
            let text = render_and_capture_text(Rect::new(0, 0, 80, height), |f, area| {
                render_header(f, area, &facts)
            });
            assert!(text.contains("zirv"), "got {text}");
            let below: String = text.chars().skip(80).collect();
            assert!(
                below.trim().is_empty(),
                "height {height} painted below its first row: {below:?}"
            );
        }
    }

    /// A quiet roster view: nothing collapsed beyond what the caller passes,
    /// the cursor on a session row, the viewport at the top, tick 0 and the
    /// same rot bands the rest of these tests use. `..` it to vary one field.
    fn test_roster_view(collapsed: &HashSet<String>) -> RosterView<'_> {
        RosterView {
            collapsed,
            chrome_selection: None,
            offset: 0,
            tick: 0,
            bands: (40, 70),
        }
    }

    fn sidebar_row(short: &str, harness: &str, state: RowState) -> SidebarRow {
        SidebarRow {
            role: "worker".into(),
            model: None,
            group: None,
            tree: TreePos::Flat,
            disclosure: Vec::new(),
            short: short.to_string(),
            harness: harness.to_string(),
            age_secs: Some(90),
            score: None,
            state,
            status: None,
            exit_code: None,
            attached: true,
            selected: false,
            focused: false,
            supervised: true,
            fact_state: "working".into(),
            fact_since_secs: Some(90),
            workflow: None,
            unread_mail: 0,
        }
    }

    /// A row whose composed attention status projects `projection` -- the
    /// shape phase 2's glyph column actually reads.
    fn attention_row(short: &str, projection: Projection) -> SidebarRow {
        let mut row = sidebar_row(short, "claude", RowState::Working);
        row.status = Some(status_for(projection));
        row
    }

    /// The smallest `SessionStatus` that projects `projection`. Built through
    /// the real fields rather than a fake enum so `attention::project`'s own
    /// rules (attention beats lifecycle; the unseen latch) stay the authority.
    fn status_for(projection: Projection) -> SessionStatus {
        use super::super::super::attention::Lifecycle;
        let mut status = SessionStatus {
            revision: 7,
            last_transition: 100,
            ..Default::default()
        };
        match projection {
            Projection::Working => status.lifecycle = Lifecycle::Working,
            Projection::Blocked(attention) => {
                status.lifecycle = Lifecycle::Waiting;
                status.attention = attention;
            }
            Projection::DoneUnread => {
                status.lifecycle = Lifecycle::Settled;
                status.visibility = Visibility::Unseen;
            }
            Projection::IdleSeen => status.lifecycle = Lifecycle::Settled,
            Projection::Failed => status.lifecycle = Lifecycle::Exited,
            Projection::Unknown => {}
        }
        assert_eq!(super::super::super::attention::project(&status), projection);
        status
    }

    #[test]
    fn row_state_matches_pane_state() {
        assert_eq!(row_state_for(&PaneState::Working), RowState::Working);
        assert_eq!(row_state_for(&PaneState::Idle), RowState::Idle);
        assert_eq!(row_state_for(&PaneState::Ended(0)), RowState::Dead);
        assert_eq!(row_state_for(&PaneState::Ended(1)), RowState::Dead);
    }

    #[test]
    fn glyph_char_matches_the_spec_per_state() {
        assert_eq!(glyph_char_for(Glyph::Idle, 0), "\u{25cf}");
        assert_eq!(glyph_char_for(Glyph::Failed, 0), "\u{2717}");
        assert_eq!(glyph_char_for(Glyph::Unknown, 0), "\u{00b7}");
        assert_eq!(glyph_char_for(Glyph::NeedsAction, 0), "\u{25b2}");
        assert_eq!(glyph_char_for(Glyph::DoneUnread, 0), "\u{25c6}");
    }

    /// The working glyph advances one spinner frame per tick, wrapping back
    /// to the first frame once the cycle completes.
    #[test]
    fn spinner_glyph_advances_with_the_tick_and_wraps() {
        let frames = style::tui::SPINNER_FRAMES;
        for tick in 0..frames.len() * 2 {
            assert_eq!(
                glyph_char_for(Glyph::Working, tick),
                frames[tick % frames.len()]
            );
        }
        assert_eq!(
            glyph_char_for(Glyph::Working, 0),
            glyph_char_for(Glyph::Working, frames.len())
        );
    }

    /// The plain text of one laid-out roster line -- what an operator sees on
    /// that row, styling aside.
    fn roster_line_text(roster: &RosterFrame, index: usize) -> String {
        roster.lines[index]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Every one of the six approved glyphs is reachable from a projection,
    /// each with its own shape AND its own colour -- never colour alone.
    #[test]
    fn every_projection_maps_to_its_own_glyph_shape_and_colour() {
        use super::super::super::attention::Attention;
        let cases = [
            (Projection::Working, Glyph::Working, "\u{280b}"),
            (
                Projection::Blocked(Attention::Approval),
                Glyph::NeedsAction,
                "\u{25b2}",
            ),
            (Projection::DoneUnread, Glyph::DoneUnread, "\u{25c6}"),
            (Projection::IdleSeen, Glyph::Idle, "\u{25cf}"),
            (Projection::Failed, Glyph::Failed, "\u{2717}"),
        ];
        let mut shapes = HashSet::new();
        let mut colours = Vec::new();
        for (projection, glyph, symbol) in cases {
            let row = attention_row("aaa11111", projection);
            assert_eq!(glyph_for(&row), glyph, "{projection:?}");
            assert_eq!(glyph_char_for(glyph, 0), symbol, "{projection:?}");
            shapes.insert(symbol);
            colours.push(glyph_style_for(glyph).fg);
        }
        assert_eq!(shapes.len(), 5, "each state needs its own shape");
        // Blocked/failed/done-unread/idle are four distinct hues; working is
        // the fifth. Unknown is the only glyph with no colour of its own.
        colours.sort_by_key(|c| format!("{c:?}"));
        colours.dedup();
        assert_eq!(colours.len(), 5, "each state needs its own colour");
        assert_eq!(glyph_style_for(Glyph::Unknown), Style::default());
    }

    /// A row with no cached status -- no issue #349 writer has ever recorded
    /// anything for it -- keeps exactly the phase 1 glyph. So does one whose
    /// status projects `Unknown`, which is what a missing or corrupt file
    /// loads back as: the two are indistinguishable and must render the same.
    #[test]
    fn a_row_with_no_attention_data_falls_back_to_the_pane_state() {
        for state in [
            RowState::Working,
            RowState::Idle,
            RowState::Dead,
            RowState::Unknown,
        ] {
            let mut row = sidebar_row("aaa11111", "claude", state);
            let fallback = glyph_for(&row);
            row.status = Some(status_for(Projection::Unknown));
            assert_eq!(
                glyph_for(&row),
                fallback,
                "{state:?}: an Unknown projection must read exactly like no status at all"
            );
        }
        assert_eq!(
            glyph_for(&sidebar_row("a", "claude", RowState::Working)),
            Glyph::Working
        );
        assert_eq!(
            glyph_for(&sidebar_row("a", "claude", RowState::Unknown)),
            Glyph::Unknown
        );
    }

    /// A retained ended row's EXIT CODE decides its glyph. `attention::project`
    /// maps every `Exited` lifecycle to `Failed`, so trusting the projection
    /// here would paint a worker that finished its job and exited 0 red.
    #[test]
    fn an_ended_rows_exit_code_decides_its_glyph_not_the_projection() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Dead);
        row.attached = false;
        row.status = Some(status_for(Projection::Failed));

        row.exit_code = Some(1);
        assert_eq!(glyph_for(&row), Glyph::Failed);

        // A clean exit is done-unread until the operator has seen it...
        row.exit_code = Some(0);
        let mut unseen = row.status.clone().unwrap();
        unseen.visibility = Visibility::Unseen;
        row.status = Some(unseen);
        assert_eq!(glyph_for(&row), Glyph::DoneUnread);

        // ...and idle afterwards.
        let mut seen = row.status.clone().unwrap();
        seen.visibility = Visibility::Seen;
        row.status = Some(seen);
        assert_eq!(glyph_for(&row), Glyph::Idle);
    }

    /// Frame A vs. frame B: anything needing attention takes the rollup on its
    /// own; with nothing waiting, the quiet counts show instead.
    #[test]
    fn a_rollup_reports_attention_states_alone_and_the_quiet_set_otherwise() {
        use super::super::super::attention::Attention;
        let quiet = [
            attention_row("a", Projection::Working),
            attention_row("b", Projection::Working),
            attention_row("c", Projection::IdleSeen),
        ];
        assert_eq!(
            rollup(&quiet.iter().collect::<Vec<_>>(), 0),
            format!("{}2  \u{25cf}1", style::tui::SPINNER_FRAMES[0])
        );

        let mut loud = quiet.to_vec();
        loud.push(attention_row("d", Projection::Blocked(Attention::Approval)));
        loud.push(attention_row("e", Projection::Failed));
        loud.push(attention_row("f", Projection::DoneUnread));
        assert_eq!(
            rollup(&loud.iter().collect::<Vec<_>>(), 0),
            "\u{25b2}1  \u{2717}1  \u{25c6}1",
            "one thing waiting suppresses the working/idle counts entirely"
        );

        // A roster of nothing but unobservable view-only rows still says so
        // rather than rendering an empty cluster.
        let unknown = [sidebar_row("g", "claude", RowState::Unknown)];
        assert_eq!(rollup(&unknown.iter().collect::<Vec<_>>(), 0), "\u{00b7}1");
        assert_eq!(rollup(&[], 0), "");
    }

    /// The approved 200×50 frame A, exactly: a work group of four whose lead
    /// is selected and waiting for approval, with the orchestrator flat above
    /// it. Asserts the summary line, the group header's rollup and the `▲`
    /// row against the 44-column contract, character for character.
    #[test]
    fn the_attention_heavy_reference_frame_matches_the_column_contract() {
        use super::super::super::attention::Attention;
        let group = GroupRef {
            id: "g".into(),
            scope: "audit".into(),
            lead_short: "a0000002".into(),
        };
        let member = |short: &str, role: &str, model: &str, age: u64, score: u32, p: Projection| {
            SidebarRow {
                role: role.into(),
                model: Some(model.into()),
                group: Some(group.clone()),
                age_secs: Some(age),
                score: Some(score),
                ..attention_row(short, p)
            }
        };
        let mut rows = vec![SidebarRow {
            role: "orch".into(),
            model: Some("fable".into()),
            age_secs: Some(840),
            score: Some(12),
            ..attention_row("a0000001", Projection::Working)
        }];
        rows.push(member(
            "a0000002",
            "sub-orch",
            "gpt-6-astra",
            540,
            21,
            Projection::Blocked(Attention::Approval),
        ));
        rows.push(member(
            "a0000003",
            "worker",
            "sonnet",
            420,
            8,
            Projection::Failed,
        ));
        rows.push(member(
            "a0000004",
            "worker",
            "gpt-5.6-terra",
            360,
            30,
            Projection::DoneUnread,
        ));
        rows.push(member(
            "a0000005",
            "worker",
            "haiku",
            300,
            4,
            Projection::IdleSeen,
        ));
        rows[1].selected = true;

        let nothing_collapsed = HashSet::new();
        let roster = roster_frame(
            Rect::new(0, 0, 44, 50),
            &rows,
            &test_roster_view(&nothing_collapsed),
        );

        // Line 0 is the ungrouped orchestrator: spawn order is never
        // re-sorted by attention, so the group header sits at its first
        // member's own position, on line 1. Dash refresh PR1: no summary
        // line in front of it any more (that title lives in its own rect
        // now, `DashLayout::sidebar_title`/`render_sidebar_title`) -- `name`
        // is `orch` (the orchestrator), `harness` and `rot` fill the rest
        // of the 44-column row (`name` widens to 26 at this width), and the
        // badge column is blank.
        assert_eq!(
            roster_line_text(&roster, 0),
            format!(
                " {} orch{}claude \u{273b}12    ",
                style::tui::SPINNER_FRAMES[0],
                " ".repeat(23)
            )
        );

        // The group header keeps its own rollup, over its members only --
        // `▾ {scope}` alone now, no lead/worker-count text.
        let counts = "\u{25b2}1  \u{2717}1  \u{25c6}1";
        let header = "\u{25be} audit";
        assert_eq!(
            roster_line_text(&roster, 1),
            format!(
                "{header}{}{counts}",
                " ".repeat(44 - style::display_width(header) - style::display_width(counts))
            )
        );

        // And the `▲` row itself, column for column: tree(1) glyph(1) sp
        // name(26) sp harness(6) sp rot(3) sp badge(2) sp.
        assert_eq!(
            roster_line_text(&roster, 2),
            format!(
                "\u{251c}\u{25b2} a0000002{} claude \u{273b}21    ",
                " ".repeat(18)
            )
        );
        assert_eq!(style::display_width(&roster_line_text(&roster, 2)), 44);
        // Selected: its own fact-block line 1 follows immediately, indented
        // under the group's own tree continuation (this member is not the
        // group's last, so the fact hangs off a `│` rather than plain
        // indentation).
        assert!(
            roster_line_text(&roster, 3).contains("working \u{b7}"),
            "got {:?}",
            roster_line_text(&roster, 3)
        );
        assert!(roster_line_text(&roster, 3).starts_with('\u{2502}'));
    }

    /// Dash refresh PR1 (§B replacement): a selected row gets a subtle
    /// `Color::Indexed(236)` background rather than REVERSED, so every
    /// glyph -- the `▲` included -- KEEPS its own colour under the tint;
    /// keyboard focus still adds BOLD.
    #[test]
    fn a_selected_needs_action_row_keeps_the_glyph_colour_under_the_background() {
        use super::super::super::attention::Attention;
        let mut row = attention_row("aaa11111", Projection::Blocked(Attention::Approval));
        let unselected = sidebar_row_parts(&row, 0, 44, 40, 70);
        assert_eq!(unselected[1].content, "\u{25b2}");
        assert_eq!(unselected[1].style.fg, style::tui::warning().fg);

        row.selected = true;
        row.focused = true;
        let selected = sidebar_row_parts(&row, 0, 44, 40, 70);
        assert_eq!(selected[1].content, "\u{25b2}", "the shape never changes");
        assert_eq!(
            selected[1].style.fg,
            style::tui::warning().fg,
            "a selected row's glyph keeps its own colour (dash refresh PR1, §B replacement)"
        );
        for span in &selected {
            assert_eq!(span.style.bg, Some(Color::Indexed(236)));
            assert!(!span.style.add_modifier.contains(Modifier::REVERSED));
            assert!(span.style.add_modifier.contains(Modifier::BOLD));
        }
    }

    /// Every projection, flat and inside a group, at all three reference
    /// widths: the row draws its own glyph, fills the sidebar exactly, and
    /// nothing panics at any of them.
    #[test]
    fn every_projection_renders_flat_and_grouped_at_every_reference_size() {
        use super::super::super::attention::Attention;
        let projections = [
            Projection::Working,
            Projection::Blocked(Attention::Approval),
            Projection::Blocked(Attention::None),
            Projection::DoneUnread,
            Projection::IdleSeen,
            Projection::Failed,
            Projection::Unknown,
        ];
        let group = GroupRef {
            id: "g".into(),
            scope: "audit".into(),
            lead_short: "aaa11111".into(),
        };
        for (cols, rows_high) in [(80u16, 20u16), (120, 40), (200, 50)] {
            let sidebar = Rect::new(0, 0, 44.min(cols), rows_high);
            for projection in projections {
                for grouped in [false, true] {
                    let mut row = attention_row("aaa11111", projection);
                    if grouped {
                        row.group = Some(group.clone());
                    }
                    row.selected = true;
                    let collapsed = HashSet::new();
                    let roster = roster_frame(
                        sidebar,
                        std::slice::from_ref(&row),
                        &test_roster_view(&collapsed),
                    );
                    // Dash refresh PR1: no summary line in front of the tree
                    // any more, so a grouped row is now at line 1 (behind
                    // its own group header) and a flat one at line 0.
                    let line = roster_line_text(&roster, if grouped { 1 } else { 0 });
                    assert_eq!(
                        style::display_width(&line),
                        sidebar.width as usize,
                        "{cols}x{rows_high} {projection:?} grouped={grouped}: {line:?}"
                    );
                    let expected = glyph_char_for(glyph_for(&row), 0);
                    assert!(
                        line.contains(expected),
                        "{cols}x{rows_high} {projection:?} grouped={grouped}: \
                         {line:?} is missing {expected}"
                    );
                }
            }
        }
    }

    /// Dash refresh PR1's 28-column row contract: `name` (the short id, not
    /// `orch`, since this row's role is `worker`), `harness`, and the row
    /// starts with the flat tree column (1 char) plus the idle glyph.
    #[test]
    fn a_sidebar_row_renders_name_and_harness() {
        let row = sidebar_row("aaa11111", "claude", RowState::Idle);
        let text = sidebar_row_text(&row, 0, 200);
        assert!(text.contains("aaa11111"), "got {text}");
        assert!(text.contains("claude"), "got {text}");
        assert!(text.starts_with(" \u{25cf}"), "got {text}");
    }

    /// The orchestrator's own row shows `orch`, never its short id.
    #[test]
    fn the_orchestrator_row_shows_orch_not_its_short_id() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Working);
        row.role = "orch".into();
        let text = sidebar_row_text(&row, 0, 200);
        assert!(text.contains("orch"), "got {text}");
        assert!(!text.contains("aaa11111"), "got {text}");
    }

    /// Dash refresh PR1: `age_secs` no longer has a column of its own in the
    /// row -- the fact block's own line 1 (`{state} · {age}`) carries the
    /// elapsed time now, and renders the shared placeholder rather than a
    /// fabricated `0s` when nothing has ever been recorded.
    #[test]
    fn the_fact_block_shows_the_placeholder_with_no_recorded_age() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Idle);
        row.selected = true;
        row.fact_since_secs = None;
        let nothing_collapsed = HashSet::new();
        let roster = roster_frame(
            Rect::new(0, 0, 28, 6),
            std::slice::from_ref(&row),
            &test_roster_view(&nothing_collapsed),
        );
        let fact = roster_line_text(&roster, 1);
        assert!(fact.contains(style::PLACEHOLDER), "got {fact:?}");
    }

    /// `dash.sidebar_cols` is configurable and a terminal can be narrower than
    /// it, so the row text has to survive any width -- including the 0..=3
    /// that `Block::inner` saturates to nothing at. The release profile is
    /// `panic = "abort"`, so an underflow here takes the terminal with it.
    #[test]
    fn a_sidebar_row_degrades_gracefully_at_every_width() {
        let row = sidebar_row("aaa11111", "claude", RowState::Working);
        for cols in 0..=64u16 {
            let text = sidebar_row_text(&row, 3, cols);
            assert!(
                style::display_width(&text) <= cols as usize,
                "width {cols} produced {text:?}"
            );
        }
        assert_eq!(sidebar_row_text(&row, 0, 0), "");
    }

    /// And the renderer itself never panics on those widths, with the
    /// scrolling window and the selection intact underneath.
    #[test]
    fn the_sidebar_renders_rows_at_every_column_width() {
        let mut rows: Vec<SidebarRow> = (0..8)
            .map(|i| {
                sidebar_row(
                    &format!("sess{i:04}"),
                    "claude",
                    if i % 2 == 0 {
                        RowState::Working
                    } else {
                        RowState::Idle
                    },
                )
            })
            .collect();
        rows[6].selected = true;
        let backend = TestBackend::new(40, 6);
        let mut term = Terminal::new(backend).expect("terminal");
        for width in 0..=40u16 {
            term.draw(|f| render_sidebar(f, Rect::new(0, 0, width, 6), &rows, 0, 40, 60))
                .expect("draw");
        }
        let text = render_and_capture_text(Rect::new(0, 0, 40, 6), |f, area| {
            render_sidebar(f, area, &rows, 0, 40, 60)
        });
        assert!(text.contains("sess0006"), "got {text}");
    }

    /// F7: the sidebar cursor may walk onto a live session this dashboard did
    /// not spawn, but the keyboard cannot follow it there. Dimmed, so that
    /// reads as "not attached" instead of as arrow navigation having silently
    /// failed -- which is how it was reported.
    #[test]
    fn view_only_sidebar_rows_are_dimmed_so_an_unfocusable_row_looks_it() {
        let rows = vec![
            SidebarRow {
                role: "worker".into(),
                model: None,
                group: None,
                tree: TreePos::Flat,
                disclosure: Vec::new(),
                short: "aaa11111".to_string(),
                harness: "claude".to_string(),
                age_secs: Some(5),
                score: None,
                state: RowState::Working,
                status: None,
                exit_code: None,
                attached: true,
                selected: false,
                focused: true,
                supervised: true,
                fact_state: "working".into(),
                fact_since_secs: Some(5),
                workflow: None,
                unread_mail: 0,
            },
            SidebarRow {
                role: "worker".into(),
                model: None,
                group: None,
                tree: TreePos::Flat,
                disclosure: Vec::new(),
                short: "bbb22222".to_string(),
                harness: "codex".to_string(),
                age_secs: Some(5),
                score: None,
                state: RowState::Unknown,
                status: None,
                exit_code: None,
                attached: false,
                selected: true,
                focused: false,
                supervised: true,
                fact_state: "unknown".into(),
                fact_since_secs: Some(5),
                workflow: None,
                unread_mail: 0,
            },
        ];
        let backend = TestBackend::new(40, 6);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 6), &rows, 0, 40, 60))
            .expect("draw");
        let buf = term.backend().buffer();
        // Issue #209/v3 §A4: the sidebar no longer draws its own box, so row
        // 0 is the frame's own first row (`y = 0`), not one row in from a
        // border that no longer exists.
        assert!(
            !buf[(2, 0)].modifier.contains(Modifier::DIM),
            "an attached pane row is drawn at full strength"
        );
        assert!(
            buf[(2, 1)].modifier.contains(Modifier::DIM),
            "a view-only row is dimmed"
        );
        assert_eq!(
            buf[(2, 1)].bg,
            Color::Indexed(236),
            "and still carries the selection highlight the cursor put on it"
        );
    }

    /// A long session list scrolls the window to keep the selected row on
    /// screen. Issue #209/v3 §A4: the sidebar no longer has a title to say
    /// which window this is (the box that title lived in is gone), so this
    /// only exercises the scrolling itself now.
    #[test]
    fn a_long_session_list_scrolls_to_keep_the_selected_row_on_screen() {
        let row = |i: usize, selected: bool| SidebarRow {
            role: "worker".into(),
            model: None,
            group: None,
            tree: TreePos::Flat,
            disclosure: Vec::new(),
            short: format!("sess{i:04}"),
            harness: String::new(),
            age_secs: None,
            score: None,
            state: RowState::Idle,
            status: None,
            exit_code: None,
            attached: true,
            selected,
            focused: false,
            supervised: true,
            fact_state: "idle".into(),
            fact_since_secs: None,
            workflow: None,
            unread_mail: 0,
        };
        let rows: Vec<SidebarRow> = (0..12).map(|i| row(i, i == 10)).collect();
        let text = render_and_capture_text(Rect::new(0, 0, 30, 6), |f, area| {
            render_sidebar(f, area, &rows, 0, 40, 60)
        });
        assert!(
            text.contains("sess0010"),
            "the selected row must be on screen: {text}"
        );
        assert!(
            !text.contains("sess0000"),
            "and the window has scrolled off the top: {text}"
        );

        let rows: Vec<SidebarRow> = (0..3).map(|i| row(i, i == 0)).collect();
        let text = render_and_capture_text(Rect::new(0, 0, 30, 6), |f, area| {
            render_sidebar(f, area, &rows, 0, 40, 60)
        });
        assert!(
            text.contains("sess0000") && text.contains("sess0002"),
            "{text}"
        );
    }

    /// Issue #354: the roster viewport and the selection are separate state
    /// now. The wheel moves `offset` freely; a keyboard navigation re-reveals
    /// the selection through here, moving the *minimum* distance rather than
    /// snapping the cursor to an edge, and the offset can never run past the
    /// last screenful.
    #[test]
    fn reveal_offset_moves_the_minimum_distance_and_never_runs_off_the_list() {
        // Everything fits, or nothing does: the viewport stays at the top.
        assert_eq!(reveal_offset(3, 10, 2, 7), 0);
        assert_eq!(reveal_offset(30, 0, 12, 4), 0);
        // Already visible: a wheel-scrolled viewport is left exactly where
        // the operator put it.
        assert_eq!(reveal_offset(30, 10, 12, 8), 8);
        // Above the window: scroll up just far enough to show it.
        assert_eq!(reveal_offset(30, 10, 3, 8), 3);
        // Below the window: scroll down just far enough, so the revealed
        // entry lands on the last visible line rather than the first.
        assert_eq!(reveal_offset(30, 10, 21, 8), 12);
        // The end of the list is as far as it goes, from either direction.
        assert_eq!(reveal_offset(30, 10, 29, 29), 20);
        assert_eq!(reveal_offset(30, 10, 25, 99), 20);
    }

    /// A sidebar with no room for a single row -- a two-row terminal, or a
    /// `dash.sidebar_cols` wider than the terminal -- must draw nothing rather
    /// than underflow.
    #[test]
    fn the_sidebar_never_panics_on_a_column_with_no_room() {
        let rows: Vec<SidebarRow> = (0..5)
            .map(|i| sidebar_row(&format!("s{i}"), "t", RowState::Idle))
            .collect();
        let backend = TestBackend::new(30, 8);
        let mut term = Terminal::new(backend).expect("terminal");
        for area in [
            Rect::new(0, 0, 30, 0),
            Rect::new(0, 0, 30, 1),
            Rect::new(0, 0, 30, 2),
            Rect::new(0, 0, 30, 3),
            Rect::new(0, 0, 0, 8),
            Rect::new(0, 0, 1, 1),
        ] {
            term.draw(|f| render_sidebar(f, area, &rows, 0, 40, 60))
                .expect("draw");
        }
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 8), &[], 0, 40, 60))
            .expect("draw");
    }

    // Issue #209/v3 §B: the selected-row REVERSED-over-colored-glyph bug fix.

    /// A selected row's own state glyph (cyan for `Working`, in this case)
    /// Dash refresh PR1 (§B replacement): a selected row's state glyph keeps
    /// its own colour under a subtle `Color::Indexed(236)` background --
    /// REVERSED is no longer used for the selection at all.
    #[test]
    fn a_selected_rows_state_glyph_keeps_its_own_fg() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Working);
        row.selected = true;
        let backend = TestBackend::new(40, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 4), &[row], 0, 40, 60))
            .expect("draw");
        let buf = term.backend().buffer();
        // Tree(1) then glyph -- column 1, not 2 (dash refresh PR1 narrowed
        // the tree prefix to one column).
        let glyph_cell = &buf[(1, 0)];
        assert_eq!(
            glyph_cell.fg,
            style::tui::accent().fg.expect("accent has a fg"),
            "a selected row's glyph keeps its own colour, got {:?}",
            glyph_cell.fg
        );
        assert_eq!(glyph_cell.bg, Color::Indexed(236));
        assert!(!glyph_cell.modifier.contains(Modifier::REVERSED));
    }

    /// The same fix, for a selected row whose rot glyph is coloured
    /// (rotting, red-bold): it keeps that colour too, under the same tint.
    #[test]
    fn a_selected_rows_rot_glyph_keeps_its_own_fg_too() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Idle);
        row.score = Some(90); // well past any default compact_at threshold
        row.selected = true;
        let backend = TestBackend::new(40, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 4), &[row], 0, 40, 60))
            .expect("draw");
        let buf = term.backend().buffer();
        for x in 0..40u16 {
            let cell = &buf[(x, 0)];
            assert_eq!(
                cell.bg,
                Color::Indexed(236),
                "every cell in a selected row carries the tint, got {:?} at column {x}",
                cell.bg
            );
        }
        // The rot column (tree(1) glyph(1) sp(1) name(N) sp(1) harness(6)
        // sp(1) -- rot starts right after) keeps its own red colour.
        let name_width = 40usize.saturating_sub(SIDEBAR_FIXED_COLS);
        let rot_x = (1 + 1 + 1 + name_width + 1 + 6 + 1) as u16;
        assert_eq!(
            buf[(rot_x, 0)].fg,
            style::tui::error().fg.expect("error has a fg")
        );
    }

    /// The list-dialog primitive's own cursor row has the identical defect
    /// with `ListDialogRow::glyph` (the QuitConfirm spinner, in practice):
    /// its glyph must not carry its own colour once reversed either.
    #[test]
    fn a_list_dialog_cursor_rows_glyph_carries_no_explicit_fg() {
        let spec = ListDialogSpec {
            title: "quit".to_string(),
            count: None,
            rows: vec![ListDialogRow {
                text: "wrk claude".to_string(),
                checked: None,
                glyph: Some(("\u{2807}".to_string(), style::tui::accent())),
                reason: None,
                dim: false,
            }],
            cursor: Some(0),
            offset: 0,
            footer: &[],
            warn: false,
            empty_message: "",
            input: None,
        };
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_list_dialog(f, f.area(), &spec))
            .expect("draw");
        let buf = term.backend().buffer();
        let rect = centered(Rect::new(0, 0, 40, 10), dialog_width(40), 5);
        // The cursor row is the first content row inside the frame's border
        // and horizontal padding.
        let row_y = rect.y + 1;
        let glyph_x = rect.x + 2;
        let cell = &buf[(glyph_x, row_y)];
        assert_eq!(
            cell.fg,
            Color::Reset,
            "the cursor row's glyph must carry no explicit fg colour, got {:?}",
            cell.fg
        );
        assert!(cell.modifier.contains(Modifier::REVERSED));
    }

    // Issue #209/v3 §C: the sidebar rot column.

    #[test]
    fn rot_band_thresholds_match_advise_and_compact_at() {
        assert_eq!(rot_band_for(0, 40, 60), RotBand::Fresh);
        assert_eq!(rot_band_for(39, 40, 60), RotBand::Fresh);
        assert_eq!(rot_band_for(40, 40, 60), RotBand::Warming);
        assert_eq!(rot_band_for(59, 40, 60), RotBand::Warming);
        assert_eq!(rot_band_for(60, 40, 60), RotBand::Rotting);
        assert_eq!(rot_band_for(100, 40, 60), RotBand::Rotting);
    }

    /// A fresh score renders in the sidebar with no colour of its own --
    /// it inherits the row's tone (here: none, since the row is neither
    /// dim nor focused nor selected).
    #[test]
    fn a_fresh_sidebar_score_inherits_the_row_tone() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Idle);
        row.score = Some(12);
        let text = sidebar_row_text(&row, 0, 40);
        assert!(text.contains("\u{273b}12"), "got {text:?}");
        let backend = TestBackend::new(40, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 4), &[row], 0, 40, 60))
            .expect("draw");
        let buf = term.backend().buffer();
        // `⠹ aaa11111 claude ✻12  1m` -- the rot glyph sits right after the
        // harness; scanning the row for it directly keeps this independent
        // of exact column math.
        let rot_col = (0..40u16)
            .find(|&x| buf[(x, 0)].symbol() == "\u{273b}")
            .expect("rot glyph must be drawn somewhere on the row");
        assert_eq!(
            buf[(rot_col, 0)].fg,
            Color::Reset,
            "fresh must carry no colour of its own"
        );
    }

    #[test]
    fn a_warming_sidebar_score_is_yellow() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Idle);
        row.score = Some(47);
        let backend = TestBackend::new(40, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 4), &[row], 0, 40, 60))
            .expect("draw");
        let buf = term.backend().buffer();
        let rot_col = (0..40u16)
            .find(|&x| buf[(x, 0)].symbol() == "\u{273b}")
            .expect("rot glyph must be drawn");
        assert_eq!(buf[(rot_col, 0)].fg, Color::Yellow);
    }

    #[test]
    fn a_rotting_sidebar_score_is_red_and_bold() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Idle);
        row.score = Some(81);
        let backend = TestBackend::new(40, 4);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 4), &[row], 0, 40, 60))
            .expect("draw");
        let buf = term.backend().buffer();
        let rot_col = (0..40u16)
            .find(|&x| buf[(x, 0)].symbol() == "\u{273b}")
            .expect("rot glyph must be drawn");
        let cell = &buf[(rot_col, 0)];
        assert_eq!(cell.fg, Color::Red);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    /// A dead pane always shows the placeholder, even with a stale score
    /// still cached from before it exited -- a dead pane's last reading is
    /// stale, not a live verdict.
    #[test]
    fn a_dead_pane_shows_the_placeholder_regardless_of_a_cached_score() {
        let mut row = sidebar_row("aaa11111", "codex", RowState::Dead);
        row.score = Some(12);
        let text = sidebar_row_text(&row, 0, 40);
        // Dash refresh PR1: the rot column is always `✻NN` or `✻ {placeholder}`
        // (never the placeholder alone) -- a dead pane's stale cached score
        // still reads as unknown, but the glyph itself is part of the
        // 3-column contract regardless.
        assert!(
            text.contains(&format!("{ROT_GLYPH} {}", style::PLACEHOLDER)),
            "dead pane must show the rot glyph plus the placeholder: {text:?}"
        );
        assert!(
            !text.contains("\u{273b}12"),
            "never the stale score: {text:?}"
        );
    }

    /// No cached score at all (an idle/working pane whose transcript has not
    /// resolved yet) also shows the placeholder, never a fabricated zero.
    #[test]
    fn no_cached_score_shows_the_placeholder() {
        let row = sidebar_row("aaa11111", "claude", RowState::Idle);
        let text = sidebar_row_text(&row, 0, 40);
        assert!(text.contains(style::PLACEHOLDER), "got {text:?}");
    }

    /// Issue #354's original §C ladder is gone (fixed columns instead of a
    /// width-degradation order), and dash refresh PR1 keeps that -- but
    /// unlike the old 44-column contract (where the ONE widening column,
    /// `model`, was the last field, so a narrower row really was a strict
    /// left-hand prefix of the wider one), the new contract's widening
    /// column (`name`) sits in the MIDDLE, before `harness`/`rot`/`badge`.
    /// So narrowing the sidebar no longer just clips a tail -- it also
    /// shifts every column after `name` left as `name` itself shrinks. That
    /// is by design (`name` tracks `dash.sidebar_cols` itself, not a
    /// per-frame width squeeze: below 100 total columns the sidebar hides
    /// outright rather than rendering squeezed). What still must hold at
    /// any width is safety: the row never overflows its own budget.
    #[test]
    fn sidebar_fixed_columns_never_overflow_their_own_width() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Idle);
        row.score = Some(47);
        row.role = "sub-orch".to_string();
        // The full contract row: every column present, in order -- dash
        // refresh PR1's `tree(1) glyph(1) sp name(N) sp harness(6) sp
        // rot(3) sp badge(2) sp`, `name` widening to 26 at this width
        // (`role` is `sub-orch`, so `name` is the short id, never `orch`).
        let full = sidebar_row_text(&row, 0, 44);
        assert_eq!(style::display_width(&full), 44, "got {full:?}");
        for (column, at) in [("aaa11111", 3), ("claude", 30), ("\u{273b}47", 37)] {
            assert_eq!(
                full.chars()
                    .skip(at)
                    .take(column.chars().count())
                    .collect::<String>(),
                column,
                "column {column:?} moved off {at} in {full:?}"
            );
        }
        for cols in 0..=44u16 {
            let text = sidebar_row_text(&row, 0, cols);
            assert!(
                style::display_width(&text) <= cols as usize,
                "{cols} cols overflowed: {text:?}"
            );
        }
    }

    /// Every sidebar row, at every width, never exceeds the column budget --
    /// the same invariant `a_sidebar_row_degrades_gracefully_at_every_width`
    /// already holds for the pre-rot-column shape, extended to a row that
    /// actually carries a score.
    #[test]
    fn a_sidebar_row_with_a_score_degrades_gracefully_at_every_width() {
        let mut row = sidebar_row("aaa11111", "claude", RowState::Working);
        row.score = Some(47);
        for cols in 0..=64u16 {
            let text = sidebar_row_text(&row, 3, cols);
            assert!(
                style::display_width(&text) <= cols as usize,
                "width {cols} produced {text:?}"
            );
        }
    }

    // Issue #209/v3 §D: the footer signal row.

    fn alive_footer_facts() -> FooterAliveFacts {
        FooterAliveFacts {
            score: Some(12),
            unread_mail: 0,
            supervised: true,
            stalled: false,
        }
    }

    /// Dash refresh PR1: the healthy footer is just the fresh verdict
    /// (`✻ NN {band}`) and the dim mail placeholder -- harness, usage and
    /// workflow moved to the pane header, and a healthy, reachable,
    /// unstalled pane renders NO supervision segment at all any more.
    #[test]
    fn footer_renders_the_healthy_example() {
        let facts = FooterFacts::Alive(alive_footer_facts());
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains("fresh"), "got {text:?}");
        assert!(text.contains("12"), "got {text:?}");
        assert!(
            text.contains(style::PLACEHOLDER),
            "dim mail placeholder: got {text:?}"
        );
        assert!(
            !text.contains("supervised"),
            "a healthy, reachable, unstalled pane shows no supervision segment at all: got {text:?}"
        );
        assert!(!text.contains("stalled"), "got {text:?}");
    }

    /// Issue #310: an armed stall latch overrides the (now absent) healthy
    /// segment with a `stalled` badge.
    #[test]
    fn footer_renders_a_stalled_badge_when_the_latch_is_armed() {
        let mut alive = alive_footer_facts();
        alive.stalled = true;
        let facts = FooterFacts::Alive(alive);
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains("stalled"), "got {text:?}");
    }

    /// An unsupervised (turn-signal bind failed) pane still renders the
    /// problem, even though a healthy one now renders nothing.
    #[test]
    fn footer_renders_unsupervised_when_the_pane_never_bound_a_signal() {
        let mut alive = alive_footer_facts();
        alive.supervised = false;
        let facts = FooterFacts::Alive(alive);
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains("unsupervised"), "got {text:?}");
    }

    /// The mock's own "attention" example: warming, with unread mail.
    #[test]
    fn footer_renders_the_attention_example() {
        let facts = FooterFacts::Alive(FooterAliveFacts {
            score: Some(47),
            unread_mail: 2,
            supervised: true,
            stalled: false,
        });
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains("warming"), "got {text:?}");
        assert!(text.contains('2'), "unread mail count: got {text:?}");
    }

    /// A rotting verdict, and a session with no unread mail (the dim
    /// placeholder, never a literal `0`).
    #[test]
    fn footer_renders_a_rotting_verdict_and_dim_placeholder_mail() {
        let mut alive = alive_footer_facts();
        alive.score = Some(90);
        alive.unread_mail = 0;
        let facts = FooterFacts::Alive(alive);
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains("rotting"), "got {text:?}");
        assert!(text.contains(style::PLACEHOLDER), "got {text:?}");
    }

    /// The mock's own "dead pane focused" example: a different message
    /// entirely -- exited-age and the restore hint, not the alive shape.
    #[test]
    fn footer_renders_the_dead_pane_example() {
        let facts = FooterFacts::Dead(FooterDeadFacts {
            harness: "codex".to_string(),
            exited_age_secs: Some(12 * 60),
            workflow: FooterWorkflow::Active {
                kind: "feature".to_string(),
                step: "implement".to_string(),
                gated: false,
            },
        });
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(text.contains("codex"), "got {text:?}");
        assert!(text.contains("exited"), "got {text:?}");
        assert!(text.contains("12m"), "got {text:?}");
        assert!(text.contains("feature"), "got {text:?}");
        assert!(text.contains("implement"), "got {text:?}");
        assert!(text.contains("restore"), "got {text:?}");
    }

    /// `FooterFacts::None` (nothing focused at all) draws nothing.
    #[test]
    fn footer_draws_nothing_when_nothing_is_focused() {
        let text = render_and_capture_text(Rect::new(0, 0, 80, 1), |f, area| {
            render_footer(f, area, &FooterFacts::None, 40, 60)
        });
        assert!(text.trim().is_empty(), "got {text:?}");
    }

    /// §D's own workflow-segment states, at the pure `footer_workflow_spans`
    /// level: no active workflow, an ungated step (dim), and a gated step
    /// (yellow-bold with "awaits approval"/"!" wording).
    #[test]
    fn footer_workflow_segment_states() {
        let (full, compressed) = footer_workflow_spans(&FooterWorkflow::None);
        let joined = |seg: &FooterSeg| seg.iter().map(|(t, _)| t.as_str()).collect::<String>();
        assert!(joined(&full).contains(style::PLACEHOLDER));
        assert_eq!(full, compressed);

        let (full, compressed) = footer_workflow_spans(&FooterWorkflow::Active {
            kind: "feature".to_string(),
            step: "design".to_string(),
            gated: false,
        });
        assert!(joined(&full).contains("feature"));
        assert!(joined(&full).contains("design"));
        assert!(joined(&compressed).contains("design"));
        assert!(!joined(&compressed).contains("feature"));
        for (_, style) in full.iter().chain(compressed.iter()) {
            assert!(
                !style.add_modifier.contains(Modifier::BOLD),
                "an ungated workflow segment must not be bold"
            );
        }

        let (full, compressed) = footer_workflow_spans(&FooterWorkflow::Active {
            kind: "feature".to_string(),
            step: "spec".to_string(),
            gated: true,
        });
        assert!(joined(&full).contains("spec awaits approval"));
        assert!(joined(&compressed).contains("spec!"));
        for (_, style) in full.iter().chain(compressed.iter()) {
            assert_eq!(style.fg, Some(Color::Yellow));
            assert!(style.add_modifier.contains(Modifier::BOLD));
        }
    }

    /// Dash refresh PR1's own (much shorter) drop order: only the verdict's
    /// score number is ever dropped under width pressure -- the word, mail
    /// and supervision (when there is a problem to show at all) never drop.
    #[test]
    fn footer_drop_order_matches_the_spec() {
        let mut alive = alive_footer_facts();
        alive.stalled = true;
        let facts = FooterFacts::Alive(alive);
        let render = |cols: u16| {
            render_and_capture_text(Rect::new(0, 0, cols, 1), |f, area| {
                render_footer(f, area, &facts, 40, 60)
            })
        };

        let wide = render(80);
        assert!(
            wide.contains("12"),
            "the score number shows at 80 cols: {wide:?}"
        );
        assert!(wide.contains("stalled"), "got {wide:?}");

        let mut saw_score_drop = false;
        for cols in (0..=80u16).rev() {
            let text = render(cols);
            if !text.contains("12") && !saw_score_drop {
                saw_score_drop = true;
                // Once the score number is gone, the irreducible core must
                // survive down to whatever `cols` can still hold.
                if cols >= 15 {
                    assert!(
                        text.contains("fresh"),
                        "verdict word must survive at {cols} cols: {text:?}"
                    );
                    assert!(
                        text.contains("stalled"),
                        "supervision must survive at {cols} cols: {text:?}"
                    );
                }
                break;
            }
        }
        assert!(saw_score_drop, "the score number must eventually drop");
    }

    /// The dead-pane footer's own drop order: the exited notice and the
    /// restore hint are never dropped, even at an absurdly narrow width.
    #[test]
    fn footer_dead_pane_never_drops_the_exited_notice_or_restore_hint() {
        let facts = FooterFacts::Dead(FooterDeadFacts {
            harness: "codex".to_string(),
            exited_age_secs: Some(720),
            workflow: FooterWorkflow::None,
        });
        // Tight but not absurd: the harness and workflow segment have
        // dropped, but the exited notice and restore hint -- the one thing
        // this state exists to tell the operator -- still fit and survive.
        let tight = render_and_capture_text(Rect::new(0, 0, 35, 1), |f, area| {
            render_footer(f, area, &facts, 40, 60)
        });
        assert!(tight.contains("exited"), "got {tight:?}");
        assert!(tight.contains("restore"), "got {tight:?}");
        assert!(
            !tight.contains("codex"),
            "harness must have dropped by 30 cols: {tight:?}"
        );

        // And at every width, including absurdly narrow ones, the row never
        // exceeds its own column budget -- the release profile is `panic =
        // "abort"`, so an overflow here would take the operator's terminal
        // with it.
        for cols in [80u16, 40, 20, 10, 1, 0] {
            let text = render_and_capture_text(Rect::new(0, 0, cols, 1), |f, area| {
                render_footer(f, area, &facts, 40, 60)
            });
            assert!(
                style::display_width(&text) <= cols as usize,
                "width {cols} produced {text:?}"
            );
        }
    }

    #[test]
    fn render_overlay_quit_confirm_lists_working_panes() {
        let overlay = Overlay::QuitConfirm(vec!["wrk claude".to_string(), "wrk codex".to_string()]);
        let area = Rect::new(0, 0, 40, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(text.contains("wrkclaude") || text.contains("wrk claude"));
    }

    #[test]
    fn mail_dialog_shows_the_selected_message_and_the_key_hints() {
        let view = MailView {
            items: vec![(
                PathBuf::from("/mail/1.md"),
                "claude".to_string(),
                "the webhook route moved".to_string(),
            )],
            cursor: 0,
            offset: 0,
            compose: None,
        };
        let overlay = Overlay::Mail(view);
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(text.contains("claude"), "got {text}");
        assert!(
            text.contains("webhookroute") || text.contains("webhook route"),
            "got {text}"
        );
    }

    #[test]
    fn mail_dialog_shows_the_compose_draft_when_composing() {
        let view = MailView {
            items: Vec::new(),
            cursor: 0,
            offset: 0,
            compose: Some(ComposeDraft {
                to: String::new(),
                body: "heads up".to_string(),
            }),
        };
        let overlay = Overlay::Mail(view);
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(
            text.contains("headsup") || text.contains("heads up"),
            "got {text}"
        );
    }

    #[test]
    fn memory_dialog_lists_entries_with_key_and_age() {
        let view = MemoryView {
            entries: vec![(
                "build-cmd".to_string(),
                "written 3d ago, verified 1d ago".to_string(),
                "cargo build --release".to_string(),
            )],
            cursor: 0,
            offset: 0,
            input: None,
        };
        let overlay = Overlay::Memory(view);
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(text.contains("build-cmd"), "got {text}");
    }

    /// D1: the dialog names the pane by the same short id the nudge is
    /// resolved against at Enter time, not by a position that can shift.
    #[test]
    fn nudge_dialog_names_an_attached_pane_target() {
        let draft = NudgeDraft {
            target: NudgeTarget::AttachedPane("bbbb2222".to_string()),
            input: "hello".to_string(),
        };
        let overlay = Overlay::Nudge(draft);
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(
            text.contains("panebbbb2222") || text.contains("pane bbbb2222"),
            "got {text}"
        );
    }

    #[test]
    fn restore_dialog_shows_checked_and_unchecked_entries() {
        let view = RestoreView {
            entries: vec![
                RestoreEntry {
                    label: "wrk claude (aaaa1111)".to_string(),
                    checked: true,
                },
                RestoreEntry {
                    label: "wrk codex (bbbb2222)".to_string(),
                    checked: false,
                },
            ],
            cursor: 0,
            offset: 0,
        };
        let overlay = Overlay::Restore(view);
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(
            text.contains("[x]"),
            "checked entry missing its mark: {text}"
        );
        assert!(text.contains("[ ]"), "unchecked entry missing: {text}");
        assert!(
            text.contains("wrkclaude") || text.contains("wrk claude"),
            "got {text}"
        );
    }

    #[test]
    fn restore_dialog_on_an_empty_roster_says_so() {
        let overlay = Overlay::Restore(RestoreView::default());
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(
            text.contains("nothingtorestore") || text.contains("nothing to restore"),
            "got {text}"
        );
    }

    /// One never-repeated, never-acknowledged error -- the ordinary case
    /// every pre-phase-5 test was written against.
    fn err_item(text: &str) -> ErrorItem {
        ErrorItem {
            text: text.to_string(),
            count: 1,
            age_secs: 0,
            acked: false,
        }
    }

    #[test]
    fn errors_overlay_lists_kept_errors_newest_first_with_a_warning_glyph() {
        let view = ErrorsView {
            items: vec![
                err_item("mail send: disk full"),
                err_item("handover: timed out"),
            ],
            cursor: 0,
            offset: 0,
            mark: 0,
        };
        let overlay = Overlay::Errors(view);
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(text.contains('\u{26a0}'), "got {text}");
        assert!(
            text.contains("mailsend") || text.contains("mail send"),
            "got {text}"
        );
    }

    /// Issue #354 phase 5: a repeated error is ONE row carrying `\u{d7}n`
    /// and the age of its most recent repeat; an acknowledged one stays in
    /// the list, dim, rather than disappearing; and the footer says how to
    /// acknowledge.
    #[test]
    fn the_errors_dialog_shows_repeat_counts_ages_and_dims_acknowledged_rows() {
        let repeated = ErrorItem {
            text: "mail send: disk full".to_string(),
            count: 4,
            age_secs: 125,
            acked: false,
        };
        let acked = ErrorItem {
            text: "handover: timed out".to_string(),
            count: 1,
            age_secs: 3600,
            acked: true,
        };
        assert_eq!(
            error_dialog_row(&repeated).text,
            "\u{26a0} mail send: disk full \u{d7}4"
        );
        assert_eq!(
            error_dialog_row(&repeated).reason.as_deref(),
            Some(style::format_age(125).as_str())
        );
        assert!(!error_dialog_row(&repeated).dim);
        assert_eq!(
            error_dialog_row(&acked).text,
            "\u{26a0} handover: timed out"
        );
        assert!(
            error_dialog_row(&acked).dim,
            "an acknowledged entry is dim, never dropped"
        );
        let overlay = Overlay::Errors(ErrorsView {
            items: vec![repeated, acked],
            cursor: 0,
            offset: 0,
            mark: 0,
        });
        let text = render_and_capture_text(Rect::new(0, 0, 70, 12), |f, area| {
            render_overlay(f, area, &overlay, 0)
        });
        assert!(text.contains('\u{d7}'), "the repeat count is drawn: {text}");
        assert!(
            text.contains("acknowledge"),
            "the footer names the key: {text}"
        );
    }

    /// The sticky header line spells a repeat the same way the dialog does.
    #[test]
    fn the_sticky_error_line_carries_its_repeat_count() {
        assert_eq!(error_line("boom", 1), "boom");
        assert_eq!(error_line("boom", 3), "boom \u{d7}3");
    }

    /// Issue #354 phase 5: with the cursor on the summary line the cluster
    /// describes the dashboard -- `^A i` first, no per-session chord.
    #[test]
    fn the_summary_line_gets_its_own_header_cluster() {
        let summary = HintContext {
            summary: true,
            ..HintContext::default()
        };
        assert_eq!(
            header_hints(&summary),
            vec![
                ("^A i", "inspect"),
                ("^A c", "actions"),
                ("^A m", "mail"),
                ("^A ?", "help"),
            ]
        );
        assert!(!header_hints(&summary).iter().any(|(_, l)| *l == "nudge"));
    }

    #[test]
    fn errors_overlay_on_no_errors_says_so() {
        let overlay = Overlay::Errors(ErrorsView::default());
        let area = Rect::new(0, 0, 60, 10);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(
            text.contains("norecenterrors") || text.contains("no recent errors"),
            "got {text}"
        );
    }

    /// Every overlay variant, in a shape that has something to draw, so the
    /// degenerate-area tests below exercise the full rendering path rather
    /// than an early "nothing to show" branch.
    fn every_overlay() -> Vec<Overlay> {
        vec![
            Overlay::None,
            Overlay::QuitConfirm(vec!["wrk claude".to_string()]),
            Overlay::Spawn(SpawnDraft {
                input: "claude fix the tests".to_string(),
                items: vec!["claude".to_string()],
                cursor: 0,
            }),
            Overlay::Nudge(NudgeDraft {
                target: NudgeTarget::AttachedPane("aaaa1111".to_string()),
                input: "hello".to_string(),
            }),
            Overlay::Mail(MailView {
                items: vec![(
                    PathBuf::from("/mail/1.md"),
                    "claude".to_string(),
                    "a body".to_string(),
                )],
                cursor: 0,
                offset: 0,
                compose: Some(ComposeDraft {
                    to: "any".to_string(),
                    body: "drafting".to_string(),
                }),
            }),
            Overlay::Memory(MemoryView {
                entries: vec![("k".to_string(), "age".to_string(), "body".to_string())],
                cursor: 0,
                offset: 0,
                input: Some("typing".to_string()),
            }),
            Overlay::Restore(RestoreView {
                entries: vec![RestoreEntry {
                    label: "wrk claude (aaaa1111)".to_string(),
                    checked: true,
                }],
                cursor: 0,
                offset: 0,
            }),
            Overlay::Palette(PaletteView {
                mode: PaletteMode::Help,
                ..PaletteView::default()
            }),
            Overlay::Palette(PaletteView {
                mode: PaletteMode::Run,
                query: "na".to_string(),
                ctx: ActionContext {
                    selected: true,
                    attached: true,
                    alive: true,
                    ..ActionContext::default()
                },
                cursor: 0,
                offset: 0,
            }),
            Overlay::Errors(ErrorsView {
                items: vec![err_item("an error")],
                cursor: 0,
                offset: 0,
                mark: 0,
            }),
            // Issue #354 phase 3: the two new list-shaped overlays go through
            // every degenerate-area and opacity test the others do.
            Overlay::Menu(MenuView {
                target: "aaaa1111".to_string(),
                subject: "aaaa1111 \u{b7} worker".to_string(),
                entries: vec![
                    MenuEntry {
                        action: MenuAction::Inspect,
                        disabled: None,
                        letter: Some('i'),
                    },
                    MenuEntry {
                        action: MenuAction::Restore,
                        disabled: Some("still running".to_string()),
                        letter: Some('r'),
                    },
                ],
                cursor: 0,
                offset: 0,
                confirm: None,
            }),
            Overlay::Inspector(InspectorView {
                target: "aaaa1111".to_string(),
                subject: "aaaa1111 \u{b7} worker".to_string(),
                sections: vec![InspectorSection {
                    name: "identity".to_string(),
                    lines: vec!["short       aaaa1111".to_string()],
                }],
                cursor: 0,
                offset: 0,
            }),
        ]
    }

    /// F1: `dialog_width` used to be `saturating_sub(4).clamp(1, width)`,
    /// and `Ord::clamp` panics when `min > max` -- which is every zero-width
    /// area. A terminal narrowed to at most the sidebar width makes
    /// `layout`'s own main rect exactly that, and the release profile is
    /// `panic = "abort"`.
    #[test]
    fn dialog_width_never_panics_and_never_exceeds_the_area() {
        assert_eq!(dialog_width(0), 1);
        assert_eq!(dialog_width(1), 1);
        assert_eq!(dialog_width(4), 1);
        assert_eq!(dialog_width(5), 1);
        assert_eq!(dialog_width(40), 36);
        for w in 0..=64u16 {
            assert!(dialog_width(w) >= 1);
            assert!(dialog_width(w) <= w.max(1));
        }
    }

    #[test]
    fn dialog_row_count_saturates_instead_of_wrapping_or_overflowing() {
        // Before this existed, `rows as u16 + extra` would silently wrap on
        // a five-digit row count and, right at the u16 boundary, overflow
        // the subsequent `+` -- which panics in every debug/test build
        // (this profile has no `overflow-checks` override).
        assert_eq!(dialog_row_count(0, 4), 4);
        assert_eq!(dialog_row_count(10, 4), 14);
        assert_eq!(dialog_row_count(u16::MAX as usize, 4), u16::MAX);
        assert_eq!(dialog_row_count(100_000, 4), u16::MAX);
        assert_eq!(dialog_row_count(usize::MAX, 4), u16::MAX);
    }

    /// A pathologically large row count (tens of thousands of mail rows, say)
    /// must render without panicking and the resulting height must never
    /// exceed the area it was clamped against -- the same guarantee the
    /// small-row-count dialogs already have, just exercised at a size where
    /// the old `content_rows as u16 + 2 + 2` cast/add could wrap or overflow.
    #[test]
    fn render_list_dialog_never_panics_on_an_enormous_row_count_or_a_very_wide_row() {
        let mut rows: Vec<ListDialogRow> = (0..70_000)
            .map(|i| ListDialogRow::plain(format!("row {i}")))
            .collect();
        // A single absurdly wide row, too: `display_width` on it is far
        // larger than any real terminal, exercising the same cast risk on
        // the cursor row's own reversed-padding width math.
        rows.push(ListDialogRow::plain("x".repeat(200_000)));
        let cursor = Some(rows.len() - 1);

        let total = rows.len();
        let spec = ListDialogSpec {
            title: "mail".to_string(),
            count: Some(total),
            rows,
            cursor,
            // Issue #354 phase 3: an offset far past the end of the list, so
            // the viewport's own clamp is exercised at the same pathological
            // size the height clamp is.
            offset: usize::MAX,
            footer: MAIL_FOOTER,
            warn: false,
            empty_message: "(no mail)",
            input: None,
        };

        for area in [
            Rect::new(0, 0, 10, 3),
            Rect::new(0, 0, 40, 10),
            Rect::new(0, 0, 200, 60),
        ] {
            let backend = TestBackend::new(area.width, area.height);
            let mut term = Terminal::new(backend).expect("terminal");
            term.draw(|f| render_list_dialog(f, area, &spec))
                .expect("draw must not panic on an enormous row count");
            let buf = term.backend().buffer();
            assert_eq!(buf.area.width, area.width);
            assert_eq!(buf.area.height, area.height);
            // The shared viewport draws only what fits, and never a row
            // outside the list -- the caret is on the very last row, so the
            // window has to be the last screenful.
            let geom = list_dialog_layout(area, &spec).expect("layout");
            assert!(geom.rows.len() <= geom.capacity, "{:?}", geom.rows.len());
            assert!(
                geom.rows.iter().all(|(_, i)| *i < total),
                "a drawn row addressed past the end of the list"
            );
            if geom.capacity > 0 {
                // The first drawn row IS the clamped viewport offset, and
                // with the caret on the very last row the window has to be
                // the last screenful.
                assert_eq!(
                    geom.rows.first().map(|(_, i)| *i),
                    Some(total - geom.capacity)
                );
                assert!(
                    geom.rows.iter().any(|(_, i)| *i == total - 1),
                    "the caret row must be on screen"
                );
                assert_eq!(
                    geom.indicator,
                    Some(format!(
                        "{}\u{2013}{total} of {total}",
                        total - geom.capacity + 1
                    ))
                );
            }
        }
    }

    // -- issue #354 phase 3: the shared scrollable list viewport ------------

    /// One dialog render, captured row by row, so a test can say which line
    /// something landed on rather than only that it is somewhere on screen.
    fn capture_rows(area: Rect, draw: impl FnOnce(&mut Frame, Rect)) -> Vec<String> {
        let backend = TestBackend::new(area.width, area.height);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| draw(f, area)).expect("draw");
        let buf = term.backend().buffer();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn list_spec(rows: usize, cursor: usize, offset: usize) -> ListDialogSpec<'static> {
        ListDialogSpec {
            title: "mail".to_string(),
            count: Some(rows),
            rows: (0..rows)
                .map(|i| ListDialogRow::plain(format!("entry-{i:03}")))
                .collect(),
            cursor: (rows > 0).then_some(cursor),
            offset,
            footer: MAIL_FOOTER,
            warn: false,
            empty_message: "(no mail)",
            input: None,
        }
    }

    /// The pure scrolling reducer: paging moves by a screenful and clamps at
    /// both ends, Home/End jump, and the caret is always brought back into
    /// the window by the minimum distance.
    #[test]
    fn the_shared_list_viewport_pages_jumps_and_reveals_the_caret() {
        // 40 rows, 12 on screen.
        assert_eq!(list_move(0, 40, 12, ListMove::PageDown), 12);
        assert_eq!(list_move(12, 40, 12, ListMove::PageDown), 24);
        assert_eq!(list_move(36, 40, 12, ListMove::PageDown), 39, "clamps");
        assert_eq!(list_move(20, 40, 12, ListMove::PageUp), 8);
        assert_eq!(list_move(3, 40, 12, ListMove::PageUp), 0, "clamps");
        assert_eq!(list_move(20, 40, 12, ListMove::Home), 0);
        assert_eq!(list_move(0, 40, 12, ListMove::End), 39);
        // A dialog with no room for a single row still pages by at least one
        // entry rather than dividing by zero or standing still forever.
        assert_eq!(list_move(0, 40, 0, ListMove::PageDown), 1);
        // An empty list has nowhere to go, at any capacity.
        for mv in [
            ListMove::PageUp,
            ListMove::PageDown,
            ListMove::Home,
            ListMove::End,
        ] {
            assert_eq!(list_move(9, 0, 12, mv), 0);
        }
        // Reveal-on-move: the window follows the caret the minimum distance.
        assert_eq!(list_scroll(40, 12, 20, 0), 9);
        assert_eq!(list_scroll(40, 12, 20, 15), 15, "already visible");
        assert_eq!(list_scroll(40, 12, 2, 15), 2);
        assert_eq!(list_scroll(40, 12, 39, usize::MAX), 28, "last screenful");
        assert_eq!(list_scroll(3, 12, 2, 9), 0, "everything fits");
    }

    /// The indicator only appears when the list does not fit, and reads as
    /// the approved `first–last of total`.
    #[test]
    fn the_scroll_indicator_names_the_window_only_when_one_exists() {
        assert_eq!(scroll_indicator(0, 12, 3), None);
        assert_eq!(scroll_indicator(0, 12, 12), None);
        assert_eq!(scroll_indicator(0, 0, 40), None);
        assert_eq!(
            scroll_indicator(2, 10, 40),
            Some("3\u{2013}12 of 40".to_string())
        );
        assert_eq!(
            scroll_indicator(35, 10, 40),
            Some("36\u{2013}40 of 40".to_string())
        );
    }

    /// A 3-row and a 60-row list, at all three reference sizes: the title
    /// stays on the frame's own top border, the hint row stays on its last
    /// interior line, the caret is always drawn, and only the list that does
    /// not fit gets an indicator.
    #[test]
    fn a_list_dialog_pins_its_title_and_hint_row_at_every_size() {
        for (width, height) in [(80u16, 20u16), (120, 40), (200, 50)] {
            for total in [3usize, 60] {
                let area = Rect::new(0, 0, width, height);
                // Caret near the end, so a 60-row list has genuinely scrolled.
                let cursor = total - 1;
                let spec = list_spec(total, cursor, 0);
                let geom = list_dialog_layout(area, &spec).expect("layout");
                let lines = capture_rows(area, |f, area| render_list_dialog(f, area, &spec));

                let top = &lines[geom.rect.y as usize];
                assert!(top.contains("mail"), "{width}x{height}/{total}: {top}");
                let hint_y = geom.inner.y + geom.inner.height - 1;
                let hint = &lines[hint_y as usize];
                assert!(
                    hint.contains("read+consume"),
                    "{width}x{height}/{total}: the hint row must be pinned to the last \
                     interior line, got {hint}"
                );
                // The caret row is on screen, and is one of the rects a click
                // is tested against.
                assert!(
                    geom.rows.iter().any(|(_, i)| *i == cursor),
                    "{width}x{height}/{total}: the caret scrolled off"
                );
                let caret_y = geom
                    .rows
                    .iter()
                    .find(|(_, i)| *i == cursor)
                    .map(|(rect, _)| rect.y)
                    .expect("caret rect");
                assert!(
                    lines[caret_y as usize].contains(&format!("entry-{cursor:03}")),
                    "{width}x{height}/{total}: the caret rect and the drawn row disagree"
                );
                // Nothing the viewport drew may sit on the pinned hint row.
                assert!(
                    geom.rows.iter().all(|(rect, _)| rect.y < hint_y),
                    "{width}x{height}/{total}: a list row overwrote the hint row"
                );
                if total > geom.capacity {
                    let indicator = geom
                        .indicator
                        .clone()
                        .expect("a scrolled list says where it is");
                    assert!(
                        hint.contains(&indicator),
                        "{width}x{height}/{total}: {hint} is missing {indicator}"
                    );
                } else {
                    assert_eq!(geom.indicator, None, "{width}x{height}/{total}");
                }
            }
        }
    }

    /// Degenerate areas: no room for a row, no height at all, no width at
    /// all. The release profile is `panic = "abort"`, so every one of these
    /// has to produce a frame rather than an exit.
    #[test]
    fn a_list_dialog_with_no_room_for_a_row_still_draws_and_never_panics() {
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 40, 0),
            Rect::new(0, 0, 0, 20),
            Rect::new(0, 0, 3, 3),
            Rect::new(0, 0, 40, 3),
            Rect::new(0, 0, 40, 4),
        ] {
            let spec = list_spec(60, 59, 0);
            let backend = TestBackend::new(area.width.max(1), area.height.max(1));
            let mut term = Terminal::new(backend).expect("terminal");
            term.draw(|f| render_list_dialog(f, area, &spec))
                .expect("draw must not panic");
            if let Some(geom) = list_dialog_layout(area, &spec) {
                assert!(geom.rows.len() <= geom.capacity);
                assert!(geom.rect.height <= area.height);
                assert!(geom.rect.width <= area.width.max(1));
            }
        }
    }

    /// A dialog hint is only clickable when it stands for exactly one key --
    /// `j/k` and `any key` get no region rather than a guessed one, so a
    /// pointer can never reach a path the keyboard could not.
    #[test]
    fn only_single_key_dialog_hints_get_a_clickable_region() {
        assert_eq!(footer_key_code("\u{23ce}"), Some(KeyCode::Enter));
        assert_eq!(footer_key_code("esc"), Some(KeyCode::Esc));
        assert_eq!(footer_key_code("esc/q"), Some(KeyCode::Esc));
        assert_eq!(footer_key_code("space"), Some(KeyCode::Char(' ')));
        assert_eq!(footer_key_code("c"), Some(KeyCode::Char('c')));
        assert_eq!(footer_key_code("j/k"), None);
        assert_eq!(footer_key_code("a-z"), None);
        assert_eq!(footer_key_code("any key"), None);
        assert_eq!(footer_key_code(""), None);

        let area = Rect::new(0, 0, 80, 20);
        let spec = list_spec(3, 0, 0);
        let geom = list_dialog_layout(area, &spec).expect("layout");
        // MAIL_FOOTER is ⏎ / c / j-k / esc: three clickable, `j/k` not.
        assert_eq!(
            geom.hints.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
            vec![
                HintId::DialogKey(KeyCode::Enter),
                HintId::DialogKey(KeyCode::Char('c')),
                HintId::DialogKey(KeyCode::Esc),
            ]
        );
        // Every hint rect sits on the one pinned hint row, inside the dialog.
        let hint_y = geom.inner.y + geom.inner.height - 1;
        for (rect, id) in &geom.hints {
            assert_eq!(rect.y, hint_y, "{id:?}");
            assert!(rect.right() <= geom.inner.right(), "{id:?} ran off the row");
        }
    }

    /// `frame_snapshot` carries exactly the geometry `render_overlay` drew,
    /// for every list-shaped overlay -- including the two phase 3 adds.
    #[test]
    fn frame_snapshot_carries_the_open_dialogs_own_rows_and_hints() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = layout(area, 44);
        let rows: Vec<SidebarRow> = vec![sidebar_row("aaaa1111", "claude", RowState::Idle)];
        let nothing_collapsed = HashSet::new();
        let roster = roster_frame(layout.sidebar, &rows, &test_roster_view(&nothing_collapsed));
        let header = base_facts();
        for overlay in every_overlay() {
            let snap = frame_snapshot(area, &layout, false, &roster, &header, &overlay, 0);
            if matches!(overlay, Overlay::None) {
                assert!(snap.overlay.is_none());
                assert!(snap.overlay_rows.is_empty());
                assert!(snap.overlay_hints.is_empty());
                assert_eq!(snap.overlay_capacity, 0);
                continue;
            }
            let rect = snap.overlay.expect("an open overlay has a rect");
            for (row, index) in &snap.overlay_rows {
                assert!(
                    rect.union(*row) == rect,
                    "row {index} at {row:?} escaped the dialog {rect:?}"
                );
            }
            for (hint, id) in &snap.overlay_hints {
                assert!(
                    rect.union(*hint) == rect,
                    "hint {id:?} at {hint:?} escaped the dialog {rect:?}"
                );
            }
            // A list-shaped overlay reports a capacity; a compose buffer does
            // not, and reports no rows either.
            if list_spec_for(&overlay, 0).is_some() {
                assert!(snap.overlay_capacity > 0, "{overlay:?}");
            } else {
                assert!(snap.overlay_rows.is_empty(), "{overlay:?}");
                assert!(snap.overlay_hints.is_empty(), "{overlay:?}");
            }
        }
    }

    /// Every menu entry keeps the first free letter of its own label, so
    /// `restore` keeps `r` and `retry` -- which follows `evidence` -- takes
    /// `t`. No two entries ever share one.
    #[test]
    fn menu_letters_are_unique_and_prefer_the_first_free_letter() {
        let order = [
            MenuAction::Inspect,
            MenuAction::Focus,
            MenuAction::Nudge,
            MenuAction::Mail,
            MenuAction::Handover,
            MenuAction::Stop,
            MenuAction::Restore,
            MenuAction::OpenWorktree,
            MenuAction::Evidence,
            MenuAction::Retry,
            MenuAction::Dismiss,
        ];
        let letters = menu_letters(&order);
        assert_eq!(
            letters,
            vec![
                Some('i'),
                Some('f'),
                Some('n'),
                Some('m'),
                Some('h'),
                Some('s'),
                Some('r'),
                Some('o'),
                Some('e'),
                Some('t'),
                Some('d'),
            ]
        );
        let mut seen = HashSet::new();
        for letter in letters.into_iter().flatten() {
            assert!(seen.insert(letter), "{letter} was handed out twice");
        }
    }

    /// A disabled menu entry is drawn, dim, WITH its reason -- never hidden.
    #[test]
    fn a_disabled_menu_entry_is_drawn_with_its_reason() {
        let overlay = Overlay::Menu(MenuView {
            target: "aaaa1111".to_string(),
            subject: "aaaa1111 \u{b7} worker".to_string(),
            entries: vec![
                MenuEntry {
                    action: MenuAction::Inspect,
                    disabled: None,
                    letter: Some('i'),
                },
                MenuEntry {
                    action: MenuAction::Restore,
                    disabled: Some("no spawn request kept".to_string()),
                    letter: Some('r'),
                },
            ],
            cursor: 0,
            offset: 0,
            confirm: None,
        });
        let area = Rect::new(0, 0, 80, 20);
        let text = render_and_capture_text(area, |f, area| render_overlay(f, area, &overlay, 0));
        assert!(text.contains("aaaa1111"), "the menu names its target");
        assert!(text.contains("restore"), "the entry is never hidden");
        assert!(
            text.contains("nospawnrequestkept") || text.contains("no spawn request kept"),
            "the reason is on the same row: {text}"
        );
    }

    #[test]
    fn draft_lines_splits_on_newline_with_a_two_space_continuation_prefix() {
        assert_eq!(
            draft_lines("line one\nline two"),
            vec!["> line one".to_string(), "  line two".to_string()]
        );
    }

    #[test]
    fn draft_lines_keeps_a_single_line_draft_as_one_row() {
        assert_eq!(draft_lines("hello"), vec!["> hello".to_string()]);
    }

    #[test]
    fn every_overlay_renders_into_a_zero_sized_area_without_panicking() {
        let backend = TestBackend::new(20, 10);
        let mut term = Terminal::new(backend).expect("terminal");
        for overlay in every_overlay() {
            term.draw(|f| render_overlay(f, Rect::new(0, 0, 0, 0), &overlay, 0))
                .expect("draw");
            // A zero-height-but-not-zero-width sliver, and its transpose:
            // both used to reach the same clamp.
            term.draw(|f| render_overlay(f, Rect::new(0, 0, 20, 0), &overlay, 0))
                .expect("draw");
            term.draw(|f| render_overlay(f, Rect::new(0, 0, 0, 10), &overlay, 0))
                .expect("draw");
        }
    }

    #[test]
    fn every_overlay_renders_into_a_one_by_one_area_without_panicking() {
        let backend = TestBackend::new(20, 10);
        let mut term = Terminal::new(backend).expect("terminal");
        for overlay in every_overlay() {
            term.draw(|f| render_overlay(f, Rect::new(0, 0, 1, 1), &overlay, 0))
                .expect("draw");
        }
    }

    /// Issue #354 phase 4: the palette/help pair. The sync tests that used
    /// to live here (`help_bindings_match_the_real_filter_key_dispatch` and
    /// `help_bindings_cover_every_dash_action`) moved to `actions.rs`, next
    /// to the one table they now prove.
    fn palette(mode: PaletteMode, query: &str) -> PaletteView {
        let ctx = ActionContext {
            selected: true,
            attached: true,
            alive: true,
            ..ActionContext::default()
        };
        let mut view = PaletteView {
            mode,
            query: query.to_string(),
            ctx,
            cursor: 0,
            offset: 0,
        };
        view.cursor = actions::palette_first(&view.rows());
        view
    }

    fn draw_overlay(width: u16, height: u16, overlay: &Overlay) -> String {
        render_and_capture_text(Rect::new(0, 0, width, height), |f, area| {
            render_overlay(f, area, overlay, 0)
        })
    }

    /// Every `^A` chord any surface draws is one the table defines: the
    /// header cluster, the rendered context menu and the rendered help
    /// screen are each scanned for `^A x` tokens and checked against
    /// [`actions::ACTIONS`]. A chord drawn anywhere else than the table is
    /// exactly the drift (findings F08/F09) this phase closes.
    #[test]
    fn every_chord_drawn_anywhere_comes_from_the_action_table() {
        let known: Vec<&str> = actions::ACTIONS
            .iter()
            .map(|d| d.chord)
            .filter(|c| !c.is_empty())
            .collect();
        let check = |text: &str, what: &str| {
            for (i, _) in text.match_indices("^A ") {
                // `^A ^A` (send the child a literal prefix) contains a second
                // `^A ` of its own; only the first is a chord start.
                if i >= 3 && &text[i - 3..i] == "^A " {
                    continue;
                }
                let rest = &text[i..];
                assert!(
                    known.iter().any(|chord| rest.starts_with(chord)),
                    "{what} drew a chord the action table does not define: {:?}",
                    rest.chars().take(12).collect::<String>()
                );
            }
        };
        for context in [
            HintContext::default(),
            HintContext {
                alive: true,
                ..HintContext::default()
            },
            HintContext {
                alive: true,
                needs_action: true,
                ..HintContext::default()
            },
            HintContext {
                ended: true,
                restorable: true,
                ..HintContext::default()
            },
        ] {
            for (chord, _) in header_hints(&context) {
                assert!(known.contains(&chord), "header drew {chord:?}");
            }
            let mut facts = base_facts();
            facts.hints = context;
            let drawn = render_and_capture_text(Rect::new(0, 0, 120, 1), |f, area| {
                render_header(f, area, &facts)
            });
            check(&drawn, "the header");
            // Review of cc92a56 (finding 3): the first-run tip draws two
            // chords of its own in the same slot, and used to be the one
            // surface this audit never rendered.
            facts.tip = Some(FIRST_RUN_TIP.as_str());
            let with_tip = render_and_capture_text(Rect::new(0, 0, 160, 1), |f, area| {
                render_header(f, area, &facts)
            });
            check(&with_tip, "the first-run tip");
            facts.tip = None;
        }
        check(
            &draw_overlay(120, 40, &Overlay::Palette(palette(PaletteMode::Help, ""))),
            "help",
        );
        check(
            &draw_overlay(120, 40, &Overlay::Palette(palette(PaletteMode::Run, ""))),
            "the palette",
        );
        let menu = Overlay::Menu(MenuView {
            target: "aaaa1111".to_string(),
            subject: "aaaa1111 \u{b7} worker".to_string(),
            entries: vec![MenuEntry {
                action: MenuAction::Inspect,
                disabled: None,
                letter: Some('i'),
            }],
            cursor: 0,
            offset: 0,
            confirm: None,
        });
        check(&draw_overlay(120, 40, &menu), "the context menu");
    }

    /// The help screen is the palette in read-only mode: same rows, same
    /// filter, same sections -- and it says so in its own pinned footer.
    #[test]
    fn the_help_screen_lists_every_section_and_says_typing_filters() {
        let text = draw_overlay(120, 40, &Overlay::Palette(palette(PaletteMode::Help, "")));
        assert!(text.contains("help"), "{text}");
        for section in ActionSection::ORDER {
            assert!(
                text.contains(section.title()),
                "help is missing the {:?} section: {text}",
                section.title()
            );
        }
        assert!(text.contains("to filter"), "{text}");
        assert!(text.contains("esc"), "{text}");
    }

    /// A query filters both faces the same way, and the query itself is
    /// drawn on the dialog's own pinned input row.
    #[test]
    fn a_typed_query_filters_the_palette_and_is_echoed_on_its_input_row() {
        let text = draw_overlay(
            120,
            40,
            &Overlay::Palette(palette(PaletteMode::Run, "hndv")),
        );
        assert!(text.contains("handover"), "{text}");
        assert!(text.contains("\u{203a} hndv"), "{text}");
        assert!(!text.contains("quit"), "{text}");
        let empty = draw_overlay(
            120,
            40,
            &Overlay::Palette(palette(PaletteMode::Run, "zzqqxx")),
        );
        assert!(empty.contains("(nothing matches)"), "{empty}");
    }

    /// A disabled row is listed with its reason and rendered dim, and the
    /// view refuses to activate it.
    #[test]
    fn a_disabled_palette_row_shows_its_reason_and_cannot_be_activated() {
        let mut view = palette(PaletteMode::Run, "restore");
        let rows = view.rows();
        let index = rows
            .iter()
            .position(|r| matches!(r, PaletteRow::Action { chord: "^A r", .. }))
            .expect("restore row");
        view.cursor = index;
        assert_eq!(view.activated(), None);
        let text = draw_overlay(120, 40, &Overlay::Palette(view));
        assert!(text.contains("disabled: still running"), "{text}");
    }

    /// Help never runs anything, however runnable the caret's row is --
    /// that is the whole difference between the two modes.
    #[test]
    fn help_mode_never_activates_a_row() {
        let mut view = palette(PaletteMode::Help, "close the dashboard");
        view.cursor = actions::palette_first(&view.rows());
        assert!(view.rows()[view.cursor].activatable());
        assert_eq!(view.activated(), None);
        assert_eq!(
            palette(PaletteMode::Run, "close the dashboard").activated(),
            Some(ActionId::Quit)
        );
    }

    /// The pinned query row is counted by the layout, so the row rects a
    /// click is tested against are the rows that were actually drawn -- one
    /// line lower than a dialog without an input.
    #[test]
    fn the_pinned_query_row_shifts_the_list_viewport_down_by_exactly_one() {
        let area = Rect::new(0, 0, 60, 20);
        let spec = |input: Option<String>| ListDialogSpec {
            title: "t".to_string(),
            count: None,
            rows: (0..40)
                .map(|i| ListDialogRow::plain(format!("row {i}")))
                .collect(),
            cursor: Some(0),
            offset: 0,
            footer: PALETTE_FOOTER,
            warn: false,
            empty_message: "",
            input,
        };
        let base = spec(None);
        let with_input = spec(Some("q".to_string()));
        let plain = list_dialog_layout(area, &base).expect("layout");
        let query = list_dialog_layout(area, &with_input).expect("layout");
        assert_eq!(plain.input_rows, 0);
        assert_eq!(query.input_rows, 1);
        assert_eq!(query.capacity + 1, plain.capacity);
        assert_eq!(query.rows[0].0.y, plain.rows[0].0.y + 1);
    }

    /// The palette renders at every approved frame size, with a handful of
    /// results and with the whole table, without panicking and with the
    /// pinned footer still on screen.
    #[test]
    fn the_palette_renders_at_every_frame_size_with_few_and_many_results() {
        for (width, height) in [(80u16, 20u16), (120, 40), (200, 50)] {
            let few = draw_overlay(
                width,
                height,
                &Overlay::Palette(palette(PaletteMode::Run, "sc")),
            );
            assert!(few.contains("scroll"), "{width}x{height}: {few}");
            let many = draw_overlay(
                width,
                height,
                &Overlay::Palette(palette(PaletteMode::Run, "")),
            );
            assert!(many.contains("\u{203a}"), "{width}x{height}: {many}");
            let help = draw_overlay(
                width,
                height,
                &Overlay::Palette(palette(PaletteMode::Help, "")),
            );
            assert!(help.contains("esc"), "{width}x{height}: {help}");
        }
        assert!(
            palette(PaletteMode::Run, "").rows().len() > 3,
            "the table should have more rows than one viewport"
        );
    }

    /// The first-run tip takes the header's middle slot, dim, and yields it
    /// to a real notice or a sticky error -- both of which are news, and the
    /// tip is not.
    #[test]
    fn the_first_run_tip_takes_the_middle_slot_only_when_nothing_else_needs_it() {
        let render = |facts: &HeaderFacts| {
            render_and_capture_text(Rect::new(0, 0, 200, 1), |f, area| {
                render_header(f, area, facts)
            })
        };
        let mut facts = base_facts();
        facts.tip = Some(FIRST_RUN_TIP.as_str());
        assert!(render(&facts).contains("^A p palette"));
        // Visible at every approved frame width -- truncated with an ellipsis
        // where the row is too narrow to hold it, never dropped silently and
        // never pushing the hint cluster off the row.
        for width in [80u16, 120, 200] {
            let drawn = render_and_capture_text(Rect::new(0, 0, width, 1), |f, area| {
                render_header(f, area, &facts)
            });
            assert!(drawn.contains("^A ?"), "{width}: {drawn}");
            assert!(drawn.contains("zirv"), "{width}: {drawn}");
        }
        facts.notice = Some("spawned claude as aaaa1111".to_string());
        let with_notice = render(&facts);
        assert!(with_notice.contains("spawned claude"));
        assert!(!with_notice.contains("^A p palette"));
        facts.notice = None;
        facts.error_count = 1;
        facts.latest_error = Some("something broke".to_string());
        let with_error = render(&facts);
        assert!(with_error.contains("something broke"));
        assert!(!with_error.contains("^A p palette"));
    }

    /// `tests/fixtures/claude-session.raw` is a gitignored capture of a real
    /// interactive session (see the vt100 spike,
    /// `docs/superpowers/notes/2026-08-13-vt100-spike.md`) -- present on the
    /// machine that recorded it, absent everywhere else including CI. Skips
    /// cleanly rather than failing when it is not there.
    #[test]
    fn renders_a_real_claude_session_fixture_without_panicking() {
        let path = std::path::Path::new("tests/fixtures/claude-session.raw");
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!(
                "skipping renders_a_real_claude_session_fixture_without_panicking: {} not present",
                path.display()
            );
            return;
        };

        let mut parser = vt100::Parser::new(40, 120, 0);
        parser.process(&bytes);

        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_grid(f, f.area(), parser.screen(), None))
            .unwrap();

        let buf = term.backend().buffer();
        let non_blank = (0..40).any(|y| (0..120).any(|x| buf[(x, y)].symbol() != " "));
        assert!(
            non_blank,
            "expected the real session fixture to render at least one non-blank cell"
        );
    }

    // ------------------------------------------------------------------
    // Dash refresh PR1: the pane header row.
    // ------------------------------------------------------------------

    fn pane_header_facts() -> PaneHeaderFacts {
        PaneHeaderFacts {
            harness: "claude".to_string(),
            role: "worker".to_string(),
            model: Some("sonnet-5".to_string()),
            cwd: "~/zirv-482".to_string(),
            workflow: None,
            glyph: Glyph::Working,
            state_word: "working".to_string(),
            age_secs: Some(180),
        }
    }

    /// The mock's own "Running" example (§04): workflow segment, then the
    /// glyph, the CAPITALIZED state word, and the age -- the sidebar's own
    /// fact line keeps the word lowercase, this is the one place it is
    /// capitalized.
    #[test]
    fn render_pane_header_shows_identity_workflow_and_capitalized_state() {
        let mut facts = pane_header_facts();
        facts.workflow = Some(SessionWorkflowFact {
            kind: "bugfix".to_string(),
            step: "debug".to_string(),
            index: 3,
            total: 7,
            awaiting_approval: false,
            completed: false,
        });
        let text = render_and_capture_text(Rect::new(0, 0, 91, 1), |f, area| {
            render_pane_header(f, area, &facts, 0)
        });
        assert!(text.contains("claude"), "got {text:?}");
        assert!(text.contains("worker"), "got {text:?}");
        assert!(text.contains("sonnet-5"), "got {text:?}");
        assert!(text.contains("~/zirv-482"), "got {text:?}");
        assert!(text.contains("bugfix"), "got {text:?}");
        assert!(text.contains("debug 3/7"), "got {text:?}");
        assert!(
            text.contains("Working"),
            "state word is capitalized here: got {text:?}"
        );
        assert!(text.contains("3m"), "got {text:?}");
    }

    /// A session with no bound workflow shows nothing for that segment --
    /// never a guess from the repo-wide pointer.
    #[test]
    fn render_pane_header_with_no_workflow_shows_nothing_for_it() {
        let facts = pane_header_facts();
        let text = render_and_capture_text(Rect::new(0, 0, 91, 1), |f, area| {
            render_pane_header(f, area, &facts, 0)
        });
        assert!(
            !text.contains('\u{203a}'),
            "no workflow segment (its own `›` step separator) at all: got {text:?}"
        );
    }

    /// Truncates the LEFT side (identity) before it ever touches the right
    /// (workflow/state/age) -- the right is what is actually happening now.
    #[test]
    fn render_pane_header_truncates_left_before_right() {
        let mut facts = pane_header_facts();
        facts.cwd =
            "~/a-very-long-checkout-path-that-will-not-fit-in-a-narrow-pane-header".to_string();
        facts.workflow = Some(SessionWorkflowFact {
            kind: "bugfix".to_string(),
            step: "debug".to_string(),
            index: 3,
            total: 7,
            awaiting_approval: false,
            completed: false,
        });
        let narrow = render_and_capture_text(Rect::new(0, 0, 40, 1), |f, area| {
            render_pane_header(f, area, &facts, 0)
        });
        assert!(
            style::display_width(&narrow) <= 40,
            "must never overflow its own budget: got {narrow:?}"
        );
        assert!(
            !narrow.contains("a-very-long-checkout-path"),
            "the long cwd is the first thing to go: got {narrow:?}"
        );
        assert!(
            narrow.contains("debug"),
            "the workflow/state segment survives the squeeze: got {narrow:?}"
        );
    }

    /// A completed workflow reads `✓ done`, and a gated one (awaiting
    /// approval) is bold yellow.
    #[test]
    fn render_pane_header_workflow_states() {
        let mut facts = pane_header_facts();
        facts.workflow = Some(SessionWorkflowFact {
            kind: "bugfix".to_string(),
            step: "done".to_string(),
            index: 7,
            total: 7,
            awaiting_approval: false,
            completed: true,
        });
        let done = render_and_capture_text(Rect::new(0, 0, 91, 1), |f, area| {
            render_pane_header(f, area, &facts, 0)
        });
        assert!(done.contains("\u{2713} done"), "got {done:?}");

        facts.workflow = Some(SessionWorkflowFact {
            kind: "feature".to_string(),
            step: "review".to_string(),
            index: 6,
            total: 8,
            awaiting_approval: true,
            completed: false,
        });
        let gated = render_and_capture_text(Rect::new(0, 0, 91, 1), |f, area| {
            render_pane_header(f, area, &facts, 0)
        });
        assert!(gated.contains("review 6/8"), "got {gated:?}");
    }

    // ------------------------------------------------------------------
    // Dash refresh PR1: the LIMITS block.
    // ------------------------------------------------------------------

    fn no_detail() -> WindowDetail {
        WindowDetail {
            resets_at: 0,
            limit_reached: false,
            overage_covered: false,
        }
    }

    /// UTC, standing in for "no offset" wherever a test's own point is not
    /// the offset itself.
    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    #[test]
    fn limits_reset_time_today_is_bare_hhmm() {
        // now = day 100 at 10:00 UTC; resets later the same day at 16:20.
        let now = 100 * 86_400 + 10 * 3600;
        let resets_at = 100 * 86_400 + 16 * 3600 + 20 * 60;
        assert_eq!(limits_reset_time(utc(), resets_at, now), "16:20");
    }

    #[test]
    fn limits_reset_time_within_a_week_names_the_weekday() {
        // 1970-01-01 (day 0) was a Thursday.
        let now = 0;
        let resets_at = 3 * 86_400 + 9 * 3600; // day 3 (Sunday) at 09:00
        assert_eq!(limits_reset_time(utc(), resets_at, now), "Sun 09:00");
    }

    #[test]
    fn limits_reset_time_later_names_the_date() {
        let now = 0;
        let resets_at = 40 * 86_400; // day 40: 1970-02-10
        assert_eq!(limits_reset_time(utc(), resets_at, now), "10 Feb");
    }

    #[test]
    fn limits_reset_time_uses_the_given_offset_for_the_today_boundary() {
        // Denmark-shaped case: at +2h, a reset just past UTC midnight can
        // still be "today" on the operator's own clock even though the UTC
        // calendar date has already turned over.
        let offset = FixedOffset::east_opt(2 * 3600).unwrap();
        let now = 100 * 86_400 + 23 * 3600; // 23:00 UTC == day 101, 01:00 local
        let resets_at = 101 * 86_400 + 1_800; // 00:30 UTC == day 101, 02:30 local
        assert_eq!(
            limits_reset_time(offset, resets_at, now),
            "02:30",
            "same local day as `now`, so bare HH:MM, not a weekday/date"
        );
    }

    #[test]
    fn limits_countdown_drops_the_hour_under_sixty_minutes() {
        assert_eq!(limits_countdown(600, 0), "10m");
        assert_eq!(limits_countdown(1, 0), "1m", "never a literal 0m");
        assert_eq!(limits_countdown(8_000, 0), "2h13m");
    }

    #[test]
    fn limits_reset_line_reads_resets_with_a_countdown_when_due_today() {
        let now = 100 * 86_400;
        let detail = WindowDetail {
            resets_at: now + 2 * 3600 + 13 * 60,
            ..no_detail()
        };
        let (text, style) = limits_reset_line(utc(), 41.0, &detail, now);
        assert_eq!(
            text,
            format!(
                "resets {} \u{b7} in 2h13m",
                limits_hhmm(utc(), detail.resets_at)
            )
        );
        assert_eq!(style, style::tui::muted());
    }

    #[test]
    fn limits_reset_line_at_80_percent_or_more_turns_yellow_with_no_countdown_beyond_today() {
        let now = 0;
        let detail = WindowDetail {
            resets_at: 3 * 86_400,
            ..no_detail()
        };
        let (text, style) = limits_reset_line(utc(), 86.0, &detail, now);
        assert!(
            !text.contains("in "),
            "no countdown past today: got {text:?}"
        );
        assert_eq!(style, style::tui::warning());
    }

    /// Round 2 coordinator review, CONFIRMED: a `resets_at` already in the
    /// past (a stale vendor reading) must never read as an ordinary future
    /// `resets HH:MM` -- it gets its own muted "stale" wording instead, at
    /// every `pct`, and fits well inside the 28-col sidebar row.
    #[test]
    fn limits_reset_line_on_a_past_resets_at_reads_stale_not_a_future_reset() {
        let now = 100 * 86_400;
        let detail = WindowDetail {
            resets_at: now - 3600,
            ..no_detail()
        };
        let (text, style) = limits_reset_line(utc(), 95.0, &detail, now);
        assert_eq!(
            text,
            format!(
                "reset at {} \u{b7} stale",
                limits_hhmm(utc(), detail.resets_at)
            )
        );
        assert!(!text.contains("in "), "no countdown on a stale reading");
        assert_eq!(
            style,
            style::tui::muted(),
            "always muted, even at a high pct"
        );
        assert!(
            style::display_width(&text) <= 25,
            "must fit the 28-col sidebar row with its own 3-col indent: got {text:?}"
        );
    }

    #[test]
    fn limits_reset_line_at_the_limit_reads_back_at_with_a_countdown() {
        let now = 100 * 86_400;
        let detail = WindowDetail {
            resets_at: now + 12 * 60,
            limit_reached: true,
            overage_covered: false,
        };
        let (text, style) = limits_reset_line(utc(), 100.0, &detail, now);
        assert_eq!(
            text,
            format!(
                "back at {} \u{b7} 12m",
                limits_hhmm(utc(), detail.resets_at)
            )
        );
        assert_eq!(style, style::tui::error());
    }

    #[test]
    fn limits_reset_line_on_credits_reads_credits_until_with_no_countdown() {
        let now = 100 * 86_400;
        let detail = WindowDetail {
            resets_at: now + 3600,
            limit_reached: false,
            overage_covered: true,
        };
        let (text, style) = limits_reset_line(utc(), 100.0, &detail, now);
        assert_eq!(
            text,
            format!("credits until {}", limits_hhmm(utc(), detail.resets_at))
        );
        assert!(
            !text.contains("in "),
            "on-credits has no countdown: got {text:?}"
        );
        assert_eq!(style, Style::default().fg(Color::Magenta));
    }

    #[test]
    fn limits_blocks_from_usage_pairs_5h_before_wk_and_blanks_the_repeat_harness() {
        let usages = vec![
            HarnessUsage {
                name: "claude",
                five_hour: Some(41.0),
                seven_day: Some(12.0),
                five_hour_detail: Some(no_detail()),
                seven_day_detail: Some(no_detail()),
                credits: false,
            },
            HarnessUsage {
                name: "codex",
                five_hour: None,
                seven_day: Some(58.0),
                five_hour_detail: None,
                seven_day_detail: Some(no_detail()),
                credits: false,
            },
            HarnessUsage {
                name: "gemini",
                five_hour: None,
                seven_day: None,
                five_hour_detail: None,
                seven_day_detail: None,
                credits: false,
            },
        ];
        let blocks = limits_blocks_from_usage(&usages);
        // gemini has neither window, so it contributes nothing at all.
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].harness, "claude");
        assert_eq!(blocks[0].window_label, "5h");
        assert!(blocks[0].show_harness);
        assert_eq!(blocks[1].harness, "claude");
        assert_eq!(blocks[1].window_label, "wk");
        assert!(!blocks[1].show_harness, "the repeat harness label is blank");
        assert_eq!(blocks[2].harness, "codex");
        assert_eq!(blocks[2].window_label, "wk");
        assert!(
            blocks[2].show_harness,
            "codex's first block for THIS harness still shows its name, even though it is wk not 5h"
        );
    }

    #[test]
    fn limits_blocks_fitting_drops_a_whole_window_at_a_time() {
        // 2 rows for title+rule, 2 rows per block: 3 blocks need 8 rows.
        assert_eq!(limits_blocks_fitting(3, 8), 3, "everything fits");
        assert_eq!(
            limits_blocks_fitting(3, 7),
            2,
            "one whole block drops, never a half"
        );
        assert_eq!(limits_blocks_fitting(3, 5), 1);
        assert_eq!(
            limits_blocks_fitting(3, 3),
            0,
            "3 rows is only the title/rule, no room left over"
        );
        assert_eq!(
            limits_blocks_fitting(3, 2),
            0,
            "room for the title/rule but no block"
        );
        assert_eq!(
            limits_blocks_fitting(3, 1),
            0,
            "not even room for the title"
        );
        assert_eq!(limits_blocks_fitting(3, 0), 0);
        assert_eq!(
            limits_rows_for(0),
            0,
            "nothing to show draws nothing at all"
        );
        assert_eq!(limits_rows_for(2), 6);
    }

    /// The mock's own worked example (§03/§01): claude 5h 41% / wk 12%,
    /// rendered exactly, at the sidebar's own 28-column width.
    #[test]
    fn render_limits_matches_the_mock_worked_example() {
        let now = 100 * 86_400 + 14 * 3600 + 7 * 60; // 14:07 UTC
        let usages = vec![HarnessUsage {
            name: "claude",
            five_hour: Some(41.0),
            seven_day: Some(12.0),
            five_hour_detail: Some(WindowDetail {
                resets_at: now + 2 * 3600 + 13 * 60,
                limit_reached: false,
                overage_covered: false,
            }),
            seven_day_detail: Some(WindowDetail {
                resets_at: now + 3 * 86_400,
                limit_reached: false,
                overage_covered: false,
            }),
            credits: false,
        }];
        let blocks = limits_blocks_from_usage(&usages);
        // 2 (title+rule) + 2 blocks * 2 rows each = 6 rows.
        let area = Rect::new(0, 0, 28, 6);
        let text =
            render_and_capture_text(area, |f, area| render_limits(f, area, &blocks, now, utc()));
        assert!(text.contains(" LIMITS"), "got {text:?}");
        assert!(text.contains("claude"), "got {text:?}");
        assert!(text.contains("5h"), "got {text:?}");
        assert!(text.contains("41%"), "got {text:?}");
        assert!(text.contains("wk"), "got {text:?}");
        assert!(text.contains("12%"), "got {text:?}");
        assert!(text.contains("in 2h13m"), "got {text:?}");
    }

    /// Coordinator scope addition: a disabled/unavailable harness must never
    /// appear in the LIMITS block, even with stale usage files still on
    /// disk for it -- `limits_blocks_from_usage` only ever sees whatever
    /// `disk.usage` already carries, and that list is built from `cfg.
    /// agents.is_enabled(name)`-filtered adapters (`FactsCache::refresh_
    /// if_due`), so a disabled harness's `HarnessUsage` never reaches here
    /// at all.
    #[test]
    fn limits_blocks_from_usage_never_shows_a_harness_missing_from_the_list() {
        // Simulates what the facts refresh already filtered out: a disabled
        // harness's usage never becomes a `HarnessUsage` entry in the first
        // place, so `disk.usage` (what this function reads) never carries
        // it even if its window file is still sitting on disk.
        let usages = vec![HarnessUsage {
            name: "claude",
            five_hour: Some(41.0),
            seven_day: None,
            five_hour_detail: Some(no_detail()),
            seven_day_detail: None,
            credits: false,
        }];
        let blocks = limits_blocks_from_usage(&usages);
        assert!(
            blocks.iter().all(|b| b.harness == "claude"),
            "a harness the facts refresh never enabled must not appear: {:?}",
            blocks.iter().map(|b| b.harness).collect::<Vec<_>>()
        );
        assert_eq!(blocks.len(), 1);
    }
}
