# Changelog

Generated from conventional-commit subjects (`feat`, `fix`, `perf`, `docs`,
plus `chore`/`refactor`/`test`/`style`/`ci`/`build`/`revert` folded into
Chores) by `scripts/generate-changelog.sh`. Never hand-edit this file --
regenerate a section instead:

    scripts/generate-changelog.sh --section --from <prev-tag> --to <new-tag>

`.github/workflows/cd.yaml` runs that command for every release and commits
the new top section here. A commit subject that does not match
`type(scope)!: summary` (most of the history before this file existed) is
listed under "Other" rather than dropped or guessed at. A `!` right after
the type/scope is the only breaking-change signal this script reads -- a
`BREAKING CHANGE:` footer in the commit body is not parsed. Full-rebuild
command: `scripts/generate-changelog.sh --full`.

## v4.17.0 (2026-09-21)

### Features

- portable cross-model skill library with integration-aware activation (#539) (#708)
- mask sensitive data before model egress (#706)
- choose the default harness by what is installed, and say so (#690)
- route a seat's model tier, and say "configuration error" once
- reuse a passing check whose inputs did not change (#699)
- shift the review rubric and test-first loop into implement (#699)
- cover the script runner and workflow
- make the implement-vs-validate split measurable (#699 phase 0)
- finish the first-run wizard and classify the new commands
- add zirv tour and zirv ctx jev status
- make Jev decisions deterministic: margin gate, frozen intake state, pinned model, decision cache
- gated Jev advisory for review triage, gate reclassification and artifact substance
- Jev supervisor pre-filters and dispatch model tiering, off by default
- proxy clarification and domain tags, leaner intake state, Jev-ranked memory retrieval and harvest gate
- jev::advise as the single gated advisory entry point
- shared Jev decision client and operator-only [jev] config table
- harness proxy -- intake decision via TypeSafe Jev with helper and deterministic fallback
- surface blocked commands in the delegation receipt
- extend the shipped allow list and escape_allow for the worker capability set
- blocked-command observability (Change 5a/b/d, reduced 5c)

### Fixes

- deterministic workflow triggering; steer wrapped sessions and subagents to skills (#714)
- resolve thin-margin complexity/risk answers upward in the proxy intake (#709)
- stop repeated wrapped-session permission prompts (#712)
- preserve source sessions when automatic rollover fails (#711)
- address review findings on the three terminal and intake fixes
- floor complexity for an explicit parallel/multi-agent delegation request
- keep a drag-selection highlight across ticks, and give the proxy intake prompt a real cursor
- re-prompt instead of skipping the proxy on a blank first line
- stop overriding the operator's own claude permission mode
- run the proxy intake on the native runtime too
- fail fast when the harness is not installed (#690)
- pin the hook-install fallback test to an injected machine
- select and scroll together, and copy only the pane (#697)
- collapse the first-run harness storm and stop claiming success without one
- name the harness, the config layer, and the package manager
- accept --version, mark the machine configured, repair a partial .zirv
- correct package metadata and make release templating robust
- dispatch tier override carries the whole tool input in updatedInput
- keep the deterministic frontend trigger byte-identical when the Jev gate is off
- approvals never become findings, no auto-dismiss of Critical/Major, wider test-runner safety net
- drop the removed harnesses field from the typesafe wrapper test
- memory reranking never truncates or drops candidates when Jev is off or silent
- keep Question::noul allowed as dead code until its intake caller lands
- append Jev decisions atomically and re-point the parity matrix at the moved tests
- proxy derives execution from complexity and launches a single seat
- narrow launchctl SSH agent access
- silence safe command permission prompts
- preserve pipe structure across a keyword-stripped segment head
- keyword-aware segment candidates and docker/kubectl exec decoding

### Documentation

- document tour and ctx jev status where the gates require it
- spec for built-in safe-command policy and blocked-command observability
- disclaimer, ctx module boundaries, latency targets, generated changelog (#430, #431, #432, #433)

### Chores

- bump version to 4.11.0
- bump version to 4.8.0
- drop the dead-code allow on Question::noul now that intake calls it
- cover the typesafe wrapper's delegation to jev::ask
- 4.4.0

## v4.0.2 (2026-09-16)

### Fixes

- launch a native successor from a wrap seat instead of parking (#632) (#662)

## v4.0.1 (2026-09-16)

### Fixes

- send the prompt once and recover an overflow with no settled boundary (#649, #647) (#661)

## v4.0.0 (2026-09-16)

### Other

- Release preparation: native harness coming soon (#469) (#493)

## v3.48.0 (2026-09-15)

### Features

- expose scoped read-only MCP tools for wrapped hosts

### Fixes

- patch Rustls advisory and synchronize nudge test readiness

## v3.47.0 (2026-09-11)

### Features

- health-aware routing slice 2 -- degraded phase, shared-endpoint aliases, single half-open trial, partial-stream handoff reconciliation (#455)

## v3.46.1 (2026-09-10)

### Fixes

- clear Approval mid-turn and price the seat + native subagents in session spend (#456, #457)

## v3.46.0 (2026-09-10)

### Features

- park the displaced harness and resume its own conversation on the return

### Fixes

- resume the harness's own conversation after a failed rollover (#462)

### Chores

- 3.46.0 (parked-harness return is a feature, not a patch)

## v3.45.0 (2026-09-10)

### Fixes

- tolerate missing Linux sandbox dependencies and report chat launch failures (#461)

## v3.44.0 (2026-09-10)

### Other

- 3.44.0: delegation receipts + persisted worker reports (#452, #454), health-aware routing slice 1 (#455), zirv works from $HOME (#459)

## v3.43.0 (2026-09-10)

### Other

- 3.43.0: migrate durable vault facts into .zirv/memory and remove the Obsidian vault (#451)

## v3.42.0 (2026-09-10)

### Other

- 3.42.0: bundled [[output.filter]] defaults and compact_search on by default (#449)

## v3.41.0 (2026-09-09)

### Other

- 3.41.0: operator [[output.filter]] compaction rules (#417) + claude/codex endpoint overrides (#395) (#448)

## v3.40.0 (2026-09-09)

### Chores

- push the Homebrew formula with a tap deploy key instead of a PAT (#446)

### Other

- 3.40.0: zirv ctx ask (#310) + native pre/post tool hook seams for copilot, droid and gemini (#418) (#447)

## v3.39.0 (2026-09-09)

### Documentation

- exhaustive grouped Features section derived from the command surface (#407) (#444)

### Other

- 3.39.0: README supported harnesses and models section; gpt-6-astra as an OpenAI ladder rung (#445)

## v3.38.1 (2026-09-09)

### Chores

- require a version bump only when shipped code changed (3.38.1) (#443)

## v3.38.0 (2026-09-09)

### Other

- 3.38.0: rollover terminal row + source resume (#440), dash kill ownership (#435), safety tokenizer + |& (#421), codex resume_target (#303), zirv ctx learn (#425) (#442)

## v3.37.0 (2026-09-09)

### Other

- 3.37.0: discover pass (#423), hook audit + status (#424), hook integrity (#420), compaction-ratio + test-presence + security-scan CI (#426 #428 #429), gh/glab template strip (#416), opt-in search compaction (#414), reuse probe fix (#435) (#441)

## v3.36.0 (2026-09-08)

### Other

- 3.36.0: rtk-derived compaction release -- never-worse guard, grouping, dedup, JSON, diff scope, test extractors, savings ledger, updatedInput seam (#437)

## v3.35.0 (2026-09-08)

### Other

- 3.35.0: overcomplication guard (#406), kill contract (#403), read-only advisory notice (#399), compacting attention (#379) (#436)

## v3.34.1 (2026-09-08)

### Fixes

- banner roster omits harnesses whose binary is confirmed absent

## v3.34.0 (2026-09-08)

### Features

- audit a worker's claimed deliverables against git and give a failed contract its own exit code

### Fixes

- review round 1 -- fail-closed envelope roots, audited recovery reports, configured crash-witness retention, untracked-directory deliverables
- read-only codex panes can deliver mail; settled panes that sent none get a transcript-derived report (#402)
- let a fable seat roll over, admit stale weekly readings, force a blocked reactive rollover after a grace (#401)
- keep crash witnesses through session listing, terminate headless children as a process group, permit liveness by pid and start time
- refuse operator and harness homes as workdirs, resolve symlinks in envelope containment, peel time/caffeinate/busybox

### Chores

- bump version to 3.34.0

## v3.33.0 (2026-09-08)

### Fixes

- envelope scope kinds must match, empty hook cwd falls back to the process directory
- command -v/-V are resolution queries, exec skips fd redirections
- collision-resistant repository slug with one-shot state migration
- peel xargs/command/builtin/exec, ask on project secret files, enforce the delegation envelope consistently
- atomic config writes, confined restore destinations, typed objective steps, README completeness checks
- prune archives ignored content; verify reads git paths NUL-delimited
- mask secrets in every runner-generated line

### Chores

- tolerate a one-second clock tick in the codex rollout readout
- bump version to 3.33.0
- modern actions, release provenance attestation, advisory job, anyhow 1.0.104

## v3.32.0 (2026-09-08)

### Features

- Qwen Code adapter (#389)
- Factory Droid adapter (#388)
- GitHub Copilot CLI adapter (#387)
- OpenCode adapter (#385)
- Gemini CLI adapter (#384)
- Pi coding-agent adapter (#386)

### Fixes

- review fixes for the wave 1-2 adapters

### Documentation

- vault pages for the wave 1-2 harness adapters

### Chores

- bump version to 3.32.0

## v3.31.0 (2026-09-07)

### Features

- resolve the billed provider per launch from the pinned model (#383)
- model catalogue module (#381)
- shadow transcript source for snapshot and SQLite harnesses (#382)

### Fixes

- derive the shadow cursor from the shadow itself, not a sidecar file
- drop the invented codex window, add per-vendor as_of
- sqlite shadow sync reads WAL live instead of via immutable=1

### Documentation

- wave-1 foundation pages; catalogue dead-code note names its real callers

### Chores

- bump version to 3.31.0

## v3.30.0 (2026-09-07)

### Fixes

- a zero drain share takes nothing; loop the reap-hold test on the hold, not the text
- fair drain shares, hold the reap for queued output, throttle the inline facts
- an agent step skipped for this platform never lowers the process priority
- apply the worker posture at dispatch, never inside a supervisor
- swap facts snapshots every tick and move the pool read off it
- stub the version-resource embedder on non-Windows hosts
- embed VERSIONINFO, publish checksums, verify zirv update
- give the operator's seat scheduling priority over its own workers
- keep the tick off the disk so typing stays responsive

### Documentation

- state the host-gated cross-compile behaviour of the version resource honestly
- record the 3.30.0 dashboard tick, scheduling posture and release hardening

## v3.29.1 (2026-09-07)

### Documentation

- add community standards files (license, code of conduct, contributing, security policy, issue and PR templates); bump to 3.29.1

## v3.29.0 (2026-09-07)

### Features

- add `zirv update [--version x.y.z]` self-update from GitHub releases; bump to 3.29.0

## v3.28.0 (2026-09-07)

### Features

- prompt-gated `zirv ctx config show|set|add` for ~/.zirv/ctx.toml; remedies name the command (#373)

### Fixes

- sent_since matches a worker's full-uuid sender by short id and ignores zirv system notices (#369 review)
- early pane death mails exit tail and ledgers failed; reminder honours a worker's own report mail; exit evidence carries the output tail; reap releases the writer permit (#368, #369)
- warn that codex read-only seats cannot build; test/verify auto-spawns are writing seats (#371)

### Documentation

- mirror the ctx config verb in the skill template, its fixtures and the enforced verb list (#373)

### Chores

- point repository references at the renamed Glubiz/zirv-cli
- bump to 3.28.0

## v3.27.1 (2026-09-07)

### Fixes

- publish releases draft-first so assets land before immutability; skip an already published version; bump to 3.27.1

## v3.27.0 (2026-09-07)

### Fixes

- read-only codex workers carry exactly one --sandbox flag; surface a pane that exits right after launch
- keep the mail sender tracked until confirmation or child exit; persist the settled-report latch (#363)
- confirm pane mail submission, hook mail marker, read-only status, settled-pane report-back (#363)
- stop rendering the interactive pacing prompt as a pane header error; truthful codex usage hint
- name a linked worktree's own gitdir as a codex writable root, warn on Windows (#364)
- track serial libtest verdicts across interleaved output, compact repeated git warnings (#361)

### Chores

- bump version to 3.27.0

### Other

- Create FUNDING.yml

## v3.26.0 (2026-09-06)

### Features

- compact tool output with reversible retrieval and PostToolUse rewrite

### Fixes

- round-2 review -- nested pipe wrappers, git global flags, summary splice, --bytes end
- failure-first summaries, reader-safe compaction scope, byte-exact reads
- expand and narrow-rule the compact-run wrapper, never replace it
- restore full-bank key precedence for memory retrieval; clamp the workflow-context marker
- protect verification and bound protected-list counts in the handoff carry-over
- resolve the read-only pane floor against the real CLI surface, not req.interactive
- orchestrator-side token trims plus three audited memory/inbox/search caps
- bound every unbounded capped-layer surface in track B
- zirv ctx run --compact is a transparent launcher
- interactive read-only panes must not carry exec-only ignore flags
- never retry a dead endpoint probe (3.25.0 startup freeze)

### Documentation

- sync status-default wording and the handoff/contract review-round fixes
- 3.26.0 token efficiency release -- active work, journal, decisions, known issues
- zirv ctx status is brief+diff by default, --full for the whole report
- list the new output and run ctx verbs (ZCHK-DOC-VERBS)

### Chores

- bump version to 3.26.0

## v3.25.0 (2026-09-06)

### Fixes

- floor a pane handover's rollout pin before the successor spawns
- never redirect an operator's explicit --transcript
- builtin checks key applicability on the repo, not on a file
- a dry run does not check a step this OS never runs
- a dry run resolves what an earlier step captures
- a handover drops the previous child's codex rollout pin
- price a pane off its own cwd, not the dashboard's repo
- re-resolve a transcript that does not exist while the child runs
- taskset and chrt no longer swallow the command they launch
- joined destination flags no longer bypass the operator ~/.zirv deny
- pace.collector_max_age_secs is repo-forbidden, not narrow-only
- release the provider reservation on the two pre-launch refusals
- pin that every ZIRV_CTX_PACE* name the crate uses exists in ENV_MAP
- a stale-but-available usage reading ranks on its real headroom
- a repo layer may not choose the pacing gate's own fallback reading
- gate the rot restart on the restart chain
- deliver the interactive handoff off argv
- bound the distiller's stdout capture
- write the nudge and stall markers atomically
- sweep a dead session's published socket path
- never clobber a restart-chain record this build cannot parse
- write a permit slot's child pid atomically
- bound the orchestrator-block read on the hook hot path
- retain 30 days of safety-decision buckets and read them once
- lock the attention ledger's read-modify-write
- word a cadence finding from the sign of its z-score
- size stripped thinking blocks from the reported thinking tokens
- read subagent spend from the subagents directory
- dedupe the usage-window folds by API response too
- count claude transcript usage once per API response
- teach both deletion classifiers PowerShell's ri alias
- docker and aws teardown verbs ask like their prune siblings
- see through ordinary launcher prefixes to the program they run
- deny direct writes and deletes under the operator's ~/.zirv
- find cmd.exe's inline-command switch after its leading switches
- unwrap every PowerShell -Command abbreviation, not just the full spelling
- add `zirv init --yes` and refuse a non-TTY init before writing
- reject an unknown key under a step's options
- --dry-run rejects an unresolved placeholder too
- clamp a repo-only harness reserve to the global floor
- refuse a drive-relative shortcut target
- a concurrent block substitutes like a single command
- refuse a tag containing a comma
- screen the committed shared bank as hard as machine-local mail
- journal and restore every body an overwrite removed
- refuse a key with an embedded newline on every tier
- forget reports the keys a stray file still claims
- split the codex state-dir seam by cfg so no return is needed
- a pane delegation writes its own ledger row
- resolve the rollout codex actually minted, and pin it
- the dashboard footer's session spend counts only this session
- price gpt-6-astra at the sol tier
- a builtin self-check that does not apply here is not a failure
- stage a restore's copy before removing the destination
- never prune the backup run a restore is reading
- an armed dash prefix followed by a modified key is not a chord
- Ctrl+Alt+<x> keeps its control bit through the dash Alt fast-path
- the durable objective spend is monotone, never overwritten by a restart
- a zero token ceiling refuses admission instead of running unbounded

### Documentation

- audit round 2 -- active work, journal rotation, decision log, known issues residuals, state layout
- interactive handoff delivery off argv, and wrap's restart chain
- usage dedup by API response, the subagents directory, thinking fallback
- record the 2026-09-06 safety audit fixes A1..A6
- audit round A-2 -- runner, shortcuts, init and harness reserve
- forget outcome, shared-bank screen parity, journal before_bodies
- pane delegations write their own ledger row, codex rollout pinning, footer spend filter
- not-applicable builtin outcome and setup restore safety
- zero ceilings refuse admission, objective spend is monotone, chords are unmodified

### Chores

- rustfmt the split credential fixtures
- drop the repo argument at the unix-only budget-enforcement call sites
- cargo fmt over the review-round fixes
- give the socket-path sweep test a process-unique endpoint name
- bump version to 3.25.0

## v3.24.0 (2026-09-06)

### Features

- remove headless as a spawn topology -- pane, or inline, never invisible
- spawn requests carry supervision ceilings; file drops cannot widen

### Fixes

- review round 1 -- pane deadline safety, snapshot scan floor, requester identity, pane-fulfilled reviewer seat
- T4 supervisors -- transcript growth is progress, the stall nudge is delivered, live capacity text survives, handover acks are not stale, the crash witness outlives a failed resume, loop mail is edge-triggered
- unix-only budget test follows the throttled enforce_pane_token_budgets signature
- #354 review -- throttle budget sweeps, scope error acks, stabilise the cursor, and stop clean exits pinning the warning
- automatic rollover and work balancing actually distribute work
- T7 review round -- terminal-status resurrection, baseline-blind gates, frontend scope, cache-hit denominator, review recurrence, degraded deploy tier, cmd-shim argv, doc/version-bump scoping
- memory/task/search/status/context untrusted-content and race fixes
- rot token gate reads real context, not cumulative spend
- fix substitution ordering, fallback context, cd parsing, and 6 more script-runner findings
- close five escape-classifier gaps and deny model-driven permissions compile writes

### Performance

- #354 A1-4 -- the delegation ledger is re-priced only when it changed

### Documentation

- review round 1 -- spawn-request system prompt and timeout clamp, pane deadline fixes, capacity snapshot throttle and requester matching, --force limitation
- agent spawns are never headless -- dispatch rule, spawn-request ceilings and file-drop strip, review-run argv, decision log; fix the dangling Supersedes link
- inline supervision vs pane attachment; headless is a launch mode
- audit release -- script runner, safety, scheduling/rollover, supervisors, memory, rot/adapters, workflow, dash contract changes; decision log, known issues, active work, journal

### Chores

- the drive-relative join escape test is Windows-only
- bump version to 3.24.0
- rustfmt the unix-only budget test
- #354 A2-3 -- the dashboard inspector asserts its subject short id as well as its kind

## v3.23.0 (2026-09-05)

### Features

- #354 phase 5 -- attention notices, error acknowledgement with counts, dashboard inspector
- #354 phase 4 -- one action table, ^A p palette, help as read-only palette, uniform Esc/Enter, first-run tip
- #354 phase 3 -- shared scrollable dialogs, overlay hits, context menu, inspector, real restore
- #354 phase 2 -- attention glyphs from cached status, done-unread acknowledgement, retained ended rows, shared group fold
- #354 phase 1 -- 44-column sidebar contract, group tree, disclosure, pure hit-testing and clickable rows

### Fixes

- #354 unix-only tests follow the ErrorLog and kept-request signatures
- #354 final review -- stale-snapshot overlay hits never synthesize keys, the tip-dismissing Esc is consumed

### Documentation

- #354 Active Work, Work Journal, five Decision Log entries, module map; bump to 3.23.0
- #354 UX audit, real captures, and the approved 200x50 dashboard design

## v3.22.0 (2026-09-05)

### Features

- wire the source-aware action matrix and cadence into real callers
- [screen] config table for the repetition-dominated thresholds
- #311 zirv ctx loop self-paces its cadence when nothing changes
- memory write-cadence signal (issue #272 design item 4)
- #311 add supervise.loop_backoff_ceiling_secs, narrow-only downward
- wire screen_prefix into score.rs's fallback tail screening
- screen.rs round 2 -- truncation finding, role-marker/invisible-unicode/opaque-run/repetition markers, source-aware action matrix
- #294 add zirv ctx measure verb
- #294 add ToolCallRead/ToolCallEdit events for zirv ctx measure
- add a built-in self-check registry (ZCHK-*) run by zirv verify
- journaled reversible writes, promote/rollback, --if-unchanged, session tier

### Fixes

- #295 review round 2 -- per-bank lock on every writer, state-dir lock files, legacy session-dir cleanup
- #272 review round 2 -- apply [screen] thresholds to handoff screening
- #272 review round 1 -- apply [screen] thresholds on every screening surface
- #295 review round 1 -- session gate fallback, if-unchanged lock, rollback conflict check, journal-error propagation, single-record rollback, session-id collision, refresh cleanup

### Documentation

- #295 round-3 review residuals -- legacy session-dir per-key forget and dedupe, timing-based lock test
- batch 7 -- consolidate Active Work, rotate Work Journal, record residuals in Known Issues
- screen.rs round 2 -- markers, action matrix, memory cadence, [screen] config
- #311 zirv ctx loop self-pacing cadence
- #294 measure verb, new events, decision + work journal
- document the built-in self-check registry and its verb anchors
- issue #295 -- memory journal, session tier, rollback/promote, round-trip guard

### Chores

- ignore the shared-memory --if-unchanged lock file under .zirv/memory/
- integrate #311 -- allow-list supervise.loop_backoff_ceiling_secs as narrow-only in ZCHK-FORBIDDEN-WIDENING
- integrate #294 + #295 -- classify memory promote/rollback as mutating, list measure in the doc verb anchor
- run zirv verify --builtin from the branch binary
- bump to 3.22.0 for harness batch 7

## v3.21.0 (2026-09-05)

### Features

- supervise.orchestrator_writes posture (allow/advise/deny, default advise) replaces the unconditional orchestrator write deny
- usage headroom ranks harnesses but never refuses or delays a spawn; initial launches skip the pacing wait
- #358 T6a -- harness pool view in status (text + --json) and the dashboard aggregate row
- #358 T5 -- automatic fenced meta-orchestrator rollover through the handover seams, exhaustion parking, pool decision events
- #358 T4 -- logical orchestrator seat with fencing generation, rollover decision policy, --pin-harness
- #358 T3 -- durable per-provider token reservation ledger wired into delegation admission, settlement and reroute
- #358 T2 -- pure capacity allocator, frozen capacity snapshot, adaptive delegation routing
- #358 T1 -- fallback scheduling config keys (adaptive_delegation, auto_orchestrator_rollover, rollover headroom/cooldown, per-harness limits)

### Fixes

- #358 review round 1 -- fence successors across swaps, close open rollovers before manual handover, move reservations across providers, atomic reserve_within, degraded config denies orchestrator writes, reactive rollover waits for idle

### Documentation

- #358 allocator, reservations, seat + rollover, pool view, write posture, never-refuse spawns; bump to 3.21.0

## v3.20.0 (2026-09-04)

### Features

- wire quota, workflow-gate/verification-failure and dashboard quiescence into the attention model (#349)
- composed session attention model, explain-status and wait verbs (#349)
- completion judge for objective-driven loops -- gates first, cheap-model verdict second, blocked/wait outcomes (#314)
- release-matched operator skill (`zirv --skill`) and generated command schema (`zirv commands --json`) (#355)
- durable task cards with claim/heartbeat/TTL, dependency gating, swarm helper and `agent --task` (#317)
- proof-required worktree prune, untracked archive, dead-owner GC, `zirv ctx worktree` verbs (#319)

### Fixes

- hold the task lock across read-decide-append, refuse impossible card moves, classify `zirv report` as mutating

### Documentation

- batch 6 -- #319 worktree prune, #317 task cards, #314 completion judge, #349 attention model, #355 operator skill + command schema

### Chores

- integrate batch 6 -- classify the new ctx verbs in the command schema and skill; bump to 3.20.0

## v3.19.0 (2026-09-04)

### Fixes

- address release integration review findings
- exempt Claude harness home from repo write guard (#346)
- show the codex usage reading, log observed_at on reroutes, document --force as the reroute opt-out, pick the worker-default substitute model (#335)
- classify only path-bearing output redirections as seat writes and exempt native subagents (#345, #346)
- confirm usage-limit text against the provider's structured reading before any park or reroute (#340)
- make per-tree exclusivity the only default writer gate; max_writers 0 lifts the machine-wide cap (#338)

### Chores

- finalize 3.18.0 release integration

## v3.18.0 (2026-09-04)

### Features

- zirv ctx search, zero-model recall over transcripts, handoffs, work artifacts and mail (#315)
- window attribution, status --breakdown, reclaim-gated compact advisory (#312)
- render the stall banner and restart-chain counts (#310)
- delegation envelopes that may only narrow, enforced in the safety hook (#262)
- progress-based liveness, stalled banner, restart-chain breaker, failure classes (#310)
- zirv ctx snapshot and zirv report bug --snapshot, redacted capped state summary (#320)

### Fixes

- compact_advisory keys are narrow-only, a repo may only quieten the advisory (#312)

### Documentation

- batch 5 -- #310 stall/chain, #315 search, #312 breakdown + advisory, #320 snapshot, #262 envelopes
- trust-boundary row for search.max_output_bytes (#315)
- trust-boundary rows for the #262 worker keys
- trust-boundary rows for the #310 supervise keys

### Chores

- rustfmt score.rs (#312)
- bump version to 3.17.0 for harness batch 5

## v3.17.0 (2026-09-04)

### Features

- add the operator-owned global memory scope (#336)
- deny orchestrator shell writes to the repo; refuse same-harness zirv agent (#328, #334)
- deny repository Edit/Write from an orchestrator seat (#334)
- the orchestrator seat never implements; same-harness delegation is native (#328, #334)
- seat-role env marker, orchestrator block log and status line (#328, #334)

### Fixes

- find git's real subcommand behind global options; exempt only diff-producing pipe upstreams (review round 2)
- refresh codex usage before routing and never hard-block a credits-covered window (#337)
- close the git apply/patch and sibling-checkout gaps; never reroute onto the seat's own harness (review round 1)
- judge an orchestrator write by the target's own repository (review round 1)

## v3.16.0 (2026-09-04)

### Features

- zirv agent --result-schema/--result-kind (issue #318)
- post-edit diagnostics channel for the Stop hook (#308 stage 1)
- two additive loop breakers for the PreToolUse hook (#313)
- state the same-harness routing rule in the claude orchestrator layer and hint when zirv agent targets the running harness (#328)

### Fixes

- anchor the denial breaker's log window on today and yesterday, not the newest files (#313, codex review round 1)
- lock the baseline only when one exists, so untouched repositories gain no lock file (#302)
- the Stop hook's cargo check uses its own target dir under the state dir (#308)
- the identical-command guard reads only the transcript tail (#313)
- hold one per-repo lock across baseline streak updates and prune (#302); point the narrow escape-safe doc comment at the #222 acceptor (#331)

### Documentation

- release 3.16.0 harness batch 4 -- #328 routing rule, #302 baseline lock, #308 diagnostics, #318 result schema

### Other

- release: bump version to 3.16.0

## v3.15.1 (2026-09-04)

### Fixes

- strip the Windows verbatim prefix from worktree and sibling grants

### Chores

- run the grant_path test on every platform so the helper is not dead in Linux test builds

## v3.15.0 (2026-09-03)

### Features

- /add-dir hint for an outside --workdir; zirv test/verify --dry-run are escape-safe (#307)
- session/repo-scoped audit with Read/Edit prompt families from the permission record (#321 #307 #320)
- verify-on-stop nudge names the stale gate command (#309)

### Fixes

- attribute nested subagent transcripts to their project; resolve a relative --repo against the cwd (codex review round 1)
- harden push --mirror/--prune and checkout -f/-B; allow mixed confined-write + read-only escape retries (#327 #321)
- a markdown heading inside a body no longer truncates the stored payload (#326)

### Documentation

- 3.14.0 harness batch 3 -- mail parser, push/checkout hardening, verify-on-stop, scoped permission audit, --dry-run escape and /add-dir hint

### Other

- release: bump version to 3.15.0 (3.14.0 shipped from #330)
- release: bump version to 3.14.0

## v3.14.0 (2026-09-03)

### Features

- sibling checkouts are writable; the subprocess env scrub is operator opt-in (#329)

### Fixes

- harden #329 sibling discovery and forge delete classifier (review round 1)
- permissions propose follows the running harness too (#329)
- fewer permission prompts for cross-repo work (#329)

### Documentation

- #329 prompt-free posture -- sibling grants, scrub opt-in, glab parity, agent default, review-round hardening

## v3.13.0 (2026-09-03)

### Fixes

- prompt-free posture round 2 - only truly harmful commands ask (#306 #316 #321 #307)

## v3.12.0 (2026-09-03)

### Fixes

- worktree ownership travels on the spawn request; guard leaves an unconfirmed dashboard spawn alone
- dot-only and parent-relative clean operands are not concrete paths
- tree claim follows the child pid; reclaim --worktree on every exit path
- concrete-path git clean, worktree reclamation, atomic tree claim, strict --since
- coordinator panes never take the tree writer permit (#267, #264)

### Chores

- plan execution ledger for 3.12.0
- bump version to 3.12.0; workflow artifacts for the second harness batch

### Other

- merge: vault pages for 3.12.0
- merge: track B -- cost ledger, zirv ctx spend, dashboard aggregate row (#264)
- merge: track F -- narrow the built-in VCS classifier to agent-scoped local git work (#306)
- merge: track C -- per-turn latency and TTFT speed signals (#293)
- merge: track E -- prompt-prefix stability harness (#299)
- merge: track A -- writer permits and one writer per tree (#267)
- merge: track D -- zirv context lint (#275)

## v3.11.0 (2026-09-02)

### Features

- durable objective layer with soft token budget and graceful wrap-up (#285)
- iterative v3 distillation with constraints, decisions, blocked and read-vs-modified (#280)
- host-verified working-set manifest and crash-interruption witness (#281)
- reserve a per-child token allocation at admission (#301)
- skip re-verifying an unchanged worktree after a failed attempt (#287)

### Fixes

- loop rolls cycle spend into the objective; resume dry run keeps the crash witness

### Documentation

- add pace.run_budget_tokens to the trust-boundary table (#285)

### Chores

- workflow review state for release 3.11.0
- workflow artifacts for release 3.11.0
- bump version to 3.11.0

### Other

- docs+fix: vault pages for 3.11.0; idempotent in-flight stamp; symlink test setup

## v3.10.0 (2026-09-02)

### Features

- proportionality-first wrapper -- engineering standard v3, meta-harness v15 (3.9.0) (#261)

## v3.9.0 (2026-09-02)

### Other

- release: 3.9.0 - close #268 #282 #283 #286 #298 (top-5 batch) (#304)

## v3.8.0 (2026-09-01)

### Other

- release: 3.8.0 - close #251 #252 #253 #255 (bug batch, two tracks) (#259)

## v3.7.0 (2026-09-01)

### Features

- prompt-free default posture (#222) - v3.6.0 (#258)

## v3.6.0 (2026-09-01)

### Features

- steering-grade parent mail (#249) + --workdir dispatch surfacing (#250) - v3.6.0 (#256)

## v3.5.0 (2026-09-01)

### Other

- release: 3.5.0 - close #222 #230 #235 #236 #241 #242 #243 #244 #245 (two-track issue batch) (#254)

## v3.4.0 (2026-09-01)

### Features

- waiver-aware review evidence (#238) + zirv ctx status --diff (#246) - v3.4.0 (#248)

## v3.3.0 (2026-09-01)

### Features

- steady-state token reductions + Ruflo evaluation (closes #225, #240) - v3.3.0 (#247)

## v3.2.0 (2026-08-31)

### Other

- release: 3.2.0 - workflow adoption detect/nudge/enforce (#223) + steady-state token usage reductions (#225) (#239)

## v3.1.0 (2026-08-31)

### Other

- release: 3.1.0 - bug batch #227 #228 #229 #232 #233 + common.md budget fix (#237)

## v3.0.1 (2026-08-31)

### Fixes

- bug batch - directed mail delivery (#226), safe auto-allow for zirv built-ins (#224) (#234)

## v3.0.0 (2026-08-31)

### Other

- release: 3.0.0 — fix #220 #219 #206 #214 and move scripts to .zirv/commands/ (#212) (#231)

## v2.39.2 (2026-08-31)

### Fixes

- bug batch - codex argv overflow (#213), workflow gate baseline (#215), budget exit-code clobber (#203), adopt-test flake (#218) (#221)

## v2.39.1 (2026-08-30)

### Features

- unify install docs and fix the Linux install story (#210) (#217)

## v2.39.0 (2026-08-30)

### Features

- dash TUI v3 — mock parity, restored rot/usage, workflow-step footer (#209) (#216)

## v2.38.1 (2026-08-30)

### Documentation

- refresh README to the v2.38 feature set (#198 part 2) (#211)

## v2.38.0 (2026-08-30)

### Other

- release: TUI design system and redesign, orchestrator design/lifecycle gates (v2.38.0) (#207)

## v2.37.0 (2026-08-30)

### Other

- release: AI-native SDLC lifecycle (#187) (#200)

## v2.36.0 (2026-08-29)

### Features

- surface fallback policy and headroom in status
- hand over blocked sessions across harnesses
- prevent fallback ping-pong across exhausted harnesses
- steer new delegations across harnesses
- register cross-harness fallback module
- add cross-harness routing selector
- map equivalent model tiers across harnesses
- expose pacing headroom for harness routing
- add trusted cross-harness fallback policy

### Fixes

- correct fallback headroom helper clock
- refuse unverified cross-harness quality guesses
- preserve fallback budget enforceability

### Documentation

- journal cross-harness fallback implementation
- record cross-harness fallback decision
- add cross-harness fallback subsystem overview
- record fallback config trust boundary
- document cross-harness supervisor continuation
- document cross-harness supervisor continuation
- document cross-harness pacing headroom

### Chores

- apply final rustfmt fallback layout
- format collapsed fallback route
- use fallback route request
- satisfy clippy for fallback routing
- simplify fallback routing API for clippy
- apply rustfmt to fallback implementation
- apply rustfmt to fallback implementation
- apply rustfmt to fallback implementation
- apply rustfmt to fallback implementation
- apply rustfmt to fallback implementation
- apply rustfmt to fallback implementation
- apply rustfmt to fallback implementation
- pin blocked-session cross-harness continuation
- isolate legacy limit-park regression from fallback
- cover equivalent cross-harness model tiers
- cover trusted fallback configuration
- sync lockfile version to 2.36.0
- bump version to 2.36.0

## v2.35.0 (2026-08-28)

### Features

- approved-prompt review and safe-list proposals (#178)
- complete reliable delivery envelopes (#177)
- file GitHub issues from the CLI (#176)

### Fixes

- gate glab api like gh api (#178)
- no re-commenting unchanged proposals, add exclusion guidance (#178)
- close three review findings on delivery envelopes (#177)
- keep shipped common context within budget (#176)

### Documentation

- tick final closeout item
- release 2.35.0 documentation pass
- close out 2.35.0 ledger and regenerate managed context
- establish release 2.35.0 progress ledger

### Chores

- repair WIP checkpoint tree

### Other

- wip(mail): checkpoint reliable delivery work (#177)

## v2.34.0 (2026-08-28)

### Features

- per-workflow ReviewRun breakdown and defect-rate in workflow stats

### Fixes

- re-stamp start_time on pid repoint, widen tolerance
- restore panes on the same launch-mode terms they had at spawn (issue #160 review round)
- disambiguate a recycled pid by process start time
- pin restored dashboard panes to the interactive launch mode (issue #160 finding 1)

### Documentation

- state the epoch-boundary caveat concretely (review finding)
- measure real pre/post-epoch token cost from machine logs

### Chores

- run CI only as a PR merge gate; trigger CD directly on the merge push
- bump version to 2.33.2
- bump version to 2.33.1
- regression test for opaque commit messages with path-like text (issue #160 finding 3)
- record the relative-path find-scan ruling (issue #160 finding 2)

## v2.33.0 (2026-08-28)

### Features

- size delegation - native subagents by default, sub-orchestrators for scoped areas (#175)
- bind sub-orchestrators to their work group by lineage (#170)
- persist the role a pane/session was spawned with (#169)
- give codex seats an orchestrator-conventions layer (#167)
- add kill verb, stale-session reaping, dead status (#166)

### Fixes

- stop role gate tests from probing for installed agent binaries
- wait for the fixture to consume its mode before nudging in the park-mail test
- wait for the outgoing child's turn before nudging in the budget-accumulation test
- fix Windows-target clippy dead-code and compile error
- never signal a pid the OS recycled away from a session
- keep a restored pane's role and work group
- never leave a minted work group behind a refused delegation
- export the work-group binding to a headless delegation's child
- claim and close a dash-spawned coordinator's work group
- read a requester's recorded role out of the session registry
- derive a spawn request's requester from its intake channel
- route codex orchestrator layer through native subagents first (#167)
- accumulate worker budget spend across restarts (#169)

### Documentation

- document intake trust boundary and a bounded discard race

### Chores

- bump version to 2.33.0

## v2.32.0 (2026-08-28)

### Features

- surface a self-healed attestation in explain text and hook reason
- allow-list zirv's own state dirs for sandboxed writes, not just mail
- allow compounds whose writes are confined to the session scratchpad
- classify a leading cd <known-root> prefix by the command after it
- self-heal an invalid attestation snapshot instead of ask/deny-everything
- always allow zirv ctx on the sandbox retry path
- allow read-only gh/glab/git/curl/wget/kubectl on sandbox retry
- add --allow-sensitive escape hatch to zirv memory remember --shared
- add --repo to zirv ctx forget, targeting the shared memory bank
- merge the local and repo memory banks in zirv ctx recall, labeling provenance
- refuse credential-shaped shared memory writes unless --allow-sensitive
- add --repo to zirv ctx remember, writing/verifying in the shared bank
- gate new delegated work on quota pressure, never rotation
- spawn-request lineage, delegation depth cap, and worker budgets
- render the work-group tree with per-child token spend in status
- scale rotation thresholds to the model's real context window
- budget heavy OPERATIONS with permits instead of counting idle sessions
- budget heavy OPERATIONS with permits instead of counting idle sessions
- add PromptRole::SubOrchestrator with a trimmed coordination layer
- let an adapter report its model's context window as a capability
- make an active workflow review gate the single review source of truth
- WorkGroup state and zirv ctx group create|status|close

### Fixes

- scan bundled POSIX short options in is_curl_or_wget_get_only
- reject a .. traversal component in target_is_confined
- kubectl get/describe reject secrets, config view/get/describe reject --raw
- confine attached/glued curl output forms, disqualify -K/--config
- require a separator boundary in target_is_confined
- narrow is_zirv_ctx_escape_safe to an explicit non-subprocess verb allow-list
- run the credential guard before truncation on the harvest path too
- cap oversized shared memory bodies like the private bank
- widen the shared memory credential guard to key/tags/paths
- gate the exactly-one-admission spawn test to windows like its shim siblings
- widen the nudge-cap test's second wait past taskkill's real cost
- harden permit slot claims against sweep/crash races
- roll back work-group admission on a post-admit spawn failure
- enforce work-group child_limit and thread work_group_id into completion logs
- resolve the live model into rot's token-gate capabilities
- refuse --max-tool-calls for the codex adapter instead of silently ignoring it
- catch an over-budget final transcript on a clean child exit
- never let a file-dropped spawn request force the hard pace refusal
- refuse a coordinator spawn from unverified request lineage
- track the spawned heavy child's own pid on its permit
- fall back to default permit policy on a config load error
- warn loudly when a heavy command proceeds without a permit
- make heavy-operation permit acquisition atomic across processes
- repo layer may only add heavy_command_patterns, never replace
- drop stale max_heavy_workers reference from merged depth-cap test

### Documentation

- document issue #168's retry-path carve-outs, cd classification, and self-heal
- add issue #168 permission-noise reduction implementation plan
- vault-keeper pass for repo-portable memories (issue #172)
- the truncation-can't-dodge-the-guard claim now holds for harvest too
- document the widened credential guard, --allow-sensitive on zirv memory, and shared-body truncation (issue #172 cross-review)
- scope RememberArgs::repo's gating doc comment to the store path
- document repo-portable memories and its secrets guard (issue #172)
- add repo-portable memories implementation plan (issue #172)
- vault-keeper pass for PR #171 (epic #155 phases 4-6 + review round)
- define the token-cost measurement procedure and record what data exists

### Chores

- scope the sandboxed allowWrite list back to mail-only
- add the issue #168 regression corpus across both modes and both sandbox flags
- regression-lock that read-only find/locate/rg never hard-deny
- extract evaluate_candidate_outcome from evaluate_candidates' fold
- e2e-cover the harvest skip-and-log path with a credential line
- centralize scope-of-bool mapping and scoped-json-entry struct
- cargo fmt the heavy_command_patterns lift
- set release version to 2.32.0

## v2.31.0 (2026-08-27)

### Features

- add zirv ctx usage --sessions with per-session raw token classes
- stamp the canonical-content hash into generated harness files
- record what every delegated worker actually cost
- end the review loop when a round yields no new findings
- record cache classes, sidechain spend and session lineage on telemetry
- record the four raw token classes instead of pre-summing them
- report a truncated canonical context layer instead of losing it silently

### Fixes

- add missing CompileArgs.escape field in merged test
- dedupe proof is byte-exact against a fresh render, not a header claim
- normalize find paths to close root-wide-scan spelling bypasses
- close find root-wide-scan escape bypass via leading flags and substitution
- correct cache-hit ratio formula, include cache-creation in delegation detail
- pin agent_bin in the forged-spawn regression test
- sandbox-escape allowlist and interactive-mode pinning end Claude approval hell (#147)
- stop toml_quoted_string tripping the cmd-shim reparse guard on Windows
- fix codex approval-posture review majors -- network ask-narrowing, spurious deny row, attached -c form
- codex approval-hell fixes -- config-override pinning, writable roots, network knob

### Performance

- skip canonical injection the harness already read natively
- re-review the delta since the last reviewed sha
- compose one deduped memory layer at the cacheable tail (v8)

### Documentation

- document Phase 3's native-content dedupe
- document phase 2 raw token telemetry and delegation lineage
- commit the issue #155 token-cost spec and phased implementation plan
- correct input_tokens semantics comment to the combined meaning
- correct layer-order doc comments to the v8 shape
- tighten canonical common.md inside the 4096-byte budget

### Chores

- bump version to 2.36.0 for the phase 4 release
- bump to 2.34.0 and record the telemetry shape change
- reuse last_model_flag for the delegation record's model field
- satisfy fmt and clippy on the flags_pin_policy attached-form check
- pin package()'s intact-chain delta wiring end-to-end
- bump to 2.33.0 and record the phase 1 prompt-shape change
- rustfmt the truncation decision detail

## v2.30.1 (2026-08-26)

### Fixes

- EPERM-blind pid liveness, dash discovery fallback, send diagnostics (#151)

## v2.30.0 (2026-08-26)

### Features

- heavy-worker budget and permissions compile (#142)

## v2.29.3 (2026-08-26)

### Fixes

- backslash+Enter inserts a newline in all dash composers (#150)

## v2.29.2 (2026-08-26)

### Fixes

- keep codex's -c flag intact and refuse dead dashboard dirs on agent join (#148)

## v2.29.1 (2026-08-26)

### Fixes

- codex exec approval probe, opaque commit messages, stale-snapshot UX (#140)

## v2.29.0 (2026-08-25)

### Features

- transcript-backed permission audit for codex and claude (#138)

## v2.28.1 (2026-08-25)

### Fixes

- split Pane::spawn cwd/repo and harden git-common-dir env (#119)
- accept linked worktrees of the dashboard repo as pane spawns (#119)

### Documentation

- document worktree-sibling pane spawn acceptance (issue #119)

### Chores

- fmt Pane::spawn call sites and allow its 8-arg signature

## v2.28.0 (2026-08-25)

### Features

- add managed profile migration

### Fixes

- publish current branch safely

## v2.27.0 (2026-08-25)

### Fixes

- adapter-aware injection submit, dash newline input, bounded read_until (#118)

## v2.26.1 (2026-08-25)

### Features

- harden cross-platform permission decisions
- attest sandboxed launch guards
- explain a verdict per launch mode and re-scope the dontAsk fall-through
- report the interactive and headless postures honestly per adapter
- use codex's low-noise on-request approval for interactive launches
- apply the SQL classifier in evaluate behind [safety] sql
- add a pure read-only SQL statement classifier
- never prompt for a command zirv has never seen on an interactive launch
- narrow the built-in ask set to genuinely dangerous families

### Fixes

- repair merge-orphaned HandoverRequest initializer and clippy question-mark lint
- fail safe on SQL escapes and dollar-quoting
- read the real verb past kubectl and helm global flags
- remove code-execution primitives from the find-exec allowlist
- detect shells behind wrapper programs in a pipeline
- thread the real headless signal into swap/spawn launches (finding 10)
- use real interactivity for compile/policy launch mode (finding 5)
- recognize this PR's own new pin flags (finding 14)
- close real unprompted-danger bypasses (findings 1-4,6-9,11,12)
- analyze SQL in every executable segment
- address fmt and clippy findings on the permissions split

### Documentation

- document two defense-in-depth residuals (findings 13, 15)
- record v2.27.0 launch guard hardening
- record final permission verification residuals
- document the never-prompt interactive posture and the SQL classifier

### Chores

- bump to 2.26.1
- corpus assertions for the four adversarial re-review bypasses
- bump to 2.26.0
- bump version to 2.26.0
- pin the no-endless-prompts requirement as an acceptance corpus
- thread LaunchMode through the launch seam

## v2.25.2 (2026-08-25)

### Features

- log an omitted report-back and remind workers to send theirs

### Fixes

- revert the deferred submit; keep it where the paste-fold actually bites
- drain the pending submit before anything that can block on stdout
- persist report-back state across restore; drain submits; share predicate
- defer /compact and mail-advisory submission the same way
- make visible pane injection a deferred two-phase submit
- split visible pane injection into two writes with a settle gap

### Documentation

- reflect PR #116's deferred-submit review round
- bump Technology Stack's version fields to 2.25.2
- document v2.25.2's two-phase injection and report-back reminder

### Chores

- bump version to 2.25.2

## v2.25.1 (2026-08-24)

### Fixes

- address PR #113 review findings 1-6
- exempt the --help capability probe in fake-codex-agent.sh
- reinstate the DASH_REQUESTS_ENV scrub and bound wrap's child.wait()
- apply the drain_to_eof fix and make exec's nudge-family give-ups loud
- close the tap-vs-child-exit race with a bounded EOF drain
- isolate real-$HOME reads and make the dash-requests guard injectable

### Performance

- isolate the load-sensitive exec nudge-restart family under nextest
- use finish_shutdown for dash test teardown instead of shutdown
- zero the pacing blind-delay in agent.rs and agent_command.rs tests

### Documentation

- document PR #113's nextest switch and supervise final-drain fix
- pin the real root cause of a_post_nudge_park's residual flake
- record two flaky-test diagnoses in-repo instead of only in the review report

### Chores

- bump version to 2.25.1
- flatten context_cli::repo()'s return tuple
- use VarGuard consistently in agent_command.rs
- run nextest with --no-fail-fast and pin its version
- switch to cargo-nextest as the fast default test loop

## v2.25.0 (2026-08-24)

### Features

- add guided first-run setup wizard for zirv and zirv chat

### Fixes

- address round-2 re-review findings (N1-N3)
- address review round 1 on the first-run wizard
- drop needless return flagged by clippy on macos

### Documentation

- refresh version references after pre-push hook flag
- document the guided first-run setup wizard (v2.25.0)

## v2.24.0 (2026-08-24)

### Features

- migrate the repo's agent context to zirv-managed layers
- make the shipped dontAsk posture usable -- whole toolchain families, harness dirs, scratchpad, WebFetch

### Fixes

- isolate the PTY test child from the checkout's own .zirv context layers
- exclude zirv-managed renders from the layer-duplicate analysis
- close argument-reordering and sibling-utility bypasses in the shipped deny list
- stop flagging prose basenames, ~-paths and placeholders as missing paths
- resolve hook binaries with PATHEXT before calling them missing
- match a trailing ' *' pattern against its bare prefix, mirroring claude semantics
- exclude zirv-managed native files from the drift duplicate analysis
- readable keys and sentence-boundary bodies for vault-extracted entries
- sweep mail addressed to a session that no longer exists
- sweep orphaned turn-signal endpoints that no live session owns
- strip the Windows verbatim prefix from user-facing paths
- let a hook "ask" fall through under dontAsk instead of denying pre-approved commands
- allow zirv's own commands and cargo fmt/clippy in the shipped posture

### Documentation

- record the optimize static-check accuracy fixes in Utilities.md (#108-#110)
- extend the journal entry to the review-round fixes (#108-#111); drop a redundant memory entry
- commit the claude-layer test-baseline refresh missed by the case-insensitive add
- refresh the Windows test baseline in the claude context layer and regenerate CLAUDE.md
- record the production-readiness pass and zirv-setup migration (issues #98-#106)
- name zirv and cargo fmt/clippy in the shipped-posture allow summary (#98)

## v2.23.0 (2026-08-23)

### Features

- close issues #83-#95, command safety policy, handover, non-fatal ctx.toml

### Fixes

- set truncate on the fan-out read-marker open (clippy on Linux)

### Chores

- assert the codex reviewer pin as an invariant, not one machine's argv

## v2.22.0 (2026-08-23)

### Fixes

- harness parity, pacing gate coverage, dashboard selection, setup restore

### Chores

- opt PTY tests out of the pacing gate and sandbox posture

## v2.21.0 (2026-08-22)

### Features

- finish artifact routing and usage telemetry
- enforce capabilities and bounded reviews
- add zirv context status budget and usage reporting
- add zirv context sync for CLAUDE.md/AGENTS.md compatibility
- compile one deterministic session context for every launch path
- add zirv memory optimize with staleness and consolidation
- harvest durable shared memory at session end

### Fixes

- complete compiled context across relaunches
- report and inject retrieved memory
- enforce context.max_harness_roster_bytes at compose time
- route resume's launch through the context compiler
- align harvest and optimize with setup bootstrap
- gate the clean-exit harvest on shared_enabled before spawning
- restore N4 -- a harvest must never overwrite an explicit entry

### Documentation

- document completed review artifact and telemetry flows
- restore full subsystem references

### Chores

- wait for restart decision log
- use harvested entries for consolidation
- align acceptance tests with supported adapters
- make fake agent portable on Windows
- remove superseded context compiler
- bump version to 2.21.0
- format context status assertions

## v2.20.0 (2026-08-22)

### Features

- add guided setup and harness migration

### Fixes

- model dashboard prompt fallback per launch

### Chores

- bump version to 2.20.0

## v2.19.0 (2026-08-22)

### Features

- raise autonomous design quality ceiling
- add parity benchmarks and provenance (#77)
- enforce fresh frontend evidence (#76)
- automate render and visual QA evidence (#75)
- add offline quality detector (#74)
- add autonomous frontend phase skills (#73)
- enforce a frontend craft floor (#72)
- select frontend profile automatically (#71)
- add zero-touch design profiles (#70)

### Fixes

- enforce autonomous quality evidence

## v2.18.0 (2026-08-21)

### Documentation

- record the memory/context/workflow rework and merge train

## v2.17.0 (2026-08-21)

### Features

- gate repository-supplied checks, skills and telemetry on operator config
- add provider-neutral development workflows

### Fixes

- a declared scope cannot talk complexity down either
- keep the reviewer relay alive and widen the secret denylist
- fail the repo-input gates closed, and stop the forged summary
- give one repository one state slug
- never let the artifact server hang or wedge the run
- measure risk from the tree, and harden artifacts
- keep review evidence honest and the reviewer read-only
- satisfy Windows target lint
- satisfy strict clippy checks
- keep design approval for high risk
- normalize reserved command dispatch
- bound review inputs and normalize formatting
- require fresh independent review evidence
- harden evidence and trust boundaries
- satisfy cross-platform build checks

### Documentation

- record the gate posture and the verifier round
- match the post-review-fix reality
- document lifecycle commands and architecture

## v2.16.0 (2026-08-21)

### Features

- state zirv's permissions policy once and map it per harness

### Fixes

- remove the looser enforcement predicate, note the sandbox flag conflict
- report only real enforcement in per-harness policy support

### Documentation

- cross-reference policy and canonical context, note the #44 coupling

### Chores

- bump version to 2.16.0 for the policy release

## v2.15.0 (2026-08-21)

### Features

- detect duplicate and contradictory instructions (issue #42)
- add canonical .zirv/context/ instruction layer
- ingest CLAUDE.md/AGENTS.md and harness-native settings

### Fixes

- scope canonical deletion exclusion to cross-surface duplicates
- pick redundancy deletion target by layer semantics
- milestone review fixes for canonical layer + drift detection
- close for_path's fail-open default for unclassifiable paths
- stop nested instruction discovery from starving settings surfaces
- close the repo-to-operator trust promotion in ContextSurface

### Chores

- bump version to 2.15.0 for the context release
- cover drift near-duplicate truncation cap
- introduce generic context-surface model

## v2.14.0 (2026-08-21)

### Features

- add deterministic context-aware retrieval engine
- merge core memory layer with budgeted, labeled injection
- add top-level zirv memory command family
- add extended entry schema and a key-addressed shared store
- introduce shared and private memory scopes

### Fixes

- warn on a disabled `zirv memory list`, document paths' inert state
- suppress shared-key shadowing case-insensitively, drop forgeries
- bump DEFAULT_PROMPT_VERSION to v6 for the memory shape change
- make memory.enabled a master switch, fix status/remember gaps
- drop shared entries shadowing a private key, close the block
- floor retrieval relevance on base score, not final score
- verify_scoped gets key agreement and stamps Verified in place
- refuse symlinked or key-mismatched canonical shared files
- close header-injection gap, scope collisions to canonical file
- keep write_atomic clean under Windows clippy, strengthen its test

### Documentation

- reunite select_memory_within_cap's doc with its function
- fix compose_launch_prompt doc-comment misattribution
- scope retrieval budgets to recall until #44, fix stale docs
- document the zirv memory command family
- document rounds 2-3's read/write contract, rewrap ragged line
- reword remaining seed-scope wording (round-1 finding 5 leftovers)
- fix wording that read as contradicting the shared scope's design
- close trust-boundary table drift and document the shared store
- fix REPO_FORBIDDEN drift and add shared-scope trust warnings
- document MemoryScope and memory.shared_enabled

### Chores

- bump version to 2.14.0 for the memory release
- cover launch-seam core injection under core_max_bytes
- tighten the trust-boundary doc-drift needle to the row shape
- assert every REPO_FORBIDDEN key has a trust-boundary doc row

## v2.13.0 (2026-08-19)

### Features

- drag-select with clipboard copy, shift/alt+enter newline, directed worker mail
- adaptive input poll and a Ctrl+A ? help overlay

### Chores

- untrack .superpowers session scratch and gitignore it

## v2.12.0 (2026-08-19)

### Features

- teach per-delegation model choice and honour a lone pin in a pane
- role-gated worker prompt and a worker-scoped user layer
- live inter-session messaging
- harden injected orchestrator conventions and add per-agent capacity marker
- default delegated workers to a cheap model and add a PreToolUse seat guard

### Fixes

- validate a spawn request's model before it reaches a pane's argv
- resume is an interactive session, not a delegated worker
- the bare wrap verb's seat is its own argv, never chat.model
- commit a mail advisory as announced only when the announcement actually surfaced
- stamp the local-input clock before the injection write
- signal-less pane quiescence measures from the latest of child output and local input
- recognise codex short -m model-flag forms in seat export and worker pinning
- pretool guard fails open on payloads that are not real dispatches
- seat guard follows the effective launch model and codex -m pins the worker model

### Documentation

- correct the pty-test count and record two skip-list traps
- record the A/B evidence behind the exec nudge timeouts
- correct the pty-test count and the two stale layer lists
- put the adapter-layer splice notes on the function they describe
- record the role-gated worker prompt and the test-suite gotchas
- align nudge advisory doc comments with the live-delivery contract
- document worker model defaults, seat guard, capacity marker, and prompt v5

### Chores

- bypass the wedged azure apt mirror for the mingw install
- bound the mingw install and the test job so a wedged mirror fails fast
- route the final full-diff review to a pinned reviewer subagent
- pin the mail-advisory test's phase boundary instead of draining it
- drain the stub's echo of the injected advisory between phases

## v2.11.0 (2026-08-18)

### Features

- per-agent code-review model config injected into the harness roster

### Fixes

- keep the review-roster line honest at the floor tier and case-insensitive seats

### Documentation

- document per-agent review-model config

## v2.10.0 (2026-08-18)

### Features

- show both usage windows, hide readings past their reset

### Fixes

- treat any dropped window slot as staleness in freshest_available_observation
- de-flake status test, tighten window::available, and unstick refresh gates at reset boundaries

### Documentation

- document the two-window usage display and staleness fixes

## v2.9.0 (2026-08-17)

### Features

- exhaustive .zirv/ctx.toml and .settings.toml reference configs with parse test
- session conventions v2 - verify-once and scope discipline, delivered to codex workers too
- per-harness usage in dashboard header, codex-aware status bar
- gate refreshes usage sources and honors use_credits
- active usage-poll fallback behind the passive collector
- pace-to-reset Slow verdict and use_credits gate skip
- codex passive usage collector from session rollouts
- parse codex rollout rate-limit snapshots and RFC 3339 timestamps
- pace config gains soft_percent, poll keys and use_credits

### Fixes

- repo config files are uncomment-to-override so they never clobber operator globals
- bound rollout timestamps, reuse the poll agent, restore the tee hint
- pace gate honors estimator-only pacing, latched slow deadlines, and post-reset readings
- pace.enabled gates the usage verb's active poll; record the silent-poll-failure deviation
- redirect home in the no-subcommand usage test and correct stale poll.rs wiring comments
- make the stale-refresh test assert the fresher scan actually wins
- add future-skew guard, timestamp-based snapshot selection, and update stale comments
- saturate window_minutes conversion against untrusted rollout JSON

### Documentation

- soft_percent is deliberately not repo-forbidden; record keychain/API-key poll inertness
- conventions v2 in Utilities.md prompt-layer table
- usage monitoring, use_credits gating, pace-to-reset throttle
- for_provider doc comment reflects its wired callers
- Task 9 — session conventions v2, verify-once and scope discipline
- codex poller ships best-effort even though unverifiable
- implementation plan for usage monitoring and pace-to-reset throttle
- usage monitoring, use-credits gating, pace-to-reset throttle

### Chores

- install mingw-w64 for the Windows-target clippy pass (ring cross-compile)
- fold the offset branch into the question-mark operator for Linux clippy
- fix two Linux-only failures masked by the Windows failure family
- bump version to 2.9.0
- pin the conventions/mail gate asymmetry under --simple
- merge the WaitUntil/Slow safety-cap match arms

## v2.8.0 (2026-08-16)

### Features

- derived harness roster layer and bounded cross-harness review policy in the orchestrator prompt

### Fixes

- close lifecycle review findings (unix lint, roster fail-safe, restart pid window)
- reap child process trees on every teardown path
- match the meta-harness layer header, not its bare name, in the worker-session guard
- close review findings on the harness roster and sidebar ownership
- scope the panes sidebar to sessions owned by this dashboard
- only release from main's own CI runs

### Documentation

- reflect the 2.8.0 version in Technology Stack
- document the process-tree lifecycle layers and their residuals
- bring Utilities.md's prompt.rs coverage up to the v5 compose shape
- document the harness roster, review policy, and sidebar ownership scoping

### Chores

- require every PR to bump Cargo.toml above its base branch
- bump version to 2.8.0 for the harness roster release

## v2.7.0 (2026-08-16)

### Features

- support codex out of the box, with an honestly degraded surface
- drop usage from the header, keep the rot score
- forward the wheel to the child, and show per-instance stats
- per-provider usage and a cheap per-session rot score
- give panes real scrollback, and instrument the input path
- dashboard roster — quit captures sessions, next launch offers restore
- zirv ctx agent joins the dashboard as a pane when one is running
- dashboard spawn-request channel with capability-token directory
- idle-gated visible nudge and mail injection for attached panes
- mailbox and memory-bank overlays driven by the same code as the CLI verbs
- dashboard sidebar shows every registered session; header carries live stats
- zirv chat opens the dashboard when the terminal can carry it
- dashboard event loop, prefix keys, zoom, quit confirm
- dashboard pure renderers (header, sidebar, grid, overlays)
- dashboard pane -- supervised ConPTY child behind a vt100 screen
- dash and chat config sections, Verb::Dash, adapter model_args
- memory harvest, session surfacing, and coordination docs
- per-session mail, nudge verb, and memory prompt layer
- session registry and memory bank store
- zirv chat entry, TUI chrome, and supervision observability
- ctx chat, agent delegation, mail verbs, and mail delivery
- registry-driven default resolution, mail store, harness prompt layer
- add .zirv/.settings.toml agent enable/disable gate

### Fixes

- coerce the signal handler to a fn pointer before the sighandler_t cast
- make the 23 Linux-only wrap/exec/memory tests pass on CI
- make overlays opaque so a modal cannot hide behind pane output
- enable wheel-only mouse reporting without motion tracking
- render the cursor, fix key encoding, and stop starving the event loop
- kill the whole child tree on Windows and make state writes atomic
- close the help-probe RCE and the case-folded reserved-name bypass
- keep repo/untrusted content off the Windows cmd.exe-reparsed argv (system-prompt file form + headless stdin)
- prevent repo-config command injection via cmd.exe argv reparse (chat.model + shim-arg guard)
- round-7 review — nudge injectability gate, restore spawn-failure writeback, doc
- round-6 review — typing gates injection not display, honest report-back gating, deferred restores
- round-5 review — output-quiet idleness, worker report-back channel, honest empty exits
- round-4 review — stable nudge targets, honest dashboard identity and exits, idle debounce
- chat.model disclosure survives a repo-disabled banner
- mail trust — consume identity, recipient-owned pruning, header sanitization, advisory watermark
- round-3 review — restorable sessions, pane reaping, injection scrubbing, setup-abort cleanup
- round-2 review — model flag placement, live-pane cap, single-injection ticks, atomic spawn files
- close dashboard review findings — terminal restore, argv guard, orchestrator prompt, zoom, focus
- panic-safe test guards, pinned delivery addresses, visible degraded sessions
- close all seventeen coordination review findings
- console safety for nested and supervised sessions
- surface every session exit and harden mail delivery timing
- close review findings on chrome, mail trust, and routing
- close settings-gate review findings
- deliver doc-coverage advisory via documented hook schema

### Documentation

- record the dashboard scrolling, overlay and header round
- record round-9 review fixes across the vault
- cross-OS/terminal acceptance testing guide
- add dash module to the CLAUDE.md module map
- dashboard documentation sweep
- orchestrator model is fable per user decision
- spike GO verdict; fold in prefix-arrows, chat.model, repo config example
- dashboard implementation plan (13 tasks, spike-gated)
- zirv dashboard multiplexer design spec
- point Active Work at PR #20
- update Active Work for zirv-chat handoff
- update Active Work and Work Journal for session handoff
- add Obsidian knowledge vault and Claude Code vault wiring

### Chores

- bump version to 2.7.0 for the meta-harness release

### Other

- spike: vt100 fidelity probe for the dashboard (gate)

## v2.6.0 (2026-08-03)

### Features

- inject a Claude orchestrator prompt, fix the Windows wrap deadlock
- agent steps — run supervised ctx exec sessions from scripts
- script-runner UX (suggestions, create flags, rich --help, shadowing warnings, headless errors, README)

### Fixes

- close a handoff deadlock, a shortcuts hard-error, and two test leaks
- relaunch-proof cooldown, honest probe target, and review follow-ups
- exempt passthrough from the agent gate, close three leaks
- stop round-tripping the prompt through argv
- key the M7 help-probe cache per binary, not a joined string
- harden wrap/exec supervision (advise cooldown, M2, M5, M7, M8)
- optimize/hook adapter routing, report hygiene (M1,M4,M6,N3,N4), codex honesty
- pace breadcrumb, watcher mtime, future-timestamp guard

### Performance

- incremental transcript scoring with persisted checkpoints

### Documentation

- record the full-diff review findings and fix mapping

### Chores

- release 2.6.0

## v2.5.1 (2026-08-02)

### Fixes

- --help exits 0 on every ctx verb
- wrap no-supervise and untrusted commands no longer inject the system prompt

### Chores

- release 2.5.1

## v2.5.0 (2026-08-01)

### Features

- inject a consistent session prompt, with --simple to opt out
- adapter surface for per-run system-prompt injection
- layered session system prompt with a capped repo layer
- the stop hook queues an optimize recommendation on repeated failures
- zirv ctx optimize reports configuration findings with proposed diffs
- gather optimize evidence from transcripts and the decision log
- deterministic redundancy and dead-reference lints
- inventory the configuration surfaces that steer a session
- release 2.5.0 with zirv ctx documentation
- zirv ctx usage report and pacing documentation
- pace supervised runs against usage windows and park on limit hits
- deterministic usage pacing gate with layered data sources
- estimate usage windows by summing transcript token usage
- collect usage windows through a transparent statusline tee
- wrap degrades to pure passthrough on any supervision failure
- wrap restarts the TUI in place with a distilled handoff
- wrap advisory and verified compaction injection with cooldown
- wrap injection preconditions at turn boundary and user idle
- zirv ctx wrap transparent PTY passthrough
- raw mode guard and window size probing
- loop backoff, failure caps and on_failure hook
- zirv ctx loop runs a fresh headless session per cycle
- exec timeout kills and turn-signal accelerated scoring
- zirv ctx exec supervises headless runs with bounded restarts
- supervision primitives and a fake agent fixture
- zirv ctx status report
- zirv ctx resume launches a clean session from the latest handoff
- zirv ctx handoff verb with per-repo storage
- distill handoffs with a fresh cheap model call
- handoff type, markdown round-trip and structural fallback
- prompt, pre-compact and notify hook entrypoints
- non-blocking stop hook that scores and forwards turns
- unix socket turn signals
- zirv ctx score verb
- deterministic rot scoring with token gates and canary case parity
- rot engine signal computation
- codex adapter shell from verified CLI behavior
- claude adapter commands, transcript paths and structural context
- AgentAdapter trait and adapter registry
- normalized event and session types
- state directory resolution and decision log
- layered ctx config with env overrides
- add zirv ctx command family skeleton

### Fixes

- strip the equals-bound form of --append-system-prompt too
- wrap no longer panics when the prompt merge empties the argv
- restrict the judgment/distiller model child's tools
- remove optimize's module-wide dead_code allow
- friction accounting counts exec's kill and stand-down too
- stop optimize's dead-reference lint from false-positiving
- optimize's redundancy diffs actually git apply
- merge a user's own --append-system-prompt instead of dropping it
- stop hook decouples optimize hint from rot advisory
- stop leaking HOME across optimize's serial test suite
- optimize falls back to default config instead of failing
- give the supervisor tests a resolved repo path
- restore HOME after the supervisor tests
- bound the distiller, close the state dir, gate the repo config
- hold the hook exit-0 invariant at the clap layer
- carry the transcript path on turn signals
- exec argv parsing must not panic on real invocations
- scope status test's signal import to unix
- notify hook must not propagate a serialization error
- quote verbatim evidence for codex exec --model flag
- remove redundant references in format! args

### Documentation

- verified system-prompt injection facts for claude and codex
- implementation plan for zirv ctx optimize and consistent-session run
- fold optimize and simple-run into the 2.5.0 release
- approved spec for zirv ctx optimize and consistent-session run (2.6.0)
- describe what the code actually does
- Phase E plan tasks for usage pacing, verified usage-window facts
- spec amendment for usage pacing (Phase E)
- implementation plan for zirv ctx, spec amendment for PreCompact semantics
- design spec for zirv ctx autonomous context management

### Chores

- drop the blanket dead_code allows and lint the windows target
- cover relaunch-failure degradation end to end and tidy quit_child/degradation labels
- drop the now-unneeded dead_code allow in run_loop
- record scrubbed real claude transcript fixture

## v2.4.0 (2026-03-16)

### Other

- Update README with upgrade guide and fix example version
- Bump version to 2.4.0
- Replace unmaintained serde_yml with serde_yaml_ng
- Add Linux support for Homebrew and shell install script

## v2.3.0 (2026-03-16)

### Other

- Bump version to 2.3.0
- Improve exit codes and remove redundant error output
- Add --dry-run flag to preview commands without executing
- Detect unresolved placeholders before execution
- Validate param ordering and detect duplicates
- Add colored output and step progress indicators
- Add output module for colored CLI output
- Replace deprecated serde_yaml with serde_yml

## v2.2.0 (2026-03-16)

### Other

- Add optional parameters and renovate dependencies

## v2.1.0 (2026-02-24)

### Chores

- deduplicate code and improve maintainability

### Other

- Renovate

## v2.0.9 (2025-11-22)

### Other

- Renovate

## v2.0.8 (2025-10-06)

### Other

- Renovate

## v2.0.7 (2025-09-15)

### Other

- Fix windows again

## v2.0.6 (2025-09-15)

### Other

- Renovate
- Make sure multishell works on linux based platforms

## v2.0.5 (2025-09-11)

### Other

- Fix multishell on windows
- Fix multishell on windows

## v2.0.4 (2025-08-29)

### Other

- Update commit command
- Renovate
- Renovate

## v2.0.3 (2025-08-19)

### Other

- Adding slab
- Update packages

## v2.0.2 (2025-07-25)

### Other

- Bump version
- Fix cd when spawning shells

## v2.0.1 (2025-07-23)

### Other

- Fixing cd not working

## v2.0.0 (2025-07-22)

### Other

- Renovate packages
- Bump version
- Reworking concurrency to now spawn shells instead
- wip
- Making spawn shell work
- wip

## v1.0.1 (2025-07-12)

### Other

- Update packages and version
- Adding root folder to  help
- Adding download command for linux

## v1.0.0 (2025-06-11)

### Other

- Updating docs and updating modules
- Adding fallback test commands
- Adding fallback once again
- adding tests
- Adding refactore and concurrency

## v0.7.4 (2025-05-16)

### Other

- Renovating crates

## v0.7.3 (2025-05-03)

### Other

- Adding test scripts and updating run function to allow scripts with no options

## v0.7.2 (2025-05-02)

### Other

- Bump version to reflect bug fixes
- Adding build options
- Adding build options

## v0.7.1 (2025-05-02)

### Other

- "help"

## v0.7.0 (2025-05-02)

### Other

- Adding swisstables, capture function and on_failure

## v0.6.5 (2025-05-01)

### Other

- Renovate packages and change edition to 2024
- Update update_chocolatey.ps1
- Update pipeline
- Update script
- Update cd pipeline
- Update homebrew
- .
- Update cd pipeline
- update cd pipeline and update chocolatey script

## v0.6.4 (2025-04-22)

### Other

- Revert
- Revert cd
- Update cd and update script
- Remove @ from readme
- Remove logo
- Adding logo to readme
- Update readme
- Updating chocolatey_update-ps script to rename the executable to zirv.exe and changing nuspec file to list the zirv.exe file instead
- Adding chocolatey to readme as installation option

## v0.6.3 (2025-04-19)

### Other

- Bumping version and updating packages
- Rewrite VERIFICATION.txt for Chocolatey
- Update readme
- Update CD to include intel macs
- Update CD to include intel macs
- Update CD to include intel macs
- Update CD to include intel macs
- Update cd pipeline
- Adding debug
- Adding debug
- Adding debug
- Update homebrew script
- Update homebrew script
- Update homebrew script
- Update homebrew script
- Update homebrew script
- Update homebrew step
- Updating cd pipeline
- Updating cd pipeline
- Updating cd pipeline
- Updating cd pipeline
- Updating cd pipeline
- Updating cd pipeline
- Updating cd pipeline
- Updating cd pipeline to require approval before creating homebrew and chocolatey release
- Updating nuspec
- update cd pipeline
- update cd pipeline
- update cd pipeline
- update cd pipeline
- update cd pipeline
- update cd pipeline
- update cd pipeline
- update cd pipeline
- update cd pipeline
- update cd pipeline
- Make script executable
- adding homebrew and chocolatey

## v0.6.2 (2025-03-25)

### Other

- Bump version
- Updating help

## v0.6.1 (2025-03-25)

### Other

- Adding more example data to created file
- Update readme

## v0.6.0 (2025-03-25)

### Other

- Adding create command
- Update readme

## v0.5.0 (2025-03-24)

### Other

- Adding init command
- Fix readme
- Update readme
- Update cd pipeline

## v0.4.1 (2025-03-24)

### Other

- Update cd pipeline
- Update cd pipeline

## v0.4.0 (2025-03-23)

### Other

- Fixing global shortcuts

## v0.3.8 (2025-03-22)

### Other

- Help user create a global .zirv folder

## v0.3.7 (2025-03-22)

### Other

- Fixing help so it new lists global commands

## v0.3.6 (2025-03-22)

### Other

- Fixing verion command

## v0.3.5 (2025-03-22)

### Other

- Adding version to shortcuts

## v0.3.4 (2025-03-22)

### Other

- Fixing buildin commands

## v0.3.3 (2025-03-22)

### Other

- Adding version command

## v0.3.2 (2025-03-22)

### Other

- Adding unittests

## v0.3.1 (2025-03-22)

### Other

- Update readme with datastructures

## v0.3.0 (2025-03-22)

### Other

- Adding support for env vars
- Adding fallback lookup in root
- .
- Updating release job

## v0.2.0 (2025-03-22)

### Other

- Update tag
- Update tag grapper

## v0.1.0 (2025-03-22)

### Other

- Change permission
- .
- Get version from cargo.toml
- Change mycli to zirv
- Update windows build job
- Change mycli to zirv
- Update upload artifact
- Fix cd pipelien
- Fix lint
- Fixing test in pipeline
- Adding CI/CD workflow
- Adding help
- Rename yaml module to run as the cli now suports json and toml
- Updating readme
- Renaming shortcuts.yaml to .shortcuts.yaml
- Adding support for shortcuts
- Adding support for params
- Adding support for params
- remove commit
- Change docs
- remove some docs
- init
- init

