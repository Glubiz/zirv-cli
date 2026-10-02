//! `PreToolUse`-adjacent permission-prompt hook: payload parsing, the
//! command-family/attention-line heuristics it logs by, and the run entry.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::adapters::{SESSION_ENV, SOCKET_ENV};
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

/// Prefer the socket's stable short ID for attention observations: hook
/// session IDs can rotate during an internal restart (#349). Without a socket
/// (Codex has none), the zirv session beats the payload's harness conversation
/// id, which the dashboard never keys attention by (#841).
pub(super) fn attention_short(env: EnvLookup<'_>, session_id_fallback: &str) -> String {
    env(SOCKET_ENV)
        .and_then(|raw| {
            Path::new(&raw)
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            let zirv_session = env(SESSION_ENV).filter(|s| !s.is_empty());
            crate::commands::ctx::sessions::short_id(
                zirv_session.as_deref().unwrap_or(session_id_fallback),
            )
        })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct PermissionHookPayload {
    session_id: String,
    cwd: String,
    permission_mode: String,
    /// Non-empty inside a native subagent.
    agent_id: String,
    hook_event_name: Option<String>,
    reason: Option<String>,
    tool_name: String,
    tool_input: PermissionToolInput,
    /// Claude's own suggested permission updates; the only source an "always allow" may apply.
    permission_suggestions: Vec<serde_json::Value>,
}

impl PermissionHookPayload {
    fn parse(raw: &str) -> CtxResult<Self> {
        Ok(serde_json::from_str(raw)?)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PermissionToolInput {
    pub command: String,
    file_path: String,
    url: String,
    /// Every other input field, so the inbox can preview a tool this parser has no typed field for.
    #[serde(flatten)]
    other: serde_json::Map<String, serde_json::Value>,
}

impl PermissionToolInput {
    /// A Bash call's own plain-language `description`, read from the untyped fields so previews and ids stay unchanged.
    fn bash_description(&self, tool_name: &str) -> Option<String> {
        if !matches!(tool_name, "Bash" | "PowerShell") {
            return None;
        }
        Some(self.other.get("description")?.as_str()?.to_string()).filter(|d| !d.is_empty())
    }

    fn outside_sandbox(&self) -> bool {
        self.other
            .get("dangerouslyDisableSandbox")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }

    /// The text the approvals inbox previews: the command, path or URL, else the remaining input fields.
    pub fn preview_source(&self, tool_name: &str) -> String {
        match tool_name {
            "Bash" | "PowerShell" => self.command.clone(),
            "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => self.file_path.clone(),
            "WebFetch" => self.url.clone(),
            _ => serde_json::to_string(&self.other).unwrap_or_default(),
        }
    }
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

/// Redacted shell program family shared by permission and safety logs;
/// exclude flag or credential-shaped operands to avoid leaking arguments.
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

/// The one `PermissionRequest` response this hook ever prints: approve.
const PERMISSION_ALLOW_DECISION: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;

/// Whether zirv's own safety verdict allows the command and every segment of
/// it is a prompt-free built-in zirv invocation (#845). Never a deny or ask:
/// anything uncertain simply leaves Claude's prompt in place.
fn permission_request_is_prompt_free(
    payload: &PermissionHookPayload,
    stdin: &str,
    env: EnvLookup<'_>,
) -> bool {
    if payload.tool_name != "Bash" || payload.hook_event_name.as_deref() == Some("PermissionDenied")
    {
        return false;
    }
    let Ok(current_exe) = std::env::current_exe().and_then(std::fs::canonicalize) else {
        return false;
    };
    if !crate::commands::ctx::safety::permission_request_command_is_prompt_free(
        &payload.tool_input.command,
        &current_exe,
    ) {
        return false;
    }
    // The side-effect-free recheck cannot see a Jev escalation of the Allow.
    let Ok(cfg) = CtxConfig::load(Path::new("."), env).and_then(|cfg| {
        if cfg.jev.approve {
            Err("jev approve is enabled".into())
        } else {
            Ok(cfg)
        }
    }) else {
        return false;
    };
    let verdict = crate::commands::ctx::safety::evaluate_check_hook_verdict(&cfg, stdin, env);
    matches!(
        verdict,
        Ok(Some(crate::commands::ctx::safety::Verdict::Allow))
    )
}

/// The latch evidence NEEDS YOU shows: the tool and a redacted preview of what it asks to run.
fn permission_evidence(payload: &PermissionHookPayload) -> String {
    let preview = crate::commands::ctx::approvals::redacted_preview(
        &payload.tool_input.preview_source(&payload.tool_name),
    );
    if preview.is_empty() {
        return format!("permission requested for {}", payload.tool_name);
    }
    format!("{}: {preview}", payload.tool_name)
}

/// Records one privacy-preserving permission-prompt row. Prints the allow
/// decision only for a command [`permission_request_is_prompt_free`]
/// proves; every error is swallowed and otherwise stdout stays empty.
pub(super) fn run_permission<W: Write>(
    w: &mut W,
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
    let auto_allowed = permission_request_is_prompt_free(&payload, stdin, env);
    let permission_id = crate::commands::ctx::approvals::request_id(
        &short,
        &payload.tool_name,
        &payload.tool_input.command,
        &payload.tool_input.preview_source(&payload.tool_name),
    );
    if auto_allowed {
        let _ = writeln!(w, "{PERMISSION_ALLOW_DECISION}");
    }
    // Observe live permission prompts. A denial also clears the prompt latch
    // because the decision is resolved (#349, #456); an auto-allowed prompt
    // never waits on the operator, so it sets no latch.
    if payload.hook_event_name.as_deref() == Some("PermissionDenied") {
        clear_resolved_approval(
            &state,
            &short,
            format!("permission denied: {}", payload.tool_name),
            now_secs(),
            |open| open.id == permission_id && open.agent == payload.agent_id,
        );
        crate::commands::ctx::approvals::clear_for_tool(
            &state,
            &short,
            &payload.tool_name,
            &payload.tool_input.command,
            &payload.tool_input.preview_source(&payload.tool_name),
        );
    } else if !auto_allowed {
        crate::commands::ctx::attention::open_prompt(
            &state,
            &short,
            crate::commands::ctx::attention::OpenPrompt {
                id: permission_id,
                agent: payload.agent_id.clone(),
                at: now_secs(),
            },
        );
        let _ = crate::commands::ctx::attention::record(
            &state,
            &short,
            crate::commands::ctx::attention::Observation::new(
                crate::commands::ctx::attention::Authority::AdapterHook,
                permission_evidence(&payload),
                100,
                now_secs(),
            )
            .with_attention(crate::commands::ctx::attention::Attention::Approval),
            now_secs(),
        );
        // Approvals inbox (#840): with the key on and a live owning dashboard, hold for its operator.
        // Any other outcome prints nothing, so the native dialog shows exactly as it always has.
        let inbox = crate::commands::ctx::config::ApprovalsConfig::load_operator_only(env)
            .unwrap_or_default();
        // Only a mode that shows the operator a dialog is worth holding; auto, dontAsk and bypassPermissions resolve without one.
        if inbox.inbox
            && matches!(
                payload.permission_mode.as_str(),
                "default" | "plan" | "acceptEdits"
            )
        {
            let rule =
                crate::commands::ctx::approvals::always_rule(&payload.permission_suggestions);
            let details = crate::commands::ctx::approvals::RequestDetails {
                cwd: Some(payload.cwd.clone()).filter(|cwd| !cwd.is_empty()),
                reason: payload.tool_input.bash_description(&payload.tool_name),
                outside_sandbox: payload.tool_input.outside_sandbox(),
                always: rule.as_ref().map(|rule| rule.label.clone()),
            };
            if let Some(json) = crate::commands::ctx::approvals::hold_for_dashboard(
                &state,
                &short,
                &payload.tool_name,
                &payload.tool_input.command,
                &payload.tool_input.preview_source(&payload.tool_name),
                details,
                std::time::Duration::from_secs(inbox.hold_secs),
            )
            .and_then(|decision| {
                crate::commands::ctx::approvals::decision_json(decision, rule.as_ref())
            }) {
                let _ = writeln!(w, "{json}");
            }
        }
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

/// Clear Approval or Question only when it is still the persisted attention state and `closes`
/// leaves no other permission prompt of the session open (#854).
/// Tool hooks and PermissionDenied prove the prompt ended; a locked
/// conditional write preserves unrelated higher-priority latches. Read
/// without the lock first to avoid common-path hot-hook latency (#456).
pub(super) fn clear_resolved_approval(
    state: &StateDir,
    short: &str,
    evidence: String,
    now: u64,
    closes: impl Fn(&crate::commands::ctx::attention::OpenPrompt) -> bool,
) {
    use crate::commands::ctx::attention::Attention;
    if crate::commands::ctx::attention::close_prompts(state, short, closes) > 0 {
        return;
    }
    let waiting = |attention| matches!(attention, Attention::Approval | Attention::Question);
    if !waiting(crate::commands::ctx::attention::load(state, short).attention) {
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
        |prev| waiting(prev.attention),
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
    fn the_approval_latch_evidence_names_the_tool_and_a_redacted_preview() {
        let payload = |command: &str| {
            PermissionHookPayload::parse(&permission_stdin(
                Some("PermissionRequest"),
                "Bash",
                serde_json::json!({"command": command}),
            ))
            .expect("payload")
        };
        assert_eq!(
            permission_evidence(&payload("cargo test")),
            "Bash: cargo test"
        );
        assert_eq!(
            permission_evidence(&payload("")),
            "permission requested for Bash"
        );
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

    fn permission_output(command: &str) -> String {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let env = permission_env(&dir.path().join("state"));
        let mut out = Vec::new();
        run_permission(
            &mut out,
            &permission_stdin(
                Some("PermissionRequest"),
                "Bash",
                serde_json::json!({"command": command}),
            ),
            &|key| env.get(key).cloned(),
        )
        .expect("never errors");
        String::from_utf8(out).expect("utf8")
    }

    #[test]
    fn the_allow_decision_has_the_documented_shape() {
        let value: serde_json::Value =
            serde_json::from_str(PERMISSION_ALLOW_DECISION).expect("json");
        assert_eq!(
            value,
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow"}
            }})
        );
    }

    #[test]
    fn a_prompt_free_zirv_command_is_approved_on_stdout() {
        let out = permission_output("cd /some/dir && zirv ctx send abc \"hello\"");
        assert_eq!(out.trim(), PERMISSION_ALLOW_DECISION);
    }

    #[test]
    fn excluded_or_refused_zirv_commands_print_nothing() {
        for command in [
            "zirv ctx exec -- rm -rf x",
            "zirv ctx config set foo bar",
            "zirv ctx config show > ~/.zirv/ctx.toml",
            "./target/debug/zirv ctx send a b",
        ] {
            assert_eq!(permission_output(command), "", "{command}");
        }
    }

    #[test]
    fn the_permission_request_path_appends_no_safety_audit_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state = dir.path().join("state");
        let env = permission_env(&state);
        let mut out = Vec::new();
        run_permission(
            &mut out,
            &permission_stdin(
                Some("PermissionRequest"),
                "Bash",
                serde_json::json!({"command": "zirv ctx status"}),
            ),
            &|key| env.get(key).cloned(),
        )
        .expect("never errors");
        assert!(!out.is_empty(), "the command must have been approved");
        assert!(
            !state
                .join("logs")
                .join(crate::commands::ctx::log::SAFETY_LOG_DIR)
                .exists()
        );
    }

    #[test]
    fn jev_approve_enabled_leaves_the_prompt_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let mut env = permission_env(&dir.path().join("state"));
        env.insert("ZIRV_CTX_JEV_APPROVE".into(), "true".into());
        let mut out = Vec::new();
        run_permission(
            &mut out,
            &permission_stdin(
                Some("PermissionRequest"),
                "Bash",
                serde_json::json!({"command": "zirv ctx status"}),
            ),
            &|key| env.get(key).cloned(),
        )
        .expect("never errors");
        assert!(out.is_empty());
    }

    #[test]
    fn ask_user_question_latches_a_question_until_the_next_tool_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let env = permission_env(&dir.path().join("state"));
        let lookup = |k: &str| env.get(k).cloned();
        run_pretool(
            &mut Vec::new(),
            &pretool_stdin(
                "AskUserQuestion",
                serde_json::json!({"questions": [{"question": "Which layout?"}]}),
            ),
            &lookup,
        )
        .expect("never errors");
        let state = StateDir::resolve(&lookup).expect("state dir");
        let short = crate::commands::ctx::sessions::short_id("abc123");
        let status = crate::commands::ctx::attention::load(&state, &short);
        assert_eq!(
            status.attention,
            crate::commands::ctx::attention::Attention::Question
        );
        assert!(status.evidence.contains("AskUserQuestion: Which layout?"));
        run_pretool(
            &mut Vec::new(),
            &pretool_stdin("Read", serde_json::json!({"file_path": "/work/repo/a.rs"})),
            &lookup,
        )
        .expect("never errors");
        assert_eq!(
            crate::commands::ctx::attention::load(&state, &short).attention,
            crate::commands::ctx::attention::Attention::None
        );
    }

    #[test]
    fn an_auto_approved_prompt_raises_no_approval_latch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let env = permission_env(&dir.path().join("state"));
        let lookup = |k: &str| env.get(k).cloned();
        run_permission(
            &mut Vec::new(),
            &permission_stdin(
                Some("PermissionRequest"),
                "Bash",
                serde_json::json!({"command": "zirv ctx status"}),
            ),
            &lookup,
        )
        .expect("never errors");
        let state = StateDir::resolve(&lookup).expect("state dir");
        let short = crate::commands::ctx::sessions::short_id("abc123");
        assert_ne!(
            crate::commands::ctx::attention::load(&state, &short).attention,
            crate::commands::ctx::attention::Attention::Approval
        );
    }

    #[test]
    fn two_subagents_prompting_for_the_same_command_hold_approval_until_both_resolve() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let env = permission_env(&dir.path().join("state"));
        let lookup = |k: &str| env.get(k).cloned();
        let state = StateDir::resolve(&lookup).expect("state dir");
        let short = crate::commands::ctx::sessions::short_id("abc123");
        let with_agent = |stdin: String, agent: &str| {
            let mut value: serde_json::Value = serde_json::from_str(&stdin).expect("json");
            value["agent_id"] = serde_json::json!(agent);
            value.to_string()
        };
        for agent in ["agent-a", "agent-b"] {
            run_permission(
                &mut Vec::new(),
                &with_agent(
                    permission_stdin(
                        Some("PermissionRequest"),
                        "Bash",
                        serde_json::json!({"command": "rm -rf /tmp/x"}),
                    ),
                    agent,
                ),
                &lookup,
            )
            .expect("never errors");
        }
        let mut post: serde_json::Value = serde_json::json!({
            "session_id": "abc123",
            "cwd": "/work/repo",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "rm -rf /tmp/x"},
            "tool_response": {"stdout": "", "stderr": ""},
            "agent_id": "agent-b",
        });
        super::super::posttool::run_posttool(&mut Vec::new(), &post.to_string(), &lookup)
            .expect("never errors");
        assert_eq!(
            crate::commands::ctx::attention::load(&state, &short).attention,
            crate::commands::ctx::attention::Attention::Approval,
            "agent a's dialog is still open"
        );
        post["agent_id"] = serde_json::json!("agent-a");
        super::super::posttool::run_posttool(&mut Vec::new(), &post.to_string(), &lookup)
            .expect("never errors");
        assert_eq!(
            crate::commands::ctx::attention::load(&state, &short).attention,
            crate::commands::ctx::attention::Attention::None
        );
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

        // A call parallel to the prompt starts within the same second and proves nothing about it (#854).
        run_pretool(
            &mut Vec::new(),
            &pretool_stdin("Read", serde_json::json!({"file_path": "/work/repo/a.md"})),
            &lookup,
        )
        .expect("never errors");
        assert_eq!(
            crate::commands::ctx::attention::load(&state, &short).attention,
            crate::commands::ctx::attention::Attention::Approval
        );
        // The same agent calling on well after the prompt shows it ended, denied or answered.
        crate::commands::ctx::attention::open_prompt(
            &state,
            &short,
            crate::commands::ctx::attention::OpenPrompt {
                id: crate::commands::ctx::approvals::request_id(
                    &short,
                    "Bash",
                    "rm -rf /tmp/x",
                    "rm -rf /tmp/x",
                ),
                agent: String::new(),
                at: 1,
            },
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

    fn inbox_env(state: &Path, inbox: bool) -> impl Fn(&str) -> Option<String> + use<> {
        let base = permission_env(state);
        move |key| match key {
            "ZIRV_CTX_APPROVALS_INBOX" => inbox.then(|| "true".to_string()),
            "ZIRV_CTX_APPROVALS_HOLD_SECS" => Some("5".to_string()),
            other => base.get(other).cloned(),
        }
    }

    fn permission_request() -> String {
        permission_stdin(
            Some("PermissionRequest"),
            "Bash",
            serde_json::json!({"command": "cargo nextest run"}),
        )
    }

    #[test]
    fn with_the_inbox_off_the_hook_prints_nothing_and_touches_no_inbox_state() {
        let tmp = tempfile::tempdir().expect("tmp");
        let lookup = inbox_env(tmp.path(), false);
        let mut out = Vec::new();
        run_permission(&mut out, &permission_request(), &lookup).expect("never errors");
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
        assert!(
            !crate::commands::ctx::approvals::approvals_dir(
                &StateDir::resolve(&lookup).expect("state")
            )
            .exists()
        );
    }

    #[test]
    fn with_the_inbox_on_but_no_live_dashboard_the_hook_prints_nothing_at_once() {
        let tmp = tempfile::tempdir().expect("tmp");
        let lookup = inbox_env(tmp.path(), true);
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        run_permission(&mut out, &permission_request(), &lookup).expect("never errors");
        assert!(out.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn a_held_request_prints_exactly_the_one_call_decision() {
        use crate::commands::ctx::approvals::Decision;
        for (decision, behavior) in [(Decision::Allow, "allow"), (Decision::Deny, "deny")] {
            let tmp = tempfile::tempdir().expect("tmp");
            let lookup = inbox_env(tmp.path(), true);
            let state = StateDir::resolve(&lookup).expect("state");
            let _guard = crate::commands::ctx::sessions::SessionGuard::register(
                &state,
                crate::commands::ctx::sessions::Record::new(
                    "abc123",
                    "claude",
                    Path::new("/work/repo"),
                    crate::commands::ctx::sessions::Verb::Dash,
                ),
            );
            let mut hub = crate::commands::ctx::approvals::Hub::bind(&state).expect("hub");
            let dashboard = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while hub.count() == 0 && std::time::Instant::now() < deadline {
                    hub.poll(&|_| true);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                hub.resolve_current(decision);
                std::thread::sleep(std::time::Duration::from_millis(300));
            });
            let mut out = Vec::new();
            run_permission(&mut out, &permission_request(), &lookup).expect("never errors");
            dashboard.join().expect("dashboard");
            let text = String::from_utf8(out).expect("utf8");
            assert!(!text.contains("updatedPermissions"), "{text}");
            let value: serde_json::Value = serde_json::from_str(text.trim()).expect("json");
            assert_eq!(
                value,
                serde_json::json!({"hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {"behavior": behavior}
                }})
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn allow_always_prints_the_suggestion_claude_sent_and_the_request_carries_its_details() {
        use crate::commands::ctx::approvals::Decision;
        let suggestion = serde_json::json!({
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "cargo nextest run:*"}],
            "behavior": "allow",
            "destination": "localSettings"
        });
        let stdin = serde_json::json!({
            "session_id": "abc123",
            "cwd": "/work/repo",
            "permission_mode": "default",
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo nextest run",
                "description": "Run the tests",
                "dangerouslyDisableSandbox": true
            },
            "permission_suggestions": [suggestion]
        })
        .to_string();
        let tmp = tempfile::tempdir().expect("tmp");
        let lookup = inbox_env(tmp.path(), true);
        let state = StateDir::resolve(&lookup).expect("state");
        let _guard = crate::commands::ctx::sessions::SessionGuard::register(
            &state,
            crate::commands::ctx::sessions::Record::new(
                "abc123",
                "claude",
                Path::new("/work/repo"),
                crate::commands::ctx::sessions::Verb::Dash,
            ),
        );
        let mut hub = crate::commands::ctx::approvals::Hub::bind(&state).expect("hub");
        let dashboard = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while hub.count() == 0 && std::time::Instant::now() < deadline {
                hub.poll(&|_| true);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let request = hub
                .current()
                .map(|item| item.request.clone())
                .expect("held");
            hub.resolve_current(Decision::AllowAlways);
            std::thread::sleep(std::time::Duration::from_millis(300));
            request
        });
        let mut out = Vec::new();
        run_permission(&mut out, &stdin, &lookup).expect("never errors");
        let request = dashboard.join().expect("dashboard");
        assert_eq!(request.command, "cargo nextest run");
        assert_eq!(request.cwd.as_deref(), Some("/work/repo"));
        assert_eq!(request.reason.as_deref(), Some("Run the tests"));
        assert!(request.outside_sandbox);
        assert_eq!(
            request.always.as_deref(),
            Some("cargo nextest run commands")
        );
        let value: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).expect("utf8").trim()).expect("json");
        assert_eq!(
            value["hookSpecificOutput"]["decision"],
            serde_json::json!({"behavior": "allow", "updatedPermissions": [suggestion]})
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_mode_where_claude_resolves_without_the_user_is_never_held() {
        for mode in ["auto", "dontAsk", "bypassPermissions"] {
            let tmp = tempfile::tempdir().expect("tmp");
            let lookup = inbox_env(tmp.path(), true);
            let state = StateDir::resolve(&lookup).expect("state");
            let _guard = crate::commands::ctx::sessions::SessionGuard::register(
                &state,
                crate::commands::ctx::sessions::Record::new(
                    "abc123",
                    "claude",
                    Path::new("/work/repo"),
                    crate::commands::ctx::sessions::Verb::Dash,
                ),
            );
            let _hub = crate::commands::ctx::approvals::Hub::bind(&state).expect("hub");
            let mut payload: serde_json::Value =
                serde_json::from_str(&permission_request()).expect("json");
            payload["permission_mode"] = mode.into();
            let mut out = Vec::new();
            run_permission(&mut out, &payload.to_string(), &lookup).expect("never errors");
            assert!(out.is_empty(), "{mode}: {}", String::from_utf8_lossy(&out));
            let held = std::fs::read_dir(crate::commands::ctx::approvals::approvals_dir(&state))
                .map(|dir| {
                    dir.flatten()
                        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                        .count()
                })
                .unwrap_or(0);
            assert_eq!(held, 0, "{mode} must not leave a held request");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unanswered_hold_prints_nothing_so_the_native_dialog_shows() {
        let tmp = tempfile::tempdir().expect("tmp");
        let lookup = {
            let base = inbox_env(tmp.path(), true);
            move |key: &str| {
                if key == "ZIRV_CTX_APPROVALS_HOLD_SECS" {
                    return Some("1".to_string());
                }
                base(key)
            }
        };
        let state = StateDir::resolve(&lookup).expect("state");
        let _guard = crate::commands::ctx::sessions::SessionGuard::register(
            &state,
            crate::commands::ctx::sessions::Record::new(
                "abc123",
                "claude",
                Path::new("/work/repo"),
                crate::commands::ctx::sessions::Verb::Dash,
            ),
        );
        let _hub = crate::commands::ctx::approvals::Hub::bind(&state).expect("hub");
        let mut out = Vec::new();
        run_permission(&mut out, &permission_request(), &lookup).expect("never errors");
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    }
}
