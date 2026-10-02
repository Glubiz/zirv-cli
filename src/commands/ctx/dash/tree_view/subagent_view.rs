//! A read-only live view of one native Claude subagent's own transcript
//! (`<session>/subagents/agent-<id>.jsonl`), for the subagents the host pane cannot be driven to:
//! finished and no longer listed by Claude, a busy host, or a host with no pane here.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use serde_json::Value;

use super::content::pal;

const TAIL_BYTES: u64 = 1024 * 1024;
const REFRESH: Duration = Duration::from_millis(500);
const ENTRY_LINES: usize = 12;
const PREVIEW_CHARS: usize = 600;

/// A file's modified time and length.
type Stamp = (SystemTime, u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    User,
    Assistant,
    Tool,
    Result,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    kind: Kind,
    text: String,
}

pub(super) struct SubagentView {
    title: String,
    path: PathBuf,
    stamp: Option<Stamp>,
    checked: Option<Instant>,
    entries: Vec<Entry>,
    /// The file was longer than the tail read, so the start of its history is not shown.
    truncated: bool,
    error: Option<String>,
    /// Lines scrolled back from the newest.
    back: usize,
}

fn preview(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = flat.chars().take(PREVIEW_CHARS).collect();
    if flat.chars().count() > PREVIEW_CHARS {
        format!("{cut}\u{2026}")
    } else {
        cut
    }
}

