//! pty input support for the interactive supervisor.

use super::*;

/// Gives the Unix stdin pump a bounded wake-up so ownership of the real
/// terminal can move to a native successor. `read` remains the byte-preserving
/// path; `poll` only says whether it would block.
#[cfg(unix)]
pub(super) fn stdin_ready() -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: STDIN_FD,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `descriptor` points to one initialized `pollfd` for the duration
    // of this call, and `STDIN_FD` is borrowed rather than closed or replaced.
    let result = unsafe { libc::poll(&mut descriptor, 1, PUMP_POLL.as_millis() as i32) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(result > 0
        && descriptor.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)
            != 0)
}

/// A Cursor Position Report for row 1, column 1 -- the reply a terminal sends
/// when something asks it where the cursor is with `ESC[6n`.
///
/// DO NOT DELETE THIS. portable-pty 0.9.0 creates every Windows pseudoconsole
/// with `PSUEDOCONSOLE_INHERIT_CURSOR` hard-coded (portable-pty
/// `src/win/psuedocon.rs`, not reachable through `openpty`). That flag makes
/// conhost emit `ESC[6n` on the pty and then *block* until a Cursor Position
/// Report comes back on the pty's input pipe -- before it services the child at
/// all. Nothing in `wrap` ever answered, so every wrapped command hung forever
/// on Windows; even `wrap --no-supervise -- cmd /c exit 0` never exited.
/// Writing this one synthetic reply into the master unblocks the console host.
///
/// It is written once per pty generation, which includes the relaunch path: a
/// restart opens a brand-new pseudoconsole that deadlocks exactly the same way.
///
/// It stays even now that raw mode works and a real terminal could answer for
/// itself, because `wrap` also has to run with stdin redirected (CI, a pipe),
/// where there is no terminal to answer at all. `CprFilter` below keeps the two
/// replies from colliding.
#[cfg(windows)]
const CURSOR_POSITION_REPORT: &[u8] = b"\x1b[1;1R";

/// Answers the `PSUEDOCONSOLE_INHERIT_CURSOR` probe described on
/// `CURSOR_POSITION_REPORT`. A no-op everywhere else: no other platform's pty
/// asks anything before it will run a child.
///
/// `pub(in crate::commands::ctx)` rather than private: `dash::pane`'s own PTY
/// spawn (the same deadlock, same fix) reuses this exact function instead of
/// duplicating it.
pub(in crate::commands::ctx) fn answer_inherit_cursor_probe(writer: &mut (dyn Write + Send)) {
    #[cfg(windows)]
    {
        let _ = writer.write_all(CURSOR_POSITION_REPORT);
        let _ = writer.flush();
    }
    #[cfg(not(windows))]
    let _ = writer;
}

/// How long after a pty is opened a Cursor Position Report arriving on stdin is
/// assumed to be the answer to that pty's own `ESC[6n`. Generous: it covers a
/// terminal that is slow to answer, and still expires long before a TUI could
/// plausibly ask for the cursor itself and want the reply.
#[cfg(windows)]
const CPR_FILTER_WINDOW: Duration = Duration::from_secs(3);

/// Swallows the *real* terminal's answer to the console-host probe.
///
/// `wrap` forwards the inner pty's output verbatim, so the `ESC[6n` from
/// `CURSOR_POSITION_REPORT`'s story also reaches the user's own terminal, which
/// -- now that raw mode works -- dutifully answers it on our stdin. The inner
/// console was already satisfied by the synthetic reply, so forwarding that
/// answer would type `ESC[24;1R` into the agent's TUI as if the user had. One
/// report per pty generation is dropped, and only inside a short window after
/// that pty opened, so a report the agent itself asked for later still arrives.
#[derive(Debug, Default)]
pub struct CprFilter {
    armed_until: Option<Instant>,
}

impl CprFilter {
    /// Armed on Windows only: no other platform's pty sends the probe, so on
    /// unix this filter is permanently inert and stdin is passed through byte
    /// for byte.
    pub fn arm(&mut self, now: Instant) {
        #[cfg(windows)]
        {
            self.armed_until = Some(now + CPR_FILTER_WINDOW);
        }
        #[cfg(not(windows))]
        let _ = now;
    }

