//! PreToolUse edit guard: denies, once per distinct command, a Bash interpreter
//! script that rewrites a tracked file, pointing the worker at the Edit tool.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::pretool_tier::{PreToolPayload, resolved_cwd};
use crate::commands::ctx::adapters::SESSION_ENV;
use crate::commands::ctx::config::EnvLookup;
use crate::commands::ctx::event::input_hash;
use crate::commands::ctx::state::StateDir;

/// Longest path literal considered a candidate.
const MAX_CANDIDATE_LEN: usize = 200;
/// Most candidates handed to one `git ls-files`.
const MAX_CANDIDATES: usize = 64;
/// Most denied-command hashes kept per session.
const MAX_DENIED_HASHES: usize = 200;
/// Longest a `git ls-files` probe may run before the guard allows.
const GIT_TIMEOUT: Duration = Duration::from_secs(3);

const INTERPRETERS: &[&str] = &["python", "python3", "py", "node", "ruby", "perl"];

const WRITE_CALLS: &[&str] = &[
    ".write(",
    "write_text(",
    "write_bytes(",
    "writeFileSync(",
    "writeFile(",
    "File.write(",
];

/// True when `token` (a command word) names one of the guarded interpreters.
fn is_interpreter(token: &str) -> bool {
    let base = token.rsplit(['/', '\\']).next().unwrap_or(token);
    let base = base.strip_suffix(".exe").unwrap_or(base);
    if INTERPRETERS.contains(&base) {
        return true;
    }
    base.strip_prefix("python3.")
        .is_some_and(|minor| !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit()))
}

/// True when `segment` starts an interpreter with an inline script (heredoc,
/// `-c` or `-e`), after optional leading `VAR=value` assignments.
fn segment_is_inline_interpreter(segment: &str) -> bool {
    let mut tokens = segment.split_whitespace().peekable();
    while let Some(token) = tokens.peek() {
        let is_assignment = token
            .split_once('=')
            .is_some_and(|(name, _)| !name.is_empty() && name.bytes().all(is_env_name_byte));
        if !is_assignment {
            break;
        }
        tokens.next();
    }
    let Some(first) = tokens.next() else {
        return false;
    };
    if !is_interpreter(first) {
        return false;
    }
    segment.contains("<<") || tokens.any(|t| t == "-c" || t == "-e")
}

fn is_env_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn invokes_inline_interpreter(command: &str) -> bool {
    command
        .split(['\n', ';', '|'])
        .flat_map(|part| part.split("&&"))
        .any(segment_is_inline_interpreter)
}

/// Every single- or double-quoted literal in `text`, contents only.
fn quoted_literals(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut open: Option<(char, usize)> = None;
    let mut skip_next = false;
    for (i, c) in text.char_indices() {
        if skip_next {
            skip_next = false;
            continue;
        }
        match open {
            Some((quote, start)) => {
                if c == '\\' {
                    skip_next = true;
                } else if c == quote {
                    out.push(&text[start..i]);
                    open = None;
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    open = Some((c, i + c.len_utf8()));
                }
            }
        }
    }
    out
}

/// True for a file mode literal that writes: `w`, `a`, `x`, `+` variants.
fn is_write_mode(literal: &str) -> bool {
    let mut bytes = literal.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !matches!(first, b'r' | b'w' | b'a' | b'x') || literal.len() > 3 {
        return false;
    }
    let rest = &literal[1..];
    if !rest.bytes().all(|b| matches!(b, b'b' | b't' | b'+')) {
        return false;
    }
    first != b'r' || rest.contains('+')
}

/// True when the text between an `open(` and its closing paren names a
/// write/append/update mode.
fn open_call_writes(command: &str) -> bool {
    let mut search = command;
    while let Some(at) = search.find("open(") {
        let after = &search[at + "open(".len()..];
        let mut depth = 0usize;
        let mut end = after.len();
        for (i, c) in after.char_indices() {
            match c {
                '(' => depth += 1,
                ')' if depth == 0 => {
                    end = i;
                    break;
                }
                ')' => depth -= 1,
                _ => {}
            }
        }
        if quoted_literals(&after[..end])
            .into_iter()
            .any(is_write_mode)
        {
            return true;
        }
        search = after;
    }
    false
}

fn contains_file_write(command: &str) -> bool {
    WRITE_CALLS.iter().any(|call| command.contains(call)) || open_call_writes(command)
}

