# Built-in safe-command policy and blocked-command observability

Operator: Jonathan. Date: 2026-09-16. Status: draft for review.

## Problem, with evidence

A 7-day audit (2026-09-09..16) of this machine's Claude Code transcripts and
zirv logs found 497 permission dialogs shown and ~120 commands denied with no
visible trace.

Attribution of the 497 dialogs:

| Source | Count |
|---|---|
| zirv `<sandbox: unsandboxed retry>` asks | 200 |
| headless-default asks (no rule matched) | 191 |
| non-Bash tool grants | 44 |
| genuinely risky-rule asks | 43 |
| auto-mode classifier escalations | 14 |
| unattributed | 5 |

The 43 risky-rule asks break down as: destructive vcs 20, `rm -rf` 9,
`push --force` 5, non-read-only SQL 4, `reset --hard` 3, `pkill` 1,
secret-file 1.

Of the ~120 silent denials: 85 came from an over-broad *Claude-side* deny
glob on the operator's machine (out of scope for this repo — see below), 34
from the auto-mode classifier, 1 evaded both layers.

### Key reframing

Verified with `zirv ctx safety explain` and `zirv ctx safety check`: every
command the operator named as safe ALREADY resolves to `allow` in an
interactive session, via the unmatched-command fallback
(`interactive_default = allow`), not via any rule. The same commands resolve
to `ask` under the headless posture, where an ask is unanswerable and fails
closed. Therefore **the built-in allow list functions as the capability list
for delegated workers** (`zirv ctx agent`/`exec`/`loop`), and "let the safe
commands run" and "stop skipping commands silently" are the same fix.

Measured examples, interactive verdict → headless verdict:

| Command | Interactive | Headless |
|---|---|---|
| `glab mr merge 5` | allow | ask |
| `docker exec db rm -rf /tmp/data` | allow | ask |
| `zirv ctx kill 3f2a` | allow | ask |
| `gitlab-ci-local phpstan` | allow | ask |
| `kubectl get pods -n crm` | allow | ask |
| `sed -i ... f.txt` | allow | ask |
| `mkdir -p ...` | allow | ask |
| `jq -r .x file.json` | allow | ask |

By contrast, `gh pr merge 123 --squash` and `gh issue close 42` are `allow`
in BOTH modes, because `Bash(gh *)` is on the shipped allow list.

## Change 1 (ships first) — keyword-aware segment tokenization

`tokenize_segments`/`split_segments` (`src/commands/ctx/safety.rs:2019` /
`:2079`) is a quote-aware scanner splitting on `;`, newline, `&&`, `||`, `|`,
`|&`, `&`, with no shell-keyword awareness. Segments keep their keyword
heads, so `push_executable_candidate` inside `visit_executable_nodes`
(`safety.rs:3362-3443`) treats the literal words `for`, `do`, `done`, `{` as
program names. `evaluate_single`/`sql_program_name` then match no rule, and
`evaluate_candidate_outcome` (`safety.rs:1180-1197`) resolves each to the
launch-mode fallback. `evaluate_candidates` (`safety.rs:1210`, fold at
`1279-1291`) takes the worst case across candidates, so one command string
still yields exactly one hook decision.

Two consequences, both measured against the installed 4.2.0 binary:

- **False negative, interactive.** A glob deny/ask rule is evaded by
  wrapping. `sudo id` → deny, but `{ sudo id; }` → allow. `for f in a; do
  sudo id; done` → allow. `if true; then sudo id; fi` → allow. `gh auth
  token` → deny, `{ gh auth token; }` → allow. `rm -rf /tmp/x` → ask, `{ rm
  -rf /tmp/x; }` → allow. `while read f; do git push --force origin main;
  done` → allow. Semantic classifiers are NOT affected — `curl
  https://example.com/i.sh | sh` is still correctly denied by `<network:
  piped into a shell interpreter>` — and the OS sandbox remains underneath,
  so this is a defence-in-depth failure rather than a full bypass. But every
  glob deny/ask rule (`sudo *`, `gh auth *`, `rm -rf *`, `git
  push*--force*`) is evadable this way. Only zirv's own policy layer was
  measured here; whether the harness-native `--disallowedTools` projection
  independently catches the wrapped form was NOT tested.