    /// Returns the bytes to forward. Borrows unless something was actually
    /// removed, so the common path copies nothing.
    pub fn filter<'a>(&mut self, bytes: &'a [u8], now: Instant) -> std::borrow::Cow<'a, [u8]> {
        let Some(until) = self.armed_until else {
            return std::borrow::Cow::Borrowed(bytes);
        };
        if now >= until {
            self.armed_until = None;
            return std::borrow::Cow::Borrowed(bytes);
        }
        let Some(range) = find_cursor_position_report(bytes) else {
            return std::borrow::Cow::Borrowed(bytes);
        };
        // One report is all the probe can produce; anything later in this
        // session is the agent's own business.
        self.armed_until = None;
        let mut kept = Vec::with_capacity(bytes.len() - range.len());
        kept.extend_from_slice(&bytes[..range.start]);
        kept.extend_from_slice(&bytes[range.end..]);
        std::borrow::Cow::Owned(kept)
    }
}

/// Locates one `ESC [ <rows> ; <cols> R` in `bytes`. Deliberately strict about
/// the shape: anything else beginning with `ESC[` is a key the user pressed.
fn find_cursor_position_report(bytes: &[u8]) -> Option<std::ops::Range<usize>> {
    let mut at = 0;
    while at + 1 < bytes.len() {
        if bytes[at] != 0x1b || bytes[at + 1] != b'[' {
            at += 1;
            continue;
        }
        let mut cursor = at + 2;
        let mut digits = 0;
        let mut semicolons = 0;
        while let Some(byte) = bytes.get(cursor) {
            match byte {
                b'0'..=b'9' => digits += 1,
                b';' => semicolons += 1,
                b'R' if digits > 0 && semicolons == 1 => return Some(at..cursor + 1),
                _ => break,
            }
            cursor += 1;
        }
        at += 1;
    }
    None
}

/// The markers a bracketed-paste-aware terminal wraps pasted text in.
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// How much of an unterminated paste span is held before it is flushed as-is.
/// Far above any plausible paste, so a real one is never split; low enough
/// that a terminal that sends a start marker and no end marker cannot make
/// `wrap` eat unbounded memory.
const PASTE_SPAN_CAP: usize = 1024 * 1024;

/// The other half of that backstop. A paste arrives in one burst, so a span
/// still open this long after it opened is a marker the terminal never
/// closed -- and holding the operator's keystrokes any longer for it would be
/// exactly the "wrap made the session worse" failure this guard exists to
/// prevent.
const PASTE_SPAN_MAX: Duration = Duration::from_secs(5);

/// The shortest trailing fragment of `PASTE_START` that is held back waiting
/// for the rest of the marker. `ESC` and `ESC [` are deliberately below the
/// floor: they are the start of Esc-to-interrupt and of every arrow key, and
/// withholding those until the *next* keystroke would break the session in a
/// far more visible way than an unrecognised paste ever could. Nothing is
/// lost by forwarding them: [`PasteGuard::forwarded_start_prefix`] remembers
/// that it did, so a marker split there still opens its span.
const MIN_HELD_MARKER_PREFIX: usize = 3;

/// Keeps a bracketed paste whole on the operator-input path.
///
/// Issue #206. A terminal wraps pasted text in `ESC[200~ ... ESC[201~` so the
/// agent's composer can tell a paste from typing and keep the pasted newlines
/// as newlines rather than submitting a turn per line. `wrap` reads the
/// operator's console 4096 bytes at a time and writes each read straight to
/// the child's pty, which leaves two ways for that span to come apart:
///
/// - A read boundary can fall inside a marker, so the child is handed
///   `ESC[20` and `0~...` and never sees a paste at all.
/// - The stdin pump and the injector share one writer. Without this guard an
///   injected advisory can be written *between* two chunks of a paste, i.e.
///   inside the span, which is the same corruption from the other side.
///
/// So a span is accumulated here and written once, markers included and
/// contents byte-for-byte untouched: no `\r`/`\n` translation, no synthetic
/// Enter. Outside a span this is a passthrough that copies nothing.
///
/// Pure, like [`CprFilter`]: `now` is passed in rather than read, so every
/// verdict is reproducible in a test.
#[derive(Debug, Default)]
pub struct PasteGuard {
    /// Either a trailing fragment of a start marker (when no span is open) or
    /// the whole of the open span, start marker included.
    held: Vec<u8>,
    /// When the currently open span's start marker arrived.
    span_started: Option<Instant>,
    /// How many bytes of `PASTE_START` the *previous* read ended on and this
    /// guard forwarded anyway because they were shorter than
    /// [`MIN_HELD_MARKER_PREFIX`] -- so 1 (`ESC`) or 2 (`ESC [`), or `None`.
    ///
    /// Those bytes are not held back, but they are still remembered: if the
    /// very next read opens with the rest of the marker, the span genuinely
    /// started back there and has to be opened, or the body streams through
    /// read by read and the one-write guarantee is lost exactly where the
    /// terminal split hardest. Cleared by the next read whatever it turns out
    /// to be, so the memory can never span more than one read.
    forwarded_start_prefix: Option<usize>,
}