/// Pure detector: the path literals of an inline interpreter script that
/// writes files. Empty when the command is not such a script.
pub(super) fn shell_edit_candidates(command: &str) -> Vec<String> {
    if !invokes_inline_interpreter(command) || !contains_file_write(command) {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    // A `-c`/`-e` script is itself one quoted literal; its own quoted
    // literals (one level deep) are where the paths are.
    let literals = quoted_literals(command).into_iter().flat_map(|outer| {
        let inner = if outer.contains(['\'', '"']) {
            quoted_literals(outer)
        } else {
            Vec::new()
        };
        std::iter::once(outer).chain(inner)
    });
    for literal in literals {
        if literal.len() > MAX_CANDIDATE_LEN
            || literal.chars().any(char::is_whitespace)
            || !(literal.contains('/') || literal.contains('.'))
            || out.iter().any(|seen| seen == literal)
        {
            continue;
        }
        out.push(literal.to_string());
        if out.len() >= MAX_CANDIDATES {
            break;
        }
    }
    out
}

/// A pathspec-safe form of `candidate` relative to `cwd`: absolute paths
/// under `cwd` are made relative; every other rooted form (leading `/` or
/// `\`, a drive letter, `~`) and any `..` component is dropped, since git
/// fails the whole probe on a path outside the repository.
fn cwd_relative(candidate: &str, cwd: &Path) -> Option<String> {
    let path = Path::new(candidate);
    let rel = if path.is_absolute() {
        path.strip_prefix(cwd)
            .ok()?
            .to_string_lossy()
            .replace('\\', "/")
    } else {
        candidate.to_string()
    };
    let rooted = rel.starts_with(['/', '\\', '~'])
        || rel.as_bytes().get(1) == Some(&b':') && rel.as_bytes()[0].is_ascii_alphabetic();
    if rooted || rel.split(['/', '\\']).any(|part| part == "..") {
        return None;
    }
    Some(rel)
}

/// I/O: one `git ls-files` over the candidates. `None` on any doubt (no git,
/// not a repo, non-zero exit, timeout); otherwise the tracked paths git
/// reported (repo-relative).
fn tracked_among(cwd: &Path, pathspecs: &[String]) -> Option<Vec<String>> {
    let mut command = crate::commands::ctx::worktree::git_command(cwd);
    command
        .args(["ls-files", "-z", "--full-name", "--"])
        .args(pathspecs.iter().map(|p| format!(":(literal){p}")))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let mut pipe = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let stdout = rx.recv_timeout(Duration::from_secs(1)).ok()?;
    Some(
        stdout
            .split(|&b| b == 0)
            .filter(|chunk| !chunk.is_empty())
            .filter_map(|chunk| std::str::from_utf8(chunk).ok())
            .map(str::to_string)
            .collect(),
    )
}

/// The candidate to name in the reason: the first one matching a tracked
/// path, else the first tracked path.
fn first_tracked_name(candidates: &[String], tracked: &[String]) -> String {
    let normalized = |c: &str| c.trim_start_matches("./").replace('\\', "/");
    candidates
        .iter()
        .find(|c| {
            let c = normalized(c);
            tracked
                .iter()
                .any(|t| t == &c || t.ends_with(&format!("/{c}")))
        })
        .cloned()
        .unwrap_or_else(|| tracked.first().cloned().unwrap_or_default())
}

fn denied_record_path(state: &StateDir, session: &str) -> PathBuf {
    state
        .root()
        .join("edit-guard")
        .join(format!("{:016x}.json", input_hash(session)))
}

/// `None` when the record cannot be read (corrupt or unreadable); a missing
/// record is an empty list.
fn load_denied(path: &Path) -> Option<Vec<String>> {
    match std::fs::read_to_string(path) {
        Ok(body) => serde_json::from_str(&body).ok(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(Vec::new()),
        Err(_) => None,
    }
}

fn save_denied(path: &Path, hashes: &[String]) -> bool {
    let Ok(json) = serde_json::to_string(hashes) else {
        return false;
    };
    if let Some(dir) = path.parent() {
        let _ = crate::commands::ctx::state::create_private_dir_all(dir);
    }
    crate::commands::ctx::state::write_private(path, &json).is_ok()
}

/// The deny reason when this Bash command is an interpreter script that
/// rewrites a tracked file in a zirv-supervised session and has not been
/// denied before in it. Every doubt allows.
pub(super) fn shell_edit_guard_reason(
    payload: &PreToolPayload,
    env: EnvLookup<'_>,
) -> Option<String> {
    let session = env(SESSION_ENV).filter(|s| !s.is_empty())?;
    let command = payload.tool_input.command.as_str();
    let candidates = shell_edit_candidates(command);
    if candidates.is_empty() {
        return None;
    }
    let state = StateDir::resolve(env).ok()?;
    let record = denied_record_path(&state, &session);
    let mut denied = load_denied(&record)?;
    let hash = format!("{:016x}", input_hash(command));
    if denied.contains(&hash) {
        return None;
    }
    let cwd = resolved_cwd(payload)?;
    let pathspecs: Vec<String> = candidates
        .iter()
        .filter_map(|c| cwd_relative(c, &cwd))
        .collect();
    if pathspecs.is_empty() {
        return None;
    }
    let tracked = tracked_among(&cwd, &pathspecs).filter(|t| !t.is_empty())?;
    denied.push(hash);
    if denied.len() > MAX_DENIED_HASHES {
        denied.remove(0);
    }
    if !save_denied(&record, &denied) {
        return None;
    }
    let path = first_tracked_name(&candidates, &tracked);
    Some(format!(
        "zirv edit guard: this script rewrites the tracked file {path}. Use the Edit tool instead: Read the lines you will change (offset/limit), then Edit them -- scripted string splices break escapes and indentation. If this command does not modify a tracked file, re-run it unchanged and it will be allowed."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_splice_shape_yields_its_path_literal() {
        let command = "python - <<'EOF'\np='ledgerlite/cli.py'\ns=open(p).read()\ns=s.replace('a','b')\nopen(p,'w').write(s)\nEOF\npython -m pytest -q";
        assert_eq!(shell_edit_candidates(command), vec!["ledgerlite/cli.py"]);
    }

    #[test]
    fn paths_inside_a_quoted_c_or_e_script_are_candidates() {
        let py = "python -c \"p='pkg/a.py';open(p,'w').write(open(p).read().replace('a','b'))\"";
        assert!(shell_edit_candidates(py).contains(&"pkg/a.py".to_string()));
        let js = "node -e 'require(\"fs\").writeFileSync(\"pkg/a.js\", \"x\")'";
        assert!(shell_edit_candidates(js).contains(&"pkg/a.js".to_string()));
    }

    #[test]
    fn non_edits_yield_no_candidates() {
        for command in [
            "python -m pytest -q",
            "python - <<'EOF'\nprint(open('a.py').read())\nEOF",
            "cat > notes.txt <<'EOF'",
            "sed -n 1,20p src/a.py",
        ] {
            assert!(
                shell_edit_candidates(command).is_empty(),
                "must not flag {command:?}"
            );
        }
    }
}
