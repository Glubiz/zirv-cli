//! What moves in the orchestrator dashboard, and when it starts. A pulse, a flash or a fresh
//! activity row is born when a gather delivers something the previous gather did not hold (or the
//! operator answers), never while drawing and never by reading a file: `observe` compares the data
//! the view already has with what it saw last, and every frame is then a pure function of those
//! birth times and the injected clock.

use std::collections::{HashMap, HashSet};

use super::super::super::graph::{Event, Node};
use super::super::super::sessions;
use super::content::{self, Mark};
use super::theme::{Rgb, c};
use super::{TreeData, TreeFacts};

/// How long a toast stays on the FLOW border.
pub(super) const TOAST_MS: u64 = 3000;
/// How long a new activity row keeps its highlight.
pub(super) const ROW_FADE_MS: u64 = 1800;
pub(super) const DISPATCH_FLASH_MS: u64 = 1000;
pub(super) const DONE_FLASH_MS: u64 = 1500;
pub(super) const JEV_FLASH_MS: u64 = 1400;

/// A glyph travelling the bus to or from one agent's card.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Pulse {
    /// A node id, or `seat`.
    pub(super) id: String,
    /// Towards the seat instead of away from it.
    pub(super) up: bool,
    pub(super) glyph: char,
    pub(super) col: Rgb,
    pub(super) born: u64,
    pub(super) dur: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Toast {
    pub(super) msg: String,
    pub(super) col: Rgb,
    pub(super) born: u64,
}

#[derive(Debug, Default)]
struct Seen {
    /// Node id to whether it was finished.
    nodes: HashMap<String, bool>,
    events: HashSet<String>,
    asks: HashSet<u64>,
    jev: HashSet<String>,
}

#[derive(Debug, Default)]
pub(super) struct Motion {
    pub(super) pulses: Vec<Pulse>,
    pub(super) dispatched: HashMap<String, u64>,
    pub(super) finished: HashMap<String, u64>,
    /// Activity rows by [`row_key`], when they first appeared.
    pub(super) rows: HashMap<String, u64>,
    /// When Jev last decided something, for the flash on its border.
    pub(super) jev_flash: Option<u64>,
    pub(super) toast: Option<Toast>,
    seen: Option<Seen>,
    seen_rev: u64,
}

pub(super) fn event_key(e: &Event) -> String {
    format!("{}|{}|{}|{}", e.ts, e.actor, e.kind, e.summary)
}

pub(super) fn ask_key(conn: u64) -> String {
    format!("ask:{conn}")
}

/// The node id a session id, short id or role label names; `seat` for the seat.
fn id_for(data: &TreeData, facts: &TreeFacts, raw: &str) -> Option<String> {
    let short = sessions::short_id(raw);
    if facts
        .seat_session
        .is_some_and(|seat| sessions::short_id(seat) == short)
    {
        return Some("seat".into());
    }
    let by_session =
        |n: &&Node| n.id == raw || (!short.is_empty() && sessions::short_id(&n.id) == short);
    let found = data.nodes.iter().find(by_session).or_else(|| {
        data.nodes.iter().find(|n| {
            n.role
                .as_deref()
                .is_some_and(|r| r.eq_ignore_ascii_case(raw))
        })
    });
    found.map(|n| n.id.clone())
}

impl Motion {
    pub(super) fn push(&mut self, id: &str, up: bool, glyph: char, col: Rgb, born: u64, dur: u64) {
        self.pulses.push(Pulse {
            id: id.to_string(),
            up,
            glyph,
            col,
            born,
            dur,
        });
    }

    pub(super) fn toast(&mut self, msg: impl Into<String>, col: Rgb, now: u64) {
        self.toast = Some(Toast {
            msg: msg.into(),
            col,
            born: now,
        });
    }