impl PasteGuard {
    /// Returns the bytes to forward: the empty slice while a span is still
    /// being accumulated, the whole span the moment it closes, and otherwise
    /// exactly what came in.
    pub fn filter<'a>(&mut self, bytes: &'a [u8], now: Instant) -> std::borrow::Cow<'a, [u8]> {
        if bytes.is_empty() {
            return std::borrow::Cow::Borrowed(bytes);
        }
        // Backstop first: a span the terminal never closed must not hold this
        // read -- or any read after it -- hostage.
        if let Some(started) = self.span_started
            && now.duration_since(started) >= PASTE_SPAN_MAX
        {
            self.span_started = None;
            let mut flushed = std::mem::take(&mut self.held);
            flushed.extend_from_slice(bytes);
            self.forwarded_start_prefix = forwarded_start_prefix(&flushed);
            return std::borrow::Cow::Owned(flushed);
        }
        // The previous read ended on an `ESC` or `ESC [` that was forwarded
        // rather than held (see `MIN_HELD_MARKER_PREFIX`). If this read opens
        // with the rest of the start marker, that really was a paste
        // beginning: open the span here, and gather from this read on. The
        // marker's first bytes are already on the wire, so the child still
        // receives the whole of it, just as two writes rather than one.
        //
        // `take` runs however the rest of the condition goes, which is what
        // clears the memory for every read that is not the continuation.
        if let Some(sent) = self.forwarded_start_prefix.take()
            && self.span_started.is_none()
            && self.held.is_empty()
            && bytes.starts_with(&PASTE_START[sent..])
        {
            self.span_started = Some(now);
        }
        // The overwhelmingly common read: no span open, nothing held, and not
        // a byte that could begin a marker. Copies nothing. Deliberately
        // *after* the check above -- the tail of a start marker (`200~`)
        // carries no `ESC` of its own and would slip straight through here.
        if self.span_started.is_none() && self.held.is_empty() && !bytes.contains(&ESC) {
            return std::borrow::Cow::Borrowed(bytes);
        }

        // `held` is either a marker fragment or the open span so far, and in
        // both cases it belongs immediately before this read.
        let mut work = std::mem::take(&mut self.held);
        work.extend_from_slice(bytes);
        let mut out: Vec<u8> = Vec::with_capacity(work.len());
        let mut at = 0usize;
        loop {
            if self.span_started.is_some() {
                match find_marker(&work[at..], PASTE_END) {
                    Some(offset) => {
                        let end = at + offset + PASTE_END.len();
                        out.extend_from_slice(&work[at..end]);
                        self.span_started = None;
                        at = end;
                    }
                    None if work.len() - at > PASTE_SPAN_CAP => {
                        // Over the cap the span is abandoned, not truncated:
                        // every byte still goes to the child, just no longer
                        // as one write.
                        out.extend_from_slice(&work[at..]);
                        self.span_started = None;
                        break;
                    }
                    None => {
                        self.held.extend_from_slice(&work[at..]);
                        break;
                    }
                }
            } else {
                match find_marker(&work[at..], PASTE_START) {
                    Some(offset) => {
                        out.extend_from_slice(&work[at..at + offset]);
                        at += offset;
                        self.span_started = Some(now);
                    }
                    None => {
                        let keep = trailing_marker_prefix(&work[at..], PASTE_START);
                        out.extend_from_slice(&work[at..work.len() - keep]);
                        self.held
                            .extend_from_slice(&work[work.len() - keep..work.len()]);
                        break;
                    }
                }
            }
            if at >= work.len() {
                break;
            }
        }
        // Only a read that ends outside a span, holding nothing, can leave a
        // forwarded fragment behind to remember.
        self.forwarded_start_prefix = if self.span_started.is_none() && self.held.is_empty() {
            forwarded_start_prefix(&out)
        } else {
            None
        };
        std::borrow::Cow::Owned(out)
    }
}

