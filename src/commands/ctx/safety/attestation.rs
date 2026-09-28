//! Attestation rules for command safety.

use super::*;

/// A stable SHA-256 identity for one fully resolved policy. `SafetyPolicy`
/// serializes as a struct with declaration-ordered fields and ordered rule
/// vectors, so the same effective posture produces the same bytes on every
/// supported operating system. Origins are intentionally included: an
/// operator-facing audit must distinguish a shipped rule from a checkout
/// that happened to contribute identical text.
pub fn policy_fingerprint(policy: &SafetyPolicy) -> Result<String, serde_json::Error> {
    Ok(sha256_hex(&serde_json::to_vec(policy)?))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub const POLICY_FINGERPRINT_ENV: &str = "ZIRV_CTX_SAFETY_POLICY_SHA256";
pub const POLICY_SNAPSHOT_ENV: &str = "ZIRV_CTX_SAFETY_POLICY_FILE";

/// Issue #139: whether the launch-time policy snapshot's verdict for one
/// command diverges from the currently-resolved policy's own verdict for
/// the SAME command, and in which direction. `evaluate_with_attestation_
/// evidence` always keeps the stricter of the two answers -- a repo may
/// narrow a running session immediately, while an operator widening the
/// policy takes effect only on the next launch -- but that fold used to be
/// invisible: the hook's own explanation named the interactive/headless
/// DEFAULT as if it were the configured posture, while `zirv ctx safety
/// explain` for the identical command (bypassing attestation entirely)
/// reported the current, wider policy. This enum is what lets both
/// surfaces agree and say WHY, instead of silently disagreeing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnapshotDivergence {
    /// The launch snapshot and the current policy agree for this command --
    /// the common case: no snapshot at all, an invalid/corrupt one (both
    /// already explained by `AttestedEvaluation::status`), or one whose
    /// verdict for this command happens to match today's policy.
    Unchanged,
    /// The pinned launch snapshot's verdict is STRICTER than the current
    /// policy's own verdict for this command would be: an operator widened
    /// the policy after this session launched, and the widening has not
    /// taken effect yet (by design -- see this enum's own doc comment).
    /// Carries the current policy's own verdict so an explanation can name
    /// what it would have been.
    SnapshotStricter { current_verdict: Verdict },
}

/// Evaluates against both the immutable launch snapshot and the policy as it
/// resolves now, then keeps the stricter answer. A repo may therefore narrow
/// a running session immediately, while an operator widening their policy
/// takes effect only on the next launch. Missing attestation variables mean
/// this is a deliberately persistent/outside-Zirv hook and preserve its
/// current-policy behavior; a partial, corrupt, or hash-mismatched
/// attestation fails closed.
#[derive(Debug)]
pub(super) struct AttestedEvaluation {
    pub(super) outcome: Outcome,
    pub(super) current_fingerprint: String,
    pub(super) launch_fingerprint: Option<String>,
    pub(super) status: &'static str,
    pub(super) divergence: SnapshotDivergence,
}

/// Issue #168, design decision (c): what an invalid attestation snapshot
/// (absent one of the two env vars, an unreadable/unparseable file, or a
/// hash mismatch) now produces INSTEAD of the old blanket `attestation_
/// failure(mode)` (interactive `Ask`/headless `Deny` on every single
/// command for the rest of the session, with no way out short of a
/// restart). A broken snapshot proves nothing about `current` -- the
/// in-process policy this same launch already resolved from `~/.zirv/
/// ctx.toml` and any repo `.zirv/ctx.toml` -- so this falls back to
/// evaluating `current` alone, exactly like the "no attestation configured
/// at all" case, and best-effort re-materializes the snapshot file at
/// `snapshot_path` (when one was named) so the NEXT command in this same
/// session attests cleanly again instead of re-detecting the identical
/// broken file every time. The re-materialization write failing is
/// silently ignored: it only ever improves the next call, never gates this
/// one. `status: "self-healed"` distinguishes this path in the audit log
/// and from both `"not-present"` and `"valid"`.
#[allow(clippy::too_many_arguments)]
fn self_healed_evaluation(
    current: &SafetyPolicy,
    command: &str,
    mode: super::adapters::LaunchMode,
    current_fingerprint: String,
    launch_fingerprint: Option<String>,
    snapshot_path: Option<&str>,
    scratchpad_roots: &[String],
    envelope: Option<&envelope::WorkerEnvelope>,
    cwd: Option<&Path>,
    now: u64,
) -> AttestedEvaluation {
    if let Some(path) = snapshot_path {
        let _ = rematerialize_policy_snapshot(path, current);
    }
    AttestedEvaluation {
        outcome: evaluate_with_scratchpad_roots(
            current,
            command,
            mode,
            scratchpad_roots,
            envelope,
            cwd,
            now,
        ),
        current_fingerprint,
        launch_fingerprint,
        status: "self-healed",
        divergence: SnapshotDivergence::Unchanged,
    }
}