    /// Compare what the view holds with what it saw last and start the motion the difference
    /// earns. The first look only records: nothing flies for what was already there.
    pub(super) fn observe(&mut self, data: &TreeData, rev: u64, facts: &TreeFacts, now: u64) {
        self.pulses.retain(|p| now < p.born + p.dur);
        self.dispatched.retain(|_, b| now < *b + DISPATCH_FLASH_MS);
        self.finished.retain(|_, b| now < *b + DONE_FLASH_MS);
        self.rows.retain(|_, b| now < *b + ROW_FADE_MS);
        self.jev_flash = self.jev_flash.filter(|b| now < *b + JEV_FLASH_MS);
        if self
            .toast
            .as_ref()
            .is_some_and(|t| now >= t.born + TOAST_MS)
        {
            self.toast = None;
        }
        if !data.loaded {
            return;
        }
        let seeded = self.seen.is_some();
        let seen = self.seen.get_or_insert_with(Seen::default);
        let mut born: Vec<(String, u64)> = Vec::new();
        let mut pulses: Vec<Pulse> = Vec::new();
        let mut dispatched: Vec<String> = Vec::new();
        let mut finished: Vec<String> = Vec::new();
        let mut flash = false;
        if rev != self.seen_rev || !seeded {
            for node in data.nodes.iter().filter(|n| n.kind != "group") {
                let done = Mark::of(&node.status) == Mark::Done;
                match seen.nodes.insert(node.id.clone(), done) {
                    None if seeded && !done => dispatched.push(node.id.clone()),
                    Some(false) if done && seeded => finished.push(node.id.clone()),
                    _ => {}
                }
            }
            for event in &data.events {
                let key = event_key(event);
                if !seen.events.insert(key.clone()) || !seeded {
                    continue;
                }
                born.push((key, now));
                if event.kind != "mail" {
                    continue;
                }
                let to = event.to.as_deref().and_then(|to| id_for(data, facts, to));
                let from = id_for(data, facts, &event.actor);
                match (from, to) {
                    (_, Some(to)) if to != "seat" => pulses.push(Pulse {
                        id: to,
                        up: false,
                        glyph: '✉',
                        col: c::SEAT,
                        born: now,
                        dur: 1300,
                    }),
                    (Some(from), Some(_)) if from != "seat" => pulses.push(Pulse {
                        id: from,
                        up: true,
                        glyph: '✉',
                        col: c::SEAT,
                        born: now,
                        dur: 1300,
                    }),
                    _ => {}
                }
            }
            for row in content::jev_feed(data).rows {
                if seen.jev.insert(row.id.clone()) && seeded {
                    born.push((row.id, now));
                    flash = true;
                }
            }
            self.seen_rev = rev;
        }
        for id in dispatched {
            pulses.push(Pulse {
                id: id.clone(),
                up: false,
                glyph: '●',
                col: c::AGENT,
                born: now,
                dur: 900,
            });
            self.dispatched.insert(id, now);
        }
        for id in finished {
            pulses.push(Pulse {
                id: id.clone(),
                up: true,
                glyph: '✓',
                col: c::OK,
                born: now,
                dur: 1100,
            });
            self.finished.insert(id, now);
        }
        for approval in &facts.approval_items {
            if !seen.asks.insert(approval.conn) || !seeded {
                continue;
            }
            born.push((ask_key(approval.conn), now));
            if let Some(id) = id_for(data, facts, &approval.short) {
                pulses.push(Pulse {
                    id,
                    up: true,
                    glyph: '⚑',
                    col: c::WARN,
                    born: now,
                    dur: 1100,
                });
            }
        }
        if flash {
            self.jev_flash = Some(now);
            pulses.push(Pulse {
                id: "jev".into(),
                up: true,
                glyph: '◆',
                col: c::JEV,
                born: now,
                dur: 1200,
            });
        }
        self.pulses.extend(pulses);
        self.rows.extend(born);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;

    #[test]
    fn the_first_look_records_and_a_later_gather_starts_the_motion_it_earns() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let mut m = Motion::default();
        m.observe(&data, 1, &f, 100);
        assert!(
            m.pulses.is_empty() && m.rows.is_empty(),
            "nothing flies on the first look"
        );

        let mut next = data.clone();
        next.nodes.push(node_k(
            "n9", "seat-1", "subagent", "claude", "haiku", "Plan", "running",
        ));
        next.nodes
            .iter_mut()
            .find(|n| n.id == "a1")
            .expect("a1")
            .status = "completed".into();
        let mut mail = event(1_300, "seat-1", "mail", "go on", None);
        mail.to = Some("w1".into());
        next.events.push(mail.clone());
        // The same gather again starts nothing.
        m.observe(&data, 1, &f, 200);
        assert!(m.pulses.is_empty());
        m.observe(&next, 2, &f, 300);
        let kinds: Vec<(&str, bool, char)> = m
            .pulses
            .iter()
            .map(|p| (p.id.as_str(), p.up, p.glyph))
            .collect();
        assert!(
            kinds.contains(&("n9", false, '●')),
            "a dispatch goes down: {kinds:?}"
        );
        assert!(
            kinds.contains(&("a1", true, '✓')),
            "a finish comes up: {kinds:?}"
        );
        assert!(
            kinds.contains(&("w1", false, '✉')),
            "mail goes down: {kinds:?}"
        );
        assert_eq!(m.dispatched.get("n9"), Some(&300));
        assert_eq!(m.finished.get("a1"), Some(&300));
        assert_eq!(m.rows.get(&event_key(&mail)), Some(&300));
        // Everything expires on its own clock.
        m.observe(&next, 2, &f, 300 + ROW_FADE_MS + 1);
        assert!(m.pulses.is_empty() && m.rows.is_empty() && m.dispatched.is_empty());
    }

    #[test]
    fn a_new_request_asks_upwards_once() {
        let (data, wf, jev) = busy();
        let mut f = orch_facts(&wf, &jev);
        let held = f.approval_items.remove(0);
        let mut m = Motion::default();
        m.observe(&data, 1, &f, 0);
        f.approval_items.push(held);
        m.observe(&data, 1, &f, 50);
        m.observe(&data, 1, &f, 60);
        let asks: Vec<&Pulse> = m.pulses.iter().filter(|p| p.glyph == '⚑').collect();
        assert_eq!(asks.len(), 1, "{:?}", m.pulses);
        assert!(asks[0].up && asks[0].id == "w1" && asks[0].born == 50);
    }
}