/// The length of the 1- or 2-byte tail of `bytes` that is a prefix of
/// [`PASTE_START`] -- the fragments [`trailing_marker_prefix`] deliberately
/// forwards instead of holding. `None` when the read ends on anything else.
fn forwarded_start_prefix(bytes: &[u8]) -> Option<usize> {
    if bytes.ends_with(&PASTE_START[..2]) {
        Some(2)
    } else if bytes.ends_with(&PASTE_START[..1]) {
        Some(1)
    } else {
        None
    }
}

/// `ESC`, the only byte either marker can start with.
const ESC: u8 = 0x1b;

/// The first offset in `bytes` where `marker` appears whole. Scans from the
/// `ESC`s rather than every position, so an open span re-scanned across
/// successive reads stays cheap.
fn find_marker(bytes: &[u8], marker: &[u8]) -> Option<usize> {
    let mut at = 0;
    while at + marker.len() <= bytes.len() {
        at += bytes[at..].iter().position(|byte| *byte == ESC)?;
        if at + marker.len() > bytes.len() {
            return None;
        }
        if &bytes[at..at + marker.len()] == marker {
            return Some(at);
        }
        at += 1;
    }
    None
}

/// How many bytes at the end of `bytes` are a strict, long-enough prefix of
/// `marker` -- i.e. how much has to be held back in case the rest of the
/// marker is in the next read. Zero unless the tail is at least
/// [`MIN_HELD_MARKER_PREFIX`] bytes long; see that constant for why the
/// shorter fragments are forwarded instead.
fn trailing_marker_prefix(bytes: &[u8], marker: &[u8]) -> usize {
    let longest = (marker.len() - 1).min(bytes.len());
    let mut len = longest;
    while len >= MIN_HELD_MARKER_PREFIX {
        if bytes[bytes.len() - len..] == marker[..len] {
            return len;
        }
        len -= 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Issue #206: a bracketed paste must reach the wrapped composer as one
    /// block, whatever the console's read boundaries do to it.
    mod bracketed_paste {
        use super::*;

        fn feed(guard: &mut PasteGuard, chunks: &[&[u8]], now: Instant) -> Vec<Vec<u8>> {
            chunks
                .iter()
                .map(|chunk| guard.filter(chunk, now).into_owned())
                .filter(|out| !out.is_empty())
                .collect()
        }

        /// The whole point: however the reads fall, the child is handed the
        /// span in exactly one write, markers included.
        #[test]
        fn a_span_split_across_reads_is_forwarded_as_one_write() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            let writes = feed(
                &mut guard,
                &[b"\x1b[200~one\rtwo", b"\rthree\x1b[201~"],
                now,
            );
            assert_eq!(
                writes,
                vec![b"\x1b[200~one\rtwo\rthree\x1b[201~".to_vec()],
                "one write, markers preserved"
            );
        }

        /// The nastiest boundary: the marker itself is cut in half.
        #[test]
        fn a_marker_cut_in_half_by_a_read_boundary_still_forms_one_span() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            let writes = feed(
                &mut guard,
                &[b"\x1b[20", b"0~alpha\rbeta\x1b[20", b"1~"],
                now,
            );
            assert_eq!(
                writes,
                vec![b"\x1b[200~alpha\rbeta\x1b[201~".to_vec()],
                "a marker split across two reads is still one span"
            );
        }

        /// Pasted line endings are data, never submissions: nothing in here
        /// may rewrite a `\r` or an `\n` or add one.
        #[test]
        fn line_endings_inside_a_span_are_untouched() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            let body: &[u8] = b"a\r\nb\nc\rd";
            let mut input = PASTE_START.to_vec();
            input.extend_from_slice(body);
            input.extend_from_slice(PASTE_END);
            let out = guard.filter(&input, now).into_owned();
            assert_eq!(out, input, "byte for byte, CR and LF included");
        }

        /// Outside a span the guard is invisible, and costs no copy.
        #[test]
        fn bytes_outside_a_span_pass_straight_through() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            let out = guard.filter(b"hello\r", now);
            assert_eq!(out.as_ref(), b"hello\r");
            assert!(matches!(out, std::borrow::Cow::Borrowed(_)), "no copy");
        }

        /// Esc is how the operator interrupts an agent. Holding it back
        /// waiting to see whether it grows into a paste marker would be a
        /// worse bug than the one this guard fixes.
        #[test]
        fn a_lone_escape_is_never_held_back() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            assert_eq!(guard.filter(b"\x1b", now).as_ref(), b"\x1b");
            assert_eq!(guard.filter(b"\x1b[", now).as_ref(), b"\x1b[");
            assert_eq!(guard.filter(b"\x1b[A", now).as_ref(), b"\x1b[A");
        }

        /// The other half of that rule. `ESC` goes out immediately, but the
        /// guard remembers it did: when the rest of the start marker turns up
        /// in the next read the span really did begin back there, so the body
        /// is still gathered into one write rather than streaming through.
        #[test]
        fn a_start_marker_split_after_the_escape_still_opens_one_span() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            // The body is deliberately spread over a further read: without
            // the span the guard would stream those reads straight through.
            let writes = feed(&mut guard, &[b"\x1b", b"[200~a\r", b"\nb\x1b[201~"], now);
            assert_eq!(
                writes,
                vec![b"\x1b".to_vec(), b"[200~a\r\nb\x1b[201~".to_vec()],
                "the escape is already on the wire; the rest arrives as one write"
            );
            assert_eq!(
                writes.concat(),
                b"\x1b[200~a\r\nb\x1b[201~".to_vec(),
                "and nothing is lost, added or rewritten"
            );
        }

        /// Same, split one byte later -- and with the body spread over a
        /// further read, to show it is genuinely accumulated.
        #[test]
        fn a_start_marker_split_after_the_csi_introducer_still_opens_one_span() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            let writes = feed(&mut guard, &[b"\x1b[", b"200~a\r", b"\nb\x1b[201~"], now);
            assert_eq!(
                writes,
                vec![b"\x1b[".to_vec(), b"200~a\r\nb\x1b[201~".to_vec()]
            );
            assert_eq!(writes.concat(), b"\x1b[200~a\r\nb\x1b[201~".to_vec());
        }

        /// The memory lasts exactly one read: an escape followed by anything
        /// else is two ordinary reads and stays that way.
        #[test]
        fn an_escape_not_followed_by_a_marker_leaves_the_next_read_untouched() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            let writes = feed(&mut guard, &[b"\x1b", b"OK"], now);
            assert_eq!(writes, vec![b"\x1b".to_vec(), b"OK".to_vec()]);
            // And the stale memory is gone: a bare `200~` later is just text.
            assert_eq!(guard.filter(b"200~", now).as_ref(), b"200~");
        }

        /// The end marker needs no such memory: inside an open span every
        /// byte is accumulated already, a trailing `ESC` included, so a split
        /// end marker is reassembled by the next read on its own.
        #[test]
        fn an_end_marker_split_after_the_escape_is_reassembled_inside_the_span() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            let writes = feed(&mut guard, &[b"\x1b[200~body\x1b", b"[201~"], now);
            assert_eq!(writes, vec![b"\x1b[200~body\x1b[201~".to_vec()]);
        }

        /// A fragment held for a marker that never arrives is released with
        /// the bytes that proved it was not one -- in order, unmodified.
        #[test]
        fn a_held_fragment_that_is_not_a_marker_is_released_intact() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            let writes = feed(&mut guard, &[b"\x1b[2", b"~rest"], now);
            assert_eq!(writes, vec![b"\x1b[2~rest".to_vec()]);
        }

        /// A start marker with no end marker must not swallow the session:
        /// past the cap the span is flushed exactly as it arrived.
        #[test]
        fn an_unterminated_span_is_flushed_once_it_outgrows_the_cap() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            assert!(guard.filter(PASTE_START, now).is_empty(), "span opened");
            let bulk = vec![b'x'; PASTE_SPAN_CAP + 1];
            let out = guard.filter(&bulk, now).into_owned();
            assert!(out.starts_with(PASTE_START), "the marker is not eaten");
            assert_eq!(
                out.len(),
                PASTE_START.len() + bulk.len(),
                "everything held is flushed as-is"
            );
            // The span is over, so ordinary keys flow again.
            assert_eq!(guard.filter(b"q", now).as_ref(), b"q");
        }

        /// The same backstop in the time dimension, for a span that stays
        /// small but never closes.
        #[test]
        fn an_unterminated_span_is_flushed_once_its_deadline_passes() {
            let mut guard = PasteGuard::default();
            let opened = Instant::now();
            assert!(guard.filter(b"\x1b[200~typed", opened).is_empty());
            let out = guard
                .filter(b"more", opened + PASTE_SPAN_MAX + Duration::from_secs(1))
                .into_owned();
            assert_eq!(out, b"\x1b[200~typedmore".to_vec());
            assert_eq!(guard.filter(b"q", Instant::now()).as_ref(), b"q");
        }

        /// Text either side of a span rides along in the right order, and a
        /// second paste opens a second span.
        #[test]
        fn spans_and_ordinary_typing_keep_their_order() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            let writes = feed(
                &mut guard,
                &[b"ab\x1b[200~in\x1b[201~cd\x1b[200~two\x1b[201~ef"],
                now,
            );
            assert_eq!(
                writes,
                vec![b"ab\x1b[200~in\x1b[201~cd\x1b[200~two\x1b[201~ef".to_vec()]
            );
        }

        /// A stray *end* marker outside a span is not a span: it is just
        /// bytes, and it is forwarded like any other.
        #[test]
        fn a_stray_end_marker_is_not_treated_as_a_span() {
            let mut guard = PasteGuard::default();
            let now = Instant::now();
            assert_eq!(guard.filter(PASTE_END, now).as_ref(), PASTE_END);
        }
    }

    /// The `ESC[6n` story: see `CURSOR_POSITION_REPORT`.
    mod cursor_position_report {
        use super::*;

        #[test]
        fn a_cursor_position_report_is_recognised_wherever_it_sits_in_a_chunk() {
            assert_eq!(find_cursor_position_report(b"\x1b[1;1R"), Some(0..6));
            assert_eq!(find_cursor_position_report(b"ab\x1b[24;80Rcd"), Some(2..10));
        }

        /// Every other escape sequence is a key the user pressed and has to
        /// reach the agent untouched.
        #[test]
        fn ordinary_keys_are_never_mistaken_for_a_cursor_report() {
            assert_eq!(find_cursor_position_report(b"\x1b[A"), None, "up arrow");
            assert_eq!(find_cursor_position_report(b"\x1b[3~"), None, "delete");
            assert_eq!(find_cursor_position_report(b"\x1b"), None, "bare escape");
            assert_eq!(find_cursor_position_report(b"\x1b[R"), None, "no row");
            assert_eq!(
                find_cursor_position_report(b"\x1b[12R"),
                None,
                "a report has two parameters"
            );
            assert_eq!(find_cursor_position_report(b"hello"), None);
        }

        /// An unarmed filter is a pure passthrough, which is the whole of its
        /// behavior on unix: no pty there sends the probe.
        #[test]
        fn an_unarmed_filter_forwards_everything() {
            let mut filter = CprFilter::default();
            let out = filter.filter(b"\x1b[1;1R", Instant::now());
            assert_eq!(out.as_ref(), b"\x1b[1;1R");
            assert!(matches!(out, std::borrow::Cow::Borrowed(_)), "no copy");
        }

        #[cfg(not(windows))]
        #[test]
        fn arming_is_inert_off_windows() {
            let mut filter = CprFilter::default();
            filter.arm(Instant::now());
            assert_eq!(
                filter.filter(b"\x1b[1;1R", Instant::now()).as_ref(),
                b"\x1b[1;1R"
            );
        }

        #[cfg(windows)]
        #[test]
        fn an_armed_filter_swallows_exactly_one_report_and_keeps_the_rest() {
            let mut filter = CprFilter::default();
            filter.arm(Instant::now());

            // The terminal's answer to the console host's probe, with a real
            // keystroke riding along in the same read.
            let out = filter.filter(b"\x1b[24;1Rq", Instant::now());
            assert_eq!(out.as_ref(), b"q", "only the report is removed");

            // The next one is the agent's own business.
            assert_eq!(
                filter.filter(b"\x1b[2;3R", Instant::now()).as_ref(),
                b"\x1b[2;3R"
            );
        }

        #[cfg(windows)]
        #[test]
        fn an_armed_filter_stops_filtering_once_its_window_has_passed() {
            let mut filter = CprFilter::default();
            filter.arm(Instant::now() - CPR_FILTER_WINDOW - Duration::from_secs(1));
            assert_eq!(
                filter.filter(b"\x1b[1;1R", Instant::now()).as_ref(),
                b"\x1b[1;1R",
                "a late report belongs to the agent"
            );
        }
    }
}