/// Best-effort rewrite of the policy snapshot file at `path` from `policy` --
/// the identical body `adapters::claude::launch_settings_path` writes at
/// launch, reused here (via the same pretty-JSON-plus-trailing-newline
/// shape) so a self-heal and a fresh launch can never format the snapshot
/// two different ways. Errors are the caller's to ignore: this is a repair
/// attempt for the NEXT command, never a gate on the current one.
fn rematerialize_policy_snapshot(path: &str, policy: &SafetyPolicy) -> std::io::Result<()> {
    let mut body = serde_json::to_string_pretty(policy).map_err(std::io::Error::other)?;
    body.push('\n');
    let path = std::path::Path::new(path);
    if let Some(parent) = path.parent() {
        super::state::create_private_dir_all(parent)?;
    }
    super::state::write_private(path, &body)
}

pub(super) fn evaluate_with_attestation_evidence(
    current: &SafetyPolicy,
    command: &str,
    mode: super::adapters::LaunchMode,
    env: EnvLookup<'_>,
    scratchpad_roots: &[String],
    cwd: Option<&Path>,
) -> AttestedEvaluation {
    let now = super::state::now_secs();
    // Issue #262: parsed once here (this function already reads `env` for
    // the attestation fingerprint/snapshot, so this is not a new dependency)
    // and threaded down into every `evaluate_with_scratchpad_roots` call
    // below -- `evaluate`/`evaluate_with_scratchpad_roots` themselves stay
    // pure, taking the envelope as an explicit parameter rather than reading
    // `ENVELOPE_ENV` internally.
    let envelope = parse_envelope_env(env);
    let envelope = envelope.as_ref();
    let current_fingerprint =
        policy_fingerprint(current).unwrap_or_else(|_| "unavailable".to_string());
    let (expected_fingerprint, snapshot_path) =
        match (env(POLICY_FINGERPRINT_ENV), env(POLICY_SNAPSHOT_ENV)) {
            (None, None) => {
                return AttestedEvaluation {
                    outcome: evaluate_with_scratchpad_roots(
                        current,
                        command,
                        mode,
                        scratchpad_roots,
                        envelope,
                        cwd,
                        now,
                    ),
                    current_fingerprint,
                    launch_fingerprint: None,
                    status: "not-present",
                    divergence: SnapshotDivergence::Unchanged,
                };
            }
            (Some(fingerprint), Some(path)) => (fingerprint, path),
            (fingerprint, _) => {
                return self_healed_evaluation(
                    current,
                    command,
                    mode,
                    current_fingerprint,
                    fingerprint,
                    None,
                    scratchpad_roots,
                    envelope,
                    cwd,
                    now,
                );
            }
        };

    let launch = std::fs::read_to_string(&snapshot_path)
        .ok()
        .and_then(|body| serde_json::from_str::<SafetyPolicy>(&body).ok());
    let Some(launch) = launch else {
        return self_healed_evaluation(
            current,
            command,
            mode,
            current_fingerprint,
            Some(expected_fingerprint),
            Some(snapshot_path.as_str()),
            scratchpad_roots,
            envelope,
            cwd,
            now,
        );
    };
    if policy_fingerprint(&launch).ok().as_deref() != Some(expected_fingerprint.as_str()) {
        return self_healed_evaluation(
            current,
            command,
            mode,
            current_fingerprint,
            Some(expected_fingerprint),
            Some(snapshot_path.as_str()),
            scratchpad_roots,
            envelope,
            cwd,
            now,
        );
    }

    let current_outcome = evaluate_with_scratchpad_roots(
        current,
        command,
        mode,
        scratchpad_roots,
        envelope,
        cwd,
        now,
    );
    let launch_outcome = evaluate_with_scratchpad_roots(
        &launch,
        command,
        mode,
        scratchpad_roots,
        envelope,
        cwd,
        now,
    );
    let divergence = if verdict_rank(launch_outcome.verdict) > verdict_rank(current_outcome.verdict)
    {
        SnapshotDivergence::SnapshotStricter {
            current_verdict: current_outcome.verdict,
        }
    } else {
        SnapshotDivergence::Unchanged
    };
    let outcome = if verdict_rank(current_outcome.verdict) >= verdict_rank(launch_outcome.verdict) {
        current_outcome
    } else {
        launch_outcome
    };
    AttestedEvaluation {
        outcome,
        current_fingerprint,
        launch_fingerprint: Some(expected_fingerprint),
        status: "valid",
        divergence,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    #[test]
    fn a_policy_fingerprint_is_stable_and_changes_with_every_effective_posture() {
        let original = SafetyPolicy::default();
        let same = original.clone();
        let fingerprint = policy_fingerprint(&original).expect("fingerprints");

        assert_eq!(
            fingerprint,
            policy_fingerprint(&same).expect("fingerprints")
        );
        assert_eq!(
            fingerprint.len(),
            64,
            "SHA-256 is rendered as 64 hex digits"
        );
        assert!(fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()));

        let mut narrowed = original.clone();
        narrowed.deny.push(Rule {
            pattern: "terraform destroy*".to_string(),
            origin: Origin::Operator,
        });
        assert_ne!(
            fingerprint,
            policy_fingerprint(&narrowed).expect("fingerprints"),
            "a changed rule set must never inherit the launch attestation"
        );

        let mut different_mode = original;
        different_mode.interactive_default = Verdict::Ask;
        assert_ne!(
            fingerprint,
            policy_fingerprint(&different_mode).expect("fingerprints"),
            "mode defaults are part of the effective security posture"
        );
    }

    /// Issue #168, design decision (c): a widened-and-tampered snapshot no
    /// longer fails the whole session closed -- it self-heals to the
    /// current, in-process policy (the trusted source: this same process
    /// already resolved it from `~/.zirv/ctx.toml` and the repo's own
    /// `.zirv/ctx.toml`), and never LOOSENS beyond what the current policy
    /// itself would allow.
    #[test]
    fn a_tampered_attestation_snapshot_self_heals_to_the_current_policy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let snapshot = tmp.path().join("policy.json");
        let launch = SafetyPolicy::default();
        std::fs::write(
            &snapshot,
            serde_json::to_string(&launch).expect("serializes"),
        )
        .expect("writes");
        let fingerprint = policy_fingerprint(&launch).expect("fingerprints");
        let env = env_from(&[
            (POLICY_FINGERPRINT_ENV, &fingerprint),
            (POLICY_SNAPSHOT_ENV, snapshot.to_str().expect("utf8 path")),
        ]);

        let current = SafetyPolicy::default();
        // `cargo test` matches the shipped `Bash(cargo *)` allow family
        // (mode-independent), proving self-heal evaluates the command's own
        // classification rather than the old blanket failure. The unmatched
        // command shows the per-MODE default still applies correctly under
        // self-heal (interactive `Allow`, headless `Ask`) -- not some third,
        // attestation-specific behavior. `rm -rf /` shows a semantically
        // classified (ask-family) command keeps its real verdict too. The
        // snapshot is re-tampered before EACH iteration: since `current`
        // equals the original `launch` here (both `SafetyPolicy::default()`),
        // the first self-heal's own best-effort rematerialization would
        // otherwise "fix" the file for every later iteration in this loop,
        // masking the very thing being tested.
        for (mode, command, expected) in [
            (LaunchMode::Interactive, "cargo test", Verdict::Allow),
            (
                LaunchMode::Headless,
                "some-tool-zirv-has-never-heard-of",
                Verdict::Ask,
            ),
            (LaunchMode::Headless, "rm -rf /", Verdict::Ask),
        ] {
            std::fs::write(&snapshot, "{}").expect("tamper snapshot");
            let evidence = evaluate_with_attestation_evidence(
                &current,
                command,
                mode,
                &|k| env.get(k).cloned(),
                &[],
                None,
            );
            assert_eq!(
                evidence.outcome.verdict, expected,
                "{mode:?} {command}: {evidence:?}"
            );
            assert_eq!(evidence.status, "self-healed");
        }
    }

    /// The self-heal must never widen past what `current` itself already
    /// says: a policy an operator has explicitly NARROWED still denies,
    /// even with a broken snapshot on disk.
    #[test]
    fn a_tampered_attestation_snapshot_still_honors_a_narrowed_current_policy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let snapshot = tmp.path().join("policy.json");
        std::fs::write(&snapshot, "not valid json at all").expect("writes garbage");
        let env = env_from(&[
            (
                POLICY_FINGERPRINT_ENV,
                "irrelevant-since-file-is-unreadable",
            ),
            (POLICY_SNAPSHOT_ENV, snapshot.to_str().expect("utf8 path")),
        ]);

        let mut narrowed = SafetyPolicy::default();
        narrowed.deny.push(Rule {
            pattern: "terraform destroy*".to_string(),
            origin: Origin::Operator,
        });
        let evidence = evaluate_with_attestation_evidence(
            &narrowed,
            "terraform destroy",
            LaunchMode::Interactive,
            &|k| env.get(k).cloned(),
            &[],
            None,
        );
        assert_eq!(evidence.outcome.verdict, Verdict::Deny);
        assert_eq!(evidence.status, "self-healed");
    }

    /// Self-heal best-effort re-materializes the snapshot file so the NEXT
    /// command in the same session attests cleanly again.
    #[test]
    fn self_heal_rewrites_the_snapshot_file_from_the_current_policy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let snapshot = tmp.path().join("nested").join("policy.json");
        let env = env_from(&[
            (POLICY_FINGERPRINT_ENV, "stale-fingerprint"),
            (POLICY_SNAPSHOT_ENV, snapshot.to_str().expect("utf8 path")),
        ]);
        let current = SafetyPolicy::default();

        let first = evaluate_with_attestation_evidence(
            &current,
            "cargo test",
            LaunchMode::Headless,
            &|k| env.get(k).cloned(),
            &[],
            None,
        );
        assert_eq!(first.status, "self-healed");
        assert!(snapshot.exists(), "the snapshot file must be rewritten");

        let rewritten: SafetyPolicy =
            serde_json::from_str(&std::fs::read_to_string(&snapshot).expect("read"))
                .expect("valid policy JSON");
        assert_eq!(rewritten, current);
    }

    /// Missing exactly one of the two attestation env vars is the same
    /// "invalid" shape as a corrupt file -- it must self-heal too, not fall
    /// through to some third behavior.
    #[test]
    fn a_partial_attestation_pair_self_heals() {
        let env = env_from(&[(POLICY_FINGERPRINT_ENV, "some-fingerprint")]);
        let current = SafetyPolicy::default();
        let evidence = evaluate_with_attestation_evidence(
            &current,
            "cargo test",
            LaunchMode::Interactive,
            &|k| env.get(k).cloned(),
            &[],
            None,
        );
        assert_eq!(evidence.status, "self-healed");
        assert_eq!(evidence.outcome.verdict, Verdict::Allow);
    }

    /// Code review fix: the hook's own `permissionDecisionReason` (what an
    /// operator or transcript viewer actually sees for a live decision) must
    /// also name a self-healed attestation -- not just `zirv ctx safety
    /// explain`, run separately and after the fact.
    #[test]
    fn the_hook_reason_names_a_self_healed_attestation() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let snapshot_dir = tempfile::tempdir().expect("tempdir");
        let snapshot = snapshot_dir.path().join("policy.json");
        std::fs::write(&snapshot, "not valid json at all").expect("writes garbage");
        let env = env_from(&[
            (
                POLICY_FINGERPRINT_ENV,
                "irrelevant-since-file-is-unreadable",
            ),
            (POLICY_SNAPSHOT_ENV, snapshot.to_str().expect("utf8 path")),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("loads");

        let stdin = r#"{"tool_name":"Bash","tool_input":{"command":"cargo test"},"permission_mode":"default"}"#;
        let mut out = Vec::new();
        run_check_hook_mode_with_env(&cfg, &mut out, stdin, &|k| env.get(k).cloned())
            .expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.to_lowercase().contains("self-heal"),
            "the hook's own decision reason must name a self-healed attestation: got {text}"
        );
    }

    /// Code review fix (verification, not a production change): the audit
    /// log already carries `evidence.status` verbatim (`audit_hook_decision`
    /// passes `attestation: evidence.status`), so a self-healed evaluation
    /// must already show up as `"attestation":"self-healed"` in the JSONL
    /// record. Regression-locked here so a future refactor cannot silently
    /// drop it.
    #[test]
    fn the_audit_log_records_a_self_healed_attestation() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let state = tempfile::tempdir().expect("state");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let snapshot_dir = tempfile::tempdir().expect("tempdir");
        let snapshot = snapshot_dir.path().join("policy.json");
        std::fs::write(&snapshot, "not valid json at all").expect("writes garbage");
        let env = env_from(&[
            (
                POLICY_FINGERPRINT_ENV,
                "irrelevant-since-file-is-unreadable",
            ),
            (POLICY_SNAPSHOT_ENV, snapshot.to_str().expect("utf8 path")),
            (
                super::super::state::STATE_ENV,
                state.path().to_str().expect("utf8 state"),
            ),
        ]);
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("loads");

        let stdin = r#"{"session_id":"abc","tool_name":"Bash","tool_input":{"command":"cargo test"},"permission_mode":"default"}"#;
        let mut out = Vec::new();
        run_check_hook_mode_with_env(&cfg, &mut out, stdin, &|k| env.get(k).cloned())
            .expect("runs");

        let dir = state.path().join("logs/safety-decisions");
        let file = std::fs::read_dir(dir)
            .expect("audit dir")
            .next()
            .expect("one file")
            .expect("entry")
            .path();
        let text = std::fs::read_to_string(file).expect("audit");
        assert!(
            text.contains(r#""attestation":"self-healed""#),
            "got {text}"
        );
    }

    /// Issue #139: the divergence direction itself, computed straight from
    /// `evaluate_with_attestation_evidence` -- not merely the outcome
    /// (already covered by `an_attested_launch_keeps_the_stricter_policy_
    /// and_fails_closed_on_tampering` above), but the field an explanation
    /// surface reads to decide whether to say anything at all.
    #[test]
    fn evaluate_with_attestation_evidence_reports_snapshot_stricter_when_the_current_policy_widened()
     {
        let tmp = tempfile::tempdir().expect("tempdir");
        let snapshot = tmp.path().join("policy.json");
        let current = SafetyPolicy::default();

        // The operator widened the interactive default AFTER this session's
        // own launch pinned `ask` -- the shipped default is already `allow`,
        // so the LAUNCH snapshot is the one narrowed here, compared against
        // the still-default (wider) current policy.
        let mut stricter_launch = current.clone();
        stricter_launch.interactive_default = Verdict::Ask;
        std::fs::write(
            &snapshot,
            serde_json::to_string(&stricter_launch).expect("serializes"),
        )
        .expect("writes");
        let strict_fingerprint = policy_fingerprint(&stricter_launch).expect("fingerprints");
        let env = env_from(&[
            (POLICY_FINGERPRINT_ENV, &strict_fingerprint),
            (POLICY_SNAPSHOT_ENV, snapshot.to_str().expect("utf8 path")),
        ]);

        let evidence = evaluate_with_attestation_evidence(
            &current,
            "some-tool-zirv-has-never-heard-of",
            LaunchMode::Interactive,
            &|key| env.get(key).cloned(),
            &[],
            None,
        );
        assert_eq!(evidence.outcome.verdict, Verdict::Ask, "{evidence:?}");
        assert_eq!(
            evidence.divergence,
            SnapshotDivergence::SnapshotStricter {
                current_verdict: Verdict::Allow
            },
            "the snapshot is stricter than today's policy for this unmatched command"
        );
    }

    /// The `Unchanged` half: an attested launch whose snapshot agrees with
    /// the current policy for a given command must not report a divergence,
    /// even though attestation is active.
    #[test]
    fn evaluate_with_attestation_evidence_reports_unchanged_when_the_snapshot_agrees() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let snapshot = tmp.path().join("policy.json");
        let launch = SafetyPolicy::default();
        std::fs::write(
            &snapshot,
            serde_json::to_string(&launch).expect("serializes"),
        )
        .expect("writes");
        let fingerprint = policy_fingerprint(&launch).expect("fingerprints");
        let env = env_from(&[
            (POLICY_FINGERPRINT_ENV, &fingerprint),
            (POLICY_SNAPSHOT_ENV, snapshot.to_str().expect("utf8 path")),
        ]);

        let evidence = evaluate_with_attestation_evidence(
            &launch,
            "cargo build",
            LaunchMode::Interactive,
            &|key| env.get(key).cloned(),
            &[],
            None,
        );
        assert_eq!(evidence.divergence, SnapshotDivergence::Unchanged);
    }

    /// Issue #139's own root cause, end to end: `zirv ctx safety explain`
    /// used to bypass attestation entirely, so it disagreed with the hook
    /// for the identical command whenever a session's launch snapshot was
    /// stricter than the current policy. Now routed through the same
    /// evidence function, the two surfaces agree, and `explain`'s own text
    /// names the divergence explicitly.
    #[test]
    fn run_explain_agrees_with_the_hook_when_the_launch_snapshot_is_stricter() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let snapshot = tmp.path().join("policy.json");
        let stricter_launch = SafetyPolicy {
            interactive_default: Verdict::Ask,
            ..SafetyPolicy::default()
        };
        std::fs::write(
            &snapshot,
            serde_json::to_string(&stricter_launch).expect("serializes"),
        )
        .expect("writes");
        let fingerprint = policy_fingerprint(&stricter_launch).expect("fingerprints");
        let env = env_from(&[
            (POLICY_FINGERPRINT_ENV, &fingerprint),
            (POLICY_SNAPSHOT_ENV, snapshot.to_str().expect("utf8 path")),
        ]);

        let args = ExplainArgs {
            repo: repo.clone(),
            mode: LaunchMode::Interactive,
            command: vec!["some-tool-zirv-has-never-heard-of".to_string()],
        };
        let mut out = Vec::new();
        let code = run_explain(&args, &mut out, &|k| env.get(k).cloned()).expect("runs");
        assert_eq!(code, Verdict::Ask.exit_code());
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("launch snapshot"),
            "explain must name the divergence, agreeing with what the hook would say: {text}"
        );
        assert!(
            text.contains("allow"),
            "must name the current policy's own verdict: {text}"
        );
    }

    /// Without the attestation env vars, `run_explain`'s behavior is
    /// byte-for-byte what it was before this fix: no snapshot, no
    /// divergence note, plain current-policy evaluation.
    #[test]
    fn run_explain_stays_unattested_without_the_env_vars() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let empty: HashMap<String, String> = HashMap::new();

        let args = ExplainArgs {
            repo,
            mode: LaunchMode::Interactive,
            command: vec!["some-tool-zirv-has-never-heard-of".to_string()],
        };
        let mut out = Vec::new();
        let code = run_explain(&args, &mut out, &|k| empty.get(k).cloned()).expect("runs");
        assert_eq!(code, Verdict::Allow.exit_code());
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("launch snapshot"),
            "no attestation in play, so no divergence note: {text}"
        );
    }

    /// Code review fix: `zirv ctx safety explain` used to silently evaluate
    /// against the current policy on a self-healed attestation, with no
    /// indication anything was wrong -- an operator reading the explanation
    /// would have no way to know the launch snapshot was invalid and the
    /// answer came from self-heal rather than a verified attestation.
    #[test]
    fn run_explain_names_a_self_healed_attestation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let snapshot = tmp.path().join("policy.json");
        std::fs::write(&snapshot, "not valid json at all").expect("writes garbage");
        let env = env_from(&[
            (
                POLICY_FINGERPRINT_ENV,
                "irrelevant-since-file-is-unreadable",
            ),
            (POLICY_SNAPSHOT_ENV, snapshot.to_str().expect("utf8 path")),
        ]);

        let args = ExplainArgs {
            repo,
            mode: LaunchMode::Interactive,
            command: vec!["cargo".to_string(), "test".to_string()],
        };
        let mut out = Vec::new();
        run_explain(&args, &mut out, &|k| env.get(k).cloned()).expect("runs");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.to_lowercase().contains("self-heal"),
            "an invalid attestation snapshot must be named as self-healed, not silently ignored: {text}"
        );
    }
}