fn block_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// The readable entries of a subagent transcript, oldest first: what it was asked, what it said,
/// the tools it called and a preview of what they returned. Thinking and bookkeeping rows are not
/// shown, and everything shown passes through the redactor.
fn parse(text: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut push = |kind: Kind, text: String| {
        let text = crate::commands::ctx::snapshot::redact_text(&text);
        if !text.trim().is_empty() {
            entries.push(Entry { kind, text });
        }
    };
    for line in text.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(content) = row.pointer("/message/content") else {
            continue;
        };
        match (row.get("type").and_then(Value::as_str), content) {
            (Some("user"), Value::String(text)) => push(Kind::User, preview(text)),
            (Some("user"), Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("tool_result") => {
                            let body = block.get("content").map(block_text).unwrap_or_default();
                            push(Kind::Result, preview(&body));
                        }
                        Some("text") => push(
                            Kind::User,
                            preview(block.get("text").and_then(Value::as_str).unwrap_or("")),
                        ),
                        _ => {}
                    }
                }
            }
            (Some("assistant"), Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => push(
                            Kind::Assistant,
                            preview(block.get("text").and_then(Value::as_str).unwrap_or("")),
                        ),
                        Some("tool_use") => {
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let input =
                                block.get("input").map(Value::to_string).unwrap_or_default();
                            push(Kind::Tool, preview(&format!("{name} {input}")));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    entries
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width)
        .take(ENTRY_LINES)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

impl SubagentView {
    pub(super) fn open(title: String, path: PathBuf) -> Self {
        let mut view = Self {
            title,
            path,
            stamp: None,
            checked: None,
            entries: Vec::new(),
            truncated: false,
            error: None,
            back: 0,
        };
        view.reload();
        view
    }

    fn reload(&mut self) {
        let read = || -> std::io::Result<(Option<Stamp>, String, bool)> {
            let mut file = std::fs::File::open(&self.path)?;
            let meta = file.metadata()?;
            let start = meta.len().saturating_sub(TAIL_BYTES);
            file.seek(SeekFrom::Start(start))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let mut text = String::from_utf8_lossy(&bytes).into_owned();
            if start > 0 {
                // The read began inside a row.
                text = text
                    .split_once('\n')
                    .map(|(_, rest)| rest.to_string())
                    .unwrap_or_default();
            }
            Ok((
                meta.modified().ok().map(|m| (m, meta.len())),
                text,
                start > 0,
            ))
        };
        match read() {
            Ok((stamp, text, truncated)) => {
                self.stamp = stamp;
                self.entries = parse(&text);
                self.truncated = truncated;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("cannot read the transcript: {e}")),
        }
    }

    /// Re-read the transcript when it changed, at most twice a second.
    pub(super) fn refresh(&mut self, now: Instant) {
        if self
            .checked
            .is_some_and(|at| now.duration_since(at) < REFRESH)
        {
            return;
        }
        self.checked = Some(now);
        let stamp = std::fs::metadata(&self.path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        if stamp != self.stamp {
            self.reload();
        }
    }

    pub(super) fn scroll(&mut self, delta: isize) {
        self.back = self.back.saturating_add_signed(-delta);
    }

    fn lines(&self, width: usize) -> Vec<(Style, String)> {
        let mut out = Vec::new();
        if self.truncated {
            out.push((
                pal::dim(),
                "\u{2026} earlier history is not shown".to_string(),
            ));
        }
        for entry in &self.entries {
            let (mark, style) = match entry.kind {
                Kind::User => ("asked  ", pal::fg(pal::AGENT)),
                Kind::Assistant => ("says   ", Style::default()),
                Kind::Tool => ("\u{25b8} ", pal::fg(pal::SEAT)),
                Kind::Result => ("  \u{21b3} ", pal::dim()),
            };
            for (i, line) in wrap(&entry.text, width.saturating_sub(mark.chars().count()))
                .into_iter()
                .enumerate()
            {
                let lead = if i == 0 {
                    mark.to_string()
                } else {
                    " ".repeat(mark.chars().count())
                };
                out.push((style, format!("{lead}{line}")));
            }
        }
        out
    }

    pub(super) fn paint(&self, buf: &mut Buffer, area: Rect) {
        let width = usize::from(area.width);
        let put = |buf: &mut Buffer, row: u16, text: &str, style: Style| {
            buf.set_stringn(area.x, area.y + row, text, width, style);
        };
        let head = format!(" \u{25cc} subagent \u{b7} {}", self.title);
        put(buf, 0, &head, pal::strong(pal::AGENT));
        let keys = "Esc back  \u{2191}\u{2193} PgUp PgDn scroll ";
        let keys_at = width.saturating_sub(keys.chars().count());
        if keys_at > head.chars().count() {
            buf.set_stringn(
                area.x + keys_at as u16,
                area.y,
                keys,
                width - keys_at,
                pal::dim(),
            );
        }
        if let Some(error) = &self.error {
            put(buf, 2, &format!("  {error}"), pal::fail());
            return;
        }
        if self.entries.is_empty() {
            put(buf, 2, "  nothing in this transcript yet", pal::dim());
            return;
        }
        let rows = usize::from(area.height).saturating_sub(2);
        let lines = self.lines(width.saturating_sub(2));
        let end = lines
            .len()
            .saturating_sub(self.back.min(lines.len().saturating_sub(1)));
        let start = end.saturating_sub(rows);
        for (i, (style, text)) in lines[start..end].iter().enumerate() {
            buf.set_stringn(
                area.x + 1,
                area.y + 2 + i as u16,
                text,
                width.saturating_sub(2),
                *style,
            );
        }
        if self.back > 0 {
            put(
                buf,
                area.height.saturating_sub(1),
                " \u{2193} newer below ",
                pal::dim(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSCRIPT: &str = concat!(
        r#"{"type":"user","message":{"role":"user","content":"Map the call sites"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"},{"type":"tool_use","name":"Grep","input":{"pattern":"foo"}}]}}"#,
        "\n",
        "not json\n",
        r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"src/a.rs:1"}]}]}}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"4 call sites"}]}}"#,
        "\n",
        r#"{"type":"system","subtype":"x"}"#,
        "\n",
    );

    #[test]
    fn a_transcript_reads_as_asked_tool_result_and_said_without_thinking_or_noise() {
        let kinds: Vec<(Kind, String)> = parse(TRANSCRIPT)
            .into_iter()
            .map(|e| (e.kind, e.text))
            .collect();
        assert_eq!(
            kinds,
            [
                (Kind::User, "Map the call sites".to_string()),
                (Kind::Tool, r#"Grep {"pattern":"foo"}"#.to_string()),
                (Kind::Result, "src/a.rs:1".to_string()),
                (Kind::Assistant, "4 call sites".to_string()),
            ]
        );
    }

    #[test]
    fn the_view_follows_the_file_and_paints_the_newest_rows_with_its_title() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("agent-x.jsonl");
        std::fs::write(&path, TRANSCRIPT).expect("write");
        let mut view = SubagentView::open("reads the code".into(), path.clone());
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        view.paint(&mut buf, area);
        let text: Vec<String> = (0..12)
            .map(|y| {
                (0..60)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        assert!(
            text[0].contains("subagent \u{b7} reads the code") && text[0].contains("Esc back"),
            "{}",
            text[0]
        );
        assert!(text.iter().any(|l| l.contains("4 call sites")), "{text:?}");
        // Appended rows show on the next refresh.
        let mut grown = TRANSCRIPT.to_string();
        grown.push_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"and one more"}]}}"#,
        );
        grown.push('\n');
        std::fs::write(&path, grown).expect("grow");
        view.refresh(Instant::now() + Duration::from_secs(5));
        let mut buf = Buffer::empty(area);
        view.paint(&mut buf, area);
        let after: String = (0..12)
            .flat_map(|y| (0..60).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(after.contains("and one more"), "{after}");
        // A missing file says so instead of drawing nothing.
        let gone = SubagentView::open("t".into(), dir.path().join("none.jsonl"));
        assert!(gone.error.is_some());
    }
}
