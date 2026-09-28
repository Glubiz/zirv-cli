//! `PreToolUse`-adjacent permission-prompt hook: payload parsing, the
//! command-family/attention-line heuristics it logs by, and the run entry.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::adapters::SOCKET_ENV;
use crate::commands::ctx::config::{CtxConfig, EnvLookup};
use crate::commands::ctx::state::{StateDir, now_secs};

pub(super) fn hook_obfuscation_options(
    cfg: &CtxConfig,
) -> CtxResult<crate::commands::ctx::obfuscate::Options> {
    let home = crate::utils::home_dir()?;
    crate::commands::ctx::obfuscate_store::options_from_config(&cfg.obfuscate, &home)
}

pub(super) fn finding_kinds(findings: &[crate::commands::ctx::obfuscate::Finding]) -> String {
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for finding in findings {
        *counts.entry(&finding.kind).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(kind, count)| format!("{kind}:{count}"))
        .collect::<Vec<_>>()
        .join(",")
}

const PERMISSION_PROMPTS_FILE: &str = "permission-prompts.jsonl";

/// The short id issue #349's [`crate::commands::ctx::attention`] observations are filed
/// under -- the same stable short a session's registry record uses
/// (`sessions::Record::short`), recovered the same way `run_stop`'s own
/// inline `stable_short` is (via the bound turn-signal socket's file stem,
/// which does not rotate across an internal restart), falling back to
/// `sessions::short_id` of whatever session id this hook call carries when
/// no socket was ever bound (an unsupervised launch, or a hook that fired
/// before one existed). Deliberately its own small function rather than a
/// refactor of `run_stop`'s existing inline derivation (no drive-by
/// refactors) -- this codebase already accepts exactly this kind of
/// duplication for this exact derivation; see `sessions::short_id`'s own
/// doc comment.
pub(super) fn attention_short(env: EnvLookup<'_>, session_id_fallback: &str) -> String {
    env(SOCKET_ENV)
        .and_then(|raw| {
            Path::new(&raw)
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| crate::commands::ctx::sessions::short_id(session_id_fallback))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct PermissionHookPayload {
    session_id: String,
    cwd: String,
    permission_mode: String,
    hook_event_name: Option<String>,
    reason: Option<String>,
    tool_name: String,
    tool_input: PermissionToolInput,
}

impl PermissionHookPayload {
    fn parse(raw: &str) -> CtxResult<Self> {
        Ok(serde_json::from_str(raw)?)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct PermissionToolInput {
    command: String,
    file_path: String,
    url: String,
}

#[derive(Debug, Serialize)]
struct PermissionPromptRow<'a> {
    ts: u64,
    session: &'a str,
    event: &'a str,
    tool: &'a str,
    family: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_sha256: Option<String>,
    cwd: &'a str,
    permission_mode: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

fn web_url_host(url: &str) -> Option<String> {
    let (_, remainder) = url.split_once("://")?;
    let authority = remainder.split(['/', '?', '#']).next()?;
    let host_and_port = authority.rsplit('@').next()?;
    let host = if let Some(ipv6) = host_and_port.strip_prefix('[') {
        ipv6.split_once(']')?.0
    } else {
        host_and_port.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_string())
}

/// Program-family for the first executable shell segment, skipping quoted
/// assignments and structural keywords: `argv[0]` plus the first
/// non-flag argument that cannot itself carry a credential (a token
/// starting with `-` such as `-pSECRET`, or one containing `:`/`@`/`=` such
/// as `user:pass@host` or `KEY=val`). `cd`, `export`, and `printf` have data
/// operands, not subcommands; `source`/`.` names only the script basename.
/// A loop with no executable body falls back to its keyword.
/// Empty input yields an empty string --
/// callers with a more specific fallback (e.g. the tool name) apply it
/// themselves. Shared by [`permission_family`]'s `Bash`/`PowerShell` branch
/// and `safety::audit_hook_decision`'s own `family` field on the
/// safety-decision record (Change 5a) -- both need the identical
/// "never leak an argument" rule, so it exists exactly once.
pub(crate) fn command_family(command: &str) -> String {
    let mut keyword = String::new();
    for segment in crate::commands::ctx::safety::split_segments(command) {
        let quoted =
            crate::commands::ctx::safety::tokenize_quoted(&segment.chars().collect::<Vec<_>>());
        let mut tokens = quoted.iter().map(|token| token.text.as_str());
        let program = loop {
            let Some(token) = tokens.next() else {
                break None;
            };
            if crate::commands::ctx::safety::is_shell_identifier_assignment(token) {
                continue;
            }
            if matches!(token, "for" | "select" | "case") {
                keyword = token.to_string();
                break None;
            }
            if matches!(
                token,
                "while"
                    | "until"
                    | "if"
                    | "then"
                    | "else"
                    | "elif"
                    | "do"
                    | "time"
                    | "!"
                    | "{"
                    | "("
                    | "fi"
                    | "done"
                    | "esac"
                    | "}"
                    | ")"
            ) {
                if keyword.is_empty() {
                    keyword = token.to_string();
                }
                continue;
            }
            break Some(token);
        };
        let Some(program) = program else {
            continue;
        };
        if matches!(program, "cd" | "export" | "printf") {
            return program.to_string();
        }
        let subcommand =
            tokens.find(|token| !token.starts_with('-') && !token.contains([':', '@', '=']));
        if matches!(program, "source" | ".") {
            return subcommand
                .and_then(|path| Path::new(path.trim_matches(['\'', '"'])).file_name())
                .map(|name| format!("source {}", name.to_string_lossy()))
                .unwrap_or_else(|| "source".to_string());
        }
        return match subcommand {
            Some(subcommand) => format!("{program} {subcommand}"),
            None => program.to_string(),
        };
    }
    keyword
}

fn permission_family(payload: &PermissionHookPayload) -> (String, Option<String>) {
    match payload.tool_name.as_str() {
        "Bash" | "PowerShell" => {
            // The full command is captured only as an opaque sha256, never
            // in clear -- `command_family` above gives the plaintext family.
            let family = command_family(&payload.tool_input.command);
            let family = if family.is_empty() {
                payload.tool_name.clone()
            } else {
                family
            };
            (
                family,
                Some(crate::commands::ctx::safety::sha256_hex(
                    payload.tool_input.command.as_bytes(),
                )),
            )
        }
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            let path = Path::new(&payload.tool_input.file_path);
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            (parent.display().to_string(), None)
        }
        "WebFetch" => (
            web_url_host(&payload.tool_input.url).unwrap_or_else(|| payload.tool_name.clone()),
            None,
        ),
        _ => (payload.tool_name.clone(), None),
    }
}

fn permission_prompt_row(payload: &PermissionHookPayload, ts: u64) -> PermissionPromptRow<'_> {
    let (family, command_sha256) = permission_family(payload);
    PermissionPromptRow {
        ts,
        session: &payload.session_id,
        event: payload
            .hook_event_name
            .as_deref()
            .unwrap_or("PermissionRequest"),
        tool: &payload.tool_name,
        family,
        command_sha256,
        cwd: &payload.cwd,
        permission_mode: &payload.permission_mode,
        reason: payload.reason.as_deref(),
    }
}

/// Records one privacy-preserving permission-prompt row without affecting
/// Claude's permission flow. Every error is swallowed and stdout stays empty.
pub(super) fn run_permission<W: Write>(
    _w: &mut W,
    stdin: &str,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let Ok(payload) = PermissionHookPayload::parse(stdin) else {
        return Ok(0);
    };
    let Ok(state) = StateDir::resolve(env) else {
        return Ok(0);
    };
    let short = attention_short(env, &payload.session_id);
    // Issue #349: the one live choke point for "an operator needs to decide
    // something" -- this hook only ever fires while Claude is actually
    // holding for a permission decision. Best-effort, like every other
    // attention observation: a failure to persist it must never affect the
    // permission flow this hook only observes.
    //
    // Issue #456: `PermissionRequest` and `PermissionDenied` are the same
    // hook wired twice in `claude.rs` (`zirv ctx hook permission` for both),
    // distinguished only by `hook_event_name` -- a denial means the prompt is
    // gone exactly as much as an approval does, so it clears the latch
    // instead of raising it, through the same guarded helper `run_posttool`/
    // `run_pretool` use.
    if payload.hook_event_name.as_deref() == Some("PermissionDenied") {
        clear_resolved_approval(
            &state,
            &short,
            format!("permission denied: {}", payload.tool_name),
            now_secs(),
        );
    } else {
        let _ = crate::commands::ctx::attention::record(
            &state,
            &short,
            crate::commands::ctx::attention::Observation::new(
                crate::commands::ctx::attention::Authority::AdapterHook,
                format!("permission requested for {}", payload.tool_name),
                100,
                now_secs(),
            )
            .with_attention(crate::commands::ctx::attention::Attention::Approval),
            now_secs(),
        );
    }
    let Ok(line) = serde_json::to_string(&permission_prompt_row(&payload, now_secs())) else {
        return Ok(0);
    };
    let dir = state.logs();
    if crate::commands::ctx::state::create_private_dir_all(&dir).is_err() {
        return Ok(0);
    }
    let Ok(mut file) =
        crate::commands::ctx::state::open_private_append(&dir.join(PERMISSION_PROMPTS_FILE))
    else {
        return Ok(0);
    };
    let _ = writeln!(file, "{line}");
    Ok(0)
}

/// Issue #456: clears a still-pending `Attention::Approval` latch the moment
/// something proves the permission prompt is no longer live -- a
/// `PostToolUse`/`PreToolUse` hook firing at all (Claude never invokes either
/// until AFTER a permission decision has been made, so their mere arrival is
/// proof enough, regardless of which tool prompted or which tool now runs) or
/// a `PermissionDenied` event.
///
/// Guarded on the CURRENTLY PERSISTED attention actually being `Approval`:
/// `AdapterHook` already outranks every other authority on the attention axis
/// (see `attention::compose`'s own doc comment on per-axis suppression), so an
/// observation that asserts `Attention::None` unconditionally would win
/// regardless of what is currently recorded and could erase a legitimate
/// `Compacting`/`Quota`/`WorkflowGate`/`WriterConflict` latch left by an
/// earlier `AdapterHook`/`Supervisor`/`Workflow` observation, just because a
/// tool happened to run. This call means "the approval prompt specifically is
/// gone", never "nothing needs attention any more", so the write only ever
/// happens when an `Approval` latch is actually what would be cleared -- and
/// the check runs under the ledger lock (`record_if`) so a supervisor
/// observation landing between check and act cannot be clobbered.
/// Best-effort like every other attention write in this file: a failure to
/// read or persist never affects the calling hook's own exit code.
///
/// Perf: this runs on EVERY `PreToolUse`/`PostToolUse`/
/// `PermissionRequest`/`PermissionDenied` hook invocation -- the single
/// hottest call in the hook fleet, since it fires several times per turn
/// where every other per-turn hook fires once. `record_if` already skips its
/// own write once `applies` reads false under the lock, but still pays for
/// the lock file's open-and-lock round trip to reach that check. An
/// `Approval` latch is the rare case (a permission prompt is not pending for
/// most tool calls), so a plain unlocked [`crate::commands::ctx::attention::load`] first
/// avoids that lock entirely on the common path; only a read that might
/// actually need clearing falls through to the locked, race-safe
/// `record_if`, which re-reads and re-checks under the lock exactly as
/// before -- this pre-check changes nothing about what gets persisted, only
/// how often the lock is taken to find out there is nothing to do. A stale
/// or missed read here can only ever skip a clear it would have skipped
/// anyway on the next call (this hook always runs again), the same
/// best-effort tolerance `record_if`'s own doc comment already states.
pub(super) fn clear_resolved_approval(state: &StateDir, short: &str, evidence: String, now: u64) {
    if crate::commands::ctx::attention::load(state, short).attention
        != crate::commands::ctx::attention::Attention::Approval
    {
        return;
    }
    let _ = crate::commands::ctx::attention::record_if(
        state,
        short,
        crate::commands::ctx::attention::Observation::new(
            crate::commands::ctx::attention::Authority::AdapterHook,
            evidence,
            100,
            now,
        )
        .with_attention(crate::commands::ctx::attention::Attention::None),
        now,
        |prev| prev.attention == crate::commands::ctx::attention::Attention::Approval,
    );
}

#[cfg(test)]
mod tests {
    use super::super::pretool_run::run_pretool;
    use super::super::tests::{permission_env, permission_stdin, pretool_stdin};
    use super::*;

    #[test]
    fn permission_rows_normalize_bash_read_and_unknown_tools() {
        let command = "gh issue view 321 --json title";
        let bash = PermissionHookPayload::parse(&permission_stdin(
            None,
            "Bash",
            serde_json::json!({"command": command}),
        ))
        .expect("payload");
        assert_eq!(
            serde_json::to_value(permission_prompt_row(&bash, 42)).expect("row"),
            serde_json::json!({
                "ts": 42,
                "session": "abc123",
                "event": "PermissionRequest",
                "tool": "Bash",
                "family": "gh issue",
                "command_sha256": crate::commands::ctx::safety::sha256_hex(command.as_bytes()),
                "cwd": "/work/repo",
                "permission_mode": "default"
            })
        );

        let read = PermissionHookPayload::parse(&permission_stdin(
            Some("PermissionDenied"),
            "Read",
            serde_json::json!({"file_path": "/work/repo/src/main.rs"}),
        ))
        .expect("payload");
        let read = serde_json::to_value(permission_prompt_row(&read, 43)).expect("row");
        assert_eq!(read["event"], "PermissionDenied");
        assert_eq!(read["family"], "/work/repo/src");
        assert!(read["command_sha256"].is_null());

        let unknown = PermissionHookPayload::parse(&permission_stdin(
            Some("PermissionRequest"),
            "mcp__example__lookup",
            serde_json::json!({"query": "private input"}),
        ))
        .expect("payload");
        let unknown = serde_json::to_value(permission_prompt_row(&unknown, 44)).expect("row");
        assert_eq!(unknown["family"], "mcp__example__lookup");
        assert!(
            !unknown.to_string().contains("private input"),
            "raw tool input must never enter the row: {unknown}"
        );
    }

    #[test]
    fn permission_family_never_captures_a_credential_bearing_token() {
        let cases = [
            ("mysql -pSECRET -h host", "mysql host", "SECRET"),
            ("curl https://user:pass@host/api", "curl", "pass"),
            ("git push origin main", "git push", "origin"),
        ];
        for (command, expected_family, secret) in cases {
            let payload = PermissionHookPayload::parse(&permission_stdin(
                None,
                "Bash",
                serde_json::json!({ "command": command }),
            ))
            .expect("payload");
            let row = serde_json::to_value(permission_prompt_row(&payload, 1)).expect("row");
            assert_eq!(row["family"], expected_family, "{command}");
            assert!(
                !row["family"].as_str().unwrap().contains(secret),
                "family leaked `{secret}` for `{command}`"
            );
        }
    }

    /// Change 5a: `safety::audit_hook_decision` calls `command_family`
    /// directly (not through `PermissionHookPayload`) for the
    /// safety-decision log's own `family` field -- same underlying function
    /// as `permission_family_never_captures_a_credential_bearing_token`
    /// above, pinned here at the function itself so the "never leak an
    /// argument" guarantee holds independent of that wrapper.
    #[test]
    fn command_family_never_leaks_a_secret_shaped_argument() {
        assert_eq!(command_family("mysql -pSECRET -h host"), "mysql host");
        assert_eq!(command_family("curl https://user:pass@host/api"), "curl");
        assert_eq!(
            command_family("printf KEY=secret-value-from-command"),
            "printf"
        );
        assert_eq!(command_family(""), "");
    }

    #[test]
    fn command_family_skips_assignments_and_shell_structure() {
        for (command, expected) in [
            ("xcrun simctl list", "xcrun simctl"),
            (
                "BM=~/claude-code/backoffice-marketing; for d in a b; do printf '%s\\n' \"$d\"; done",
                "printf",
            ),
            (
                r#"for h in 6aaa503b 65d88e6f; do printf "%s -> " "$h"; date -u -r $((0x$h)) "+%Y-%m-%d %H:%M:%S UTC"; done"#,
                "printf",
            ),
            (
                r#"zirv ctx wait 62de9de3 --until done 2>&1 | tail -3; echo "wait-exit=$?""#,
                "zirv ctx",
            ),
            (
                "source /private/tmp/claude-501/scratchpad/kbn.sh; kbn_file a1",
                "source kbn.sh",
            ),
            (
                "cd /Users/jonathansolskov/Documents/Privat/zirv-fitness-tracking",
                "cd",
            ),
            ("export FOO=1", "export"),
            (
                r#"S=/private/tmp/scratchpad; P="/Users/j/Library/Application Support/zirv/ctx"; ZIRV_CTX_FALLBACK=false zirv agent codex - --workdir repo -- --model gpt-6-astra < $S/r.md > $S/o.out 2> $S/e.err; echo "exit=$?"; cat $S/o.out"#,
                "zirv agent",
            ),
            (
                r#"OUT="/private/tmp/gates-fix"; WT="repo/wt-jev-tier"; cargo fmt --manifest-path "$WT/Cargo.toml" -- --check > "$OUT/01-fmt.log" 2>&1; echo "FMT_EXIT=$?" | tee -a "$OUT/exit-codes.txt""#,
                "cargo fmt",
            ),
            ("W=.claude/worktrees/wt-jev-tier; git status", "git status"),
            (
                r#"S='space ; secret'; P="other ; secret" git status"#,
                "git status",
            ),
            (r#". "/private/tmp/a dir/kbn.sh""#, "source kbn.sh"),
            ("for h in a b", "for"),
            ("select h in a b; do git status; done", "git status"),
            ("case $x in", "case"),
            ("while git status; do echo ok; done", "git status"),
            ("until git status; do echo ok; done", "git status"),
            ("if git status; then echo ok; fi", "git status"),
            ("then git status", "git status"),
            ("else git status", "git status"),
            ("elif git status", "git status"),
            ("do git status", "git status"),
            ("time ! { ( git status; ) }", "git status"),
            ("TOKEN='secret value; still secret'", ""),
        ] {
            assert_eq!(command_family(command), expected, "{command}");
        }
    }

    #[test]
    fn permission_denied_reason_is_recorded_only_when_present() {
        let mut denied: serde_json::Value = serde_json::from_str(&permission_stdin(
            Some("PermissionDenied"),
            "Bash",
            serde_json::json!({"command": "git push"}),
        ))
        .expect("payload json");
        denied["reason"] = serde_json::json!("Blocked by auto-mode classifier");
        let denied = PermissionHookPayload::parse(&denied.to_string()).expect("payload");
        let denied = serde_json::to_value(permission_prompt_row(&denied, 45)).expect("row");
        assert_eq!(denied["reason"], "Blocked by auto-mode classifier");

        let requested = PermissionHookPayload::parse(&permission_stdin(
            Some("PermissionRequest"),
            "Bash",
            serde_json::json!({"command": "git status"}),
        ))
        .expect("payload");
        let requested = serde_json::to_value(permission_prompt_row(&requested, 46)).expect("row");
        assert!(requested.get("reason").is_none());
    }

    #[test]
    fn run_permission_exits_zero_and_silent_on_garbage_stdin() {
        let mut out = Vec::new();
        let code = run_permission(&mut out, "not json", &|_| None).expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn run_permission_appends_one_parseable_json_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();
        let mut out = Vec::new();
        let code = run_permission(
            &mut out,
            &permission_stdin(
                Some("PermissionRequest"),
                "Read",
                serde_json::json!({"file_path": "/work/repo/src/lib.rs"}),
            ),
            &|key| env.get(key).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty());

        let log = std::fs::read_to_string(state.join("logs/permission-prompts.jsonl"))
            .expect("permission prompt log");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 1);
        let row: serde_json::Value = serde_json::from_str(lines[0]).expect("json row");
        assert_eq!(row["session"], "abc123");
        assert_eq!(row["family"], "/work/repo/src");
    }

    /// A `PreToolUse` for a DIFFERENT tool than the one that prompted also
    /// clears `Approval`: a new tool call at all proves the prompt is gone.
    #[test]
    fn pretool_for_a_different_tool_after_a_permission_request_clears_approval() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("state");
        let env = permission_env(&state_path);
        let lookup = |k: &str| env.get(k).cloned();
        let state = StateDir::resolve(&lookup).expect("state dir");
        let short = crate::commands::ctx::sessions::short_id("abc123");

        let mut out = Vec::new();
        run_permission(
            &mut out,
            &permission_stdin(
                Some("PermissionRequest"),
                "Bash",
                serde_json::json!({"command": "rm -rf /tmp/x"}),
            ),
            &lookup,
        )
        .expect("never errors");
        assert_eq!(
            crate::commands::ctx::attention::load(&state, &short).attention,
            crate::commands::ctx::attention::Attention::Approval
        );

        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin(
                "Read",
                serde_json::json!({"file_path": "/work/repo/README.md"}),
            ),
            &lookup,
        )
        .expect("never errors");
        assert_eq!(code, 0);

        let status = crate::commands::ctx::attention::load(&state, &short);
        assert_eq!(
            status.attention,
            crate::commands::ctx::attention::Attention::None
        );
        assert!(
            crate::commands::ctx::attention::reason(&status).contains("permission resolved: Read"),
            "got {}",
            crate::commands::ctx::attention::reason(&status)
        );
    }

    /// A `PermissionDenied` (classifier or user deny) clears `Approval` the
    /// same way as a resolved prompt -- the decision is made either way.
    #[test]
    fn permission_denied_clears_approval() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("state");
        let env = permission_env(&state_path);
        let lookup = |k: &str| env.get(k).cloned();
        let state = StateDir::resolve(&lookup).expect("state dir");
        let short = crate::commands::ctx::sessions::short_id("abc123");

        let mut out = Vec::new();
        run_permission(
            &mut out,
            &permission_stdin(
                Some("PermissionRequest"),
                "Bash",
                serde_json::json!({"command": "curl evil.example"}),
            ),
            &lookup,
        )
        .expect("never errors");
        assert_eq!(
            crate::commands::ctx::attention::load(&state, &short).attention,
            crate::commands::ctx::attention::Attention::Approval
        );

        let mut out = Vec::new();
        let code = run_permission(
            &mut out,
            &permission_stdin(
                Some("PermissionDenied"),
                "Bash",
                serde_json::json!({"command": "curl evil.example"}),
            ),
            &lookup,
        )
        .expect("never errors");
        assert_eq!(code, 0);

        let status = crate::commands::ctx::attention::load(&state, &short);
        assert_eq!(
            status.attention,
            crate::commands::ctx::attention::Attention::None
        );
        assert!(
            crate::commands::ctx::attention::reason(&status).contains("permission denied: Bash"),
            "got {}",
            crate::commands::ctx::attention::reason(&status)
        );
    }
}