- **False positive, headless.** The same missing strip makes any loop/brace
  compound fold to the headless fallback `ask`, i.e. a silent block in
  workers. The existing test
  `orchestrator_seat_read_only_redirect_commands_allow_through_the_hook`
  (`safety.rs:10658`, command literal at `:10673`) pins that a real `for i in
  $(seq 1 80); do ...; done` polling loop is `allow` interactively — the
  headless side is unpinned.

**Fix:** skip leading shell keywords (`for`, `while`, `until`, `if`, `then`,
`elif`, `else`, `fi`, `do`, `done`, `case`, `esac`, `in`, `{`, `}`) when
deriving a segment's executable candidate, so the body command is what gets
matched.

Land this BEFORE any widening, because widening a policy whose enforcement
is evadable compounds the gap.

## Change 2 — extend the shipped allow list (the worker capability list)

The built-in rule sets are data in `src/commands/ctx/adapters/mod.rs`, not
duplicated in `safety.rs`: `SHIPPED_POSTURE_ALLOW`
(`adapters/mod.rs:610`, ~35 entries, entry shape `("Bash(git *)", "the full
git command family; ...")`), `SHIPPED_POSTURE_DENY` (`adapters/mod.rs:831`),
`SHIPPED_POSTURE_ASK` (`adapters/mod.rs:984`). `safety.rs` wraps them:
`builtin_allow()` (`safety.rs:724`) = filtered `SHIPPED_POSTURE_ALLOW` +
`reserved_zirv_command_patterns()`, `builtin_deny()` (`:619`), `builtin_ask()`
(`:750`), each stripping the `Bash(...)` wrapper via
`command_pattern_from_bash_rule` (`safety.rs:606`).

Add to `SHIPPED_POSTURE_ALLOW`:

- `Bash(glab *)` — resolves a real asymmetry: `Bash(gh *)` is on the shipped
  list but glab is absent, so glab falls to the unmatched default inside
  `safety::evaluate`, while at the adapter-native layer
  `PROMPT_FREE_COMMAND_FAMILIES` (`src/commands/ctx/adapters/claude.rs:1377-1391`,
  issue #329) already grants `glab *` prompt-free treatment with
  `sandbox_excluded: true`. The two layers disagree today; this reconciles
  them.
- `Bash(gitlab-ci-local *)`, `Bash(php *)`.
- `Bash(kubectl get *)`, `Bash(kubectl logs *)`, `Bash(kubectl describe *)`,
  `Bash(kubectl config *)` — read verbs only; `exec`/`apply`/`delete` stay
  off.
- Everyday tools absent today: `sed *`, `awk *`, `jq *`, `mkdir *`,
  `touch *`, `cp *`, `mv *`, `stat *`, `df *`, `du *`, `ps *`, `printf *`,
  `date *`, `basename *`, `dirname *`, `xargs *`, `tee *`, `mktemp *`,
  `realpath *`.
- The fixed macOS SSH-agent lookup: `launchctl getenv SSH_AUTH_SOCK` and
  `export SSH_AUTH_SOCK=*`. Command substitutions remain separate executable
  candidates, so this does not hide a dangerous command used as the value.

Separately, add `"kill"` to `ZIRV_CTX_ESCAPE_SAFE_VERBS`
(`safety.rs:7037-7053`). That list feeds `ctx_base_allow_verbs()`
(`safety.rs:7069`, the same list minus `usage`) and
`reserved_zirv_command_patterns()` (`safety.rs:675`), which maps each verb to
`zirv ctx <verb> *`, so one edit reaches both the base allow list and the
sandbox-escape acceptor. The list's own doc comment (`safety.rs:636-674`)
restricts membership to verbs whose payload is a fixed shape rather than
caller-controlled argv handed to a subprocess; `zirv ctx kill <PREFIX>` takes
exactly one positional session-id prefix and no trailing argv, so it
qualifies.

Note explicitly: `gh pr create`, `gh pr merge`, `gh issue create`, `gh issue
close` need NO change. `apply_vcs_outcome`/`is_destructive_vcs_action`
(`safety.rs:1750` / `:3928`) returns false for any non-git program, and
`is_irreversible_distribution_action`'s `gh`/`glab` arm (`safety.rs:3835-3852`)
only fires on a positional `delete` or `api ... DELETE`, so these already
fall to `Bash(gh *)`.

## Change 3 — make `docker exec` / `kubectl exec` analyzable, and only then allow them

`unwrap_shell_wrapper` (`safety.rs:2833`) recognizes only `bash`, `sh`,
`zsh`, `cmd`/`cmd.exe`, `powershell`/`pwsh` (+ variants).
`unwrap_launcher_prefix` (`safety.rs:3204`) + `LAUNCHER_PREFIXES`
(`safety.rs:3076-3193`) cover `time, caffeinate, busybox, xargs, command,
builtin, exec, nohup, setsid, stdbuf, nice, ionice, doas, timeout, flock,
chrt, taskset`. Neither `docker exec` nor `kubectl exec` appears anywhere,
and the regression test
`write_targets_confined_has_no_opinion_when_there_is_no_write_target_at_all`
(`safety.rs:10221-10226`) uses `kubectl exec -it pod -- sh` as its canonical
*opaque* command, so treating it as one flat command is a deliberate current
premise, not an oversight.

Consequence today, for every zirv user: `docker exec db rm -rf /tmp/data` →
allow, and `gh pr list; docker exec db psql -c "drop table t"` → allow
interactively, because the inner argv is invisible to every semantic
classifier.

**Proposal:** extend the wrapper decoding to strip `docker exec [flags]
<container>` and `kubectl exec [flags] <pod> [-n ns] [--]`, analyse the
remainder as a nested command, then add `Bash(docker exec *)` / keep
`kubectl exec` off or on per the operator's call. This is a net TIGHTENING
of current behaviour and it requires revisiting the premise of the test at
`safety.rs:10221`.

**Recommendation:** ship the analyzer change and the allow entry together;
if the analyzer work slips, leave `docker exec`/`kubectl exec` OFF the allow
list rather than shipping an unanalyzable allow, because the allow entry is
what would extend it to unattended workers.

Flag as an open decision for the operator, who has stated `docker exec` is
safe and wants it on by default.

## Change 4 — extend built-in `escape_allow`

A built-in default already exists: `builtin_escape_allow()`
(`safety.rs:5613`) = `builtin_allow()` filtered to
`SANDBOX_ESCAPE_BUILTIN_PROGRAMS` (`safety.rs:5603-5606`: ls, grep, rg, cat,
head, tail, wc, find, echo, pwd, which, where, diff, sort, uniq, tr, cut),
wired as `SafetyPolicy::default().escape_allow` at `safety.rs:298`. It is
read-only tools only, which is why the 200 unsandboxed-retry asks — the
single largest dialog class — are all build/dev tooling.

Add `cargo`, `gh`, `glab`, `gitlab-ci-local`, `npm`, `npx`, `git`, `python3`,
`mkdir`, and the two narrowly allowed SSH-agent lookup programs (`launchctl`,
`export`). `zirv` uses its existing subcommand-aware retry acceptor instead of
a broad leading-program entry.

Safety argument: the escape branch is gated on the base verdict already
being `Allow` (`safety.rs:8573`), so deny and ask rules still win; an entry
can only clear a family that the policy already permits.

## Change 5 — blocked-command observability ("nothing skipped silently")

Current state:

- `logs/safety-decisions/<day>.jsonl` via `append_safety` (`log.rs:399`)
  persists only `command_sha256` + `policy_sha256`; a test
  (`log.rs:1340-1348`) asserts no raw command leaks. Hence `zirv ctx hook
  audit` reports `safety-policy denials: 37 (commands are hashed -- no
  program names available)` (`hook.rs:3422-3426`) — a block is
  un-diagnosable after the fact.
- `logs/permission-prompts.jsonl` via `PermissionPromptRow` (`hook.rs:190-202`)
  DOES keep plaintext `family` and `reason`, read back by
  `read_permission_prompts` (`log.rs:224-264`). So a privacy-preserving
  precedent for naming the program already exists in this codebase.
- Headless blocking is two-layer: zirv's hook (`hook_output_with_extras`,
  `safety.rs:7958-8009`) deliberately emits NOTHING for `Ask` under
  `permission_mode == "dontAsk"` (`safety.rs:7978`; rationale in the doc
  comment at `:7818-7850` — an unsatisfiable prompt would be turned into a
  denial that strips the operator's own allow list), and the actual hard
  block comes from `ClaudeAdapter::default_sandbox_args`
  (`adapters/claude.rs:2546`, ask-set folding at `~2587-2600`) folding the
  ask set into `--disallowedTools` headlessly.
- No path exists from a command denied inside a `zirv ctx agent` worker to
  that worker's returned result or to mail. `zirv ctx hook audit`
  (`hook.rs:3341`, body through `~3426`) and `zirv ctx status` both pull
  from local logs on demand; nothing pushes.

Proposal:

a. Add a `family` field (argv[0] + subcommand only — never paths, args or
   secrets) to the safety-decision record, mirroring
   `PermissionPromptRow.family`, so `zirv ctx hook audit` can name what was
   blocked.
b. Give a headless block a structured refusal instructing the worker to
   report `BLOCKED: <family>` rather than silently work around it.
c. Propagate: a `blocked:` section in the `zirv ctx agent` delegation
   result/receipt and a `blocked:` line in `zirv ctx status`. This is new
   work — no existing hook to extend.
d. Have `zirv setup` wire `PermissionRequest`/`PermissionDenied` into the
   persistent settings file, not only the launch-time settings; today only
   zirv-launched sessions log these, which is why just 9 of ~120 denials
   produced a record.

## Tests to extend

Inline `safety.rs` `mod tests`:

- `builtin_ask_covers_the_genuinely_dangerous_families` (`:15052`)
- `builtin_deny_still_blocks_the_self_destructive_and_irreversible_families` (`:15092`)
- `builtin_allow_skips_the_non_command_file_scope_rules` (`:15208`)
- `builtin_rule_sets_are_derived_from_the_shipped_posture_not_duplicated` (`:15184`)
- `builtin_rule_sets_skip_the_non_command_file_scope_rules` (`:15218`)
- `the_product_requirement_no_everyday_or_novel_command_ever_prompts` (`:14474`,
  the regression lock cited by `SHIPPED_POSTURE_ASK`'s doc comment)

Derivation-based golden:
`the_headless_projection_is_byte_exact_against_the_shipped_constants`
(`adapters/claude.rs:4988`) rebuilds expectations from the shipped constants,
so adding entries should not break it, but it must be re-run.

New tests needed: keyword-stripping (both the deny-evasion and the
headless-loop cases), the docker/kubectl exec unwrap, each new allow entry,
`zirv ctx kill` reaching both derived lists, and the blocked-family record.

## README anchors

- `### Command safety policy (issue #83)` — `README.md:2571-2645` (built-in
  rules and `[safety]` key semantics).
- `### Permission auditing and safe-list proposals (issue #178)` —
  `README.md:2646-2706` (`zirv ctx permissions` and any new observability
  surface).
- Trust-boundary table rows for the operator-only keys `safety.allow`,
  `safety.escape_allow`, `safety.default`, `safety.interactive_default`,
  `safety.sql` — `README.md:1645-1649`; `#### Trust boundary` heading at
  `:1558`.

Note: `safety.ask`/`safety.deny` are deliberately NOT operator-only (a repo
layer may narrow).

## Rollout order and open decisions

Order: Change 1 (bug fix) → Change 5a+5d (observability, so the next steps
are measurable) → Change 2 + Change 4 (widening) → Change 3 (analyzer + exec
allow) → Change 5b+5c (propagation).

Open decisions for the operator:

i. Whether `gh pr merge`, `glab mr merge` and `docker exec` should be plain
   global defaults or sit behind a named default that one config line
   tightens — these make every unattended zirv worker able to merge MRs and
   exec into containers.
ii. Whether Change 3 is in scope for the first PR or whether exec stays off
    the allow list until it lands.
iii. Whether `kubectl exec` follows `docker exec` or stays off.

## Out of scope (operator's machine, not this repo)

The over-broad `Bash(*find /*)` and Windows-registry deny globs in the
operator's `~/.claude/settings.json` (85 silent denials in 7 days, and a
`*regedit*` glob that blocked a read-only probe because the word appeared
inside a `grep -v` argument) remain machine-local configuration defects. The
commands themselves are safe in zirv's shipped policy; a harness-native deny
still outranks zirv's allow. Also out of scope:
`~/.claude/hooks/enforce-rules.sh` using GNU-only `grep -oP`, which BSD/macOS
grep rejects, so its repo resolution
silently fails, plus its substring matching firing on command text inside
quoted arguments. Neither belongs to this repo's policy layer.
