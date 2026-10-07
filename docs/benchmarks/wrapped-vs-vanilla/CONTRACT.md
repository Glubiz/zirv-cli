# zirv-vs-vanilla benchmark: shared contract

Root: `C:\Users\josj\AppData\Local\Temp\claude\D--GitHub-zirv-dynamic-cli\d7a42858-e38f-4e1c-bc29-4f279a7cdf43\scratchpad\bench` (call it `$BENCH`).
Python: `C:\Python311\python.exe` (3.11, stdlib only, NO pytest -- use `unittest`).
Git: `C:\Program Files\Git\cmd\git.exe`.

```
$BENCH/
  CONTRACT.md
  template/                 # git repo, one commit, clean tree. The Python project "ledgerlite". No CLAUDE.md, no .zirv/.
  tasks/<task_id>/
     prompt.txt             # exact user prompt given to the agent (may be multi-line) -- omitted for kind=chain
     kind.txt               # one of: tests | answer | judge | chain
     grade.py               # see grading protocol -- omitted for kind=chain (graded inline by run.py)
     hidden/                # (kind=tests) unittest files copied into <repo>/tests_hidden/ by grade.py
     rubric.md              # (kind=judge) rubric text handed to a blind judge
     prompts/01.txt..NN.txt # (kind=chain) ordered follow-up prompts, one agent session
     hidden/step_NN/*.py    # (kind=chain) per-step hidden tests, same tests_hidden/ convention
     rubric/step_NN.md      # (kind=chain) per-step judge rubric for a step with no hidden tests
     reference/step_NN.patch # (kind=chain) cumulative reference diff through step NN, for fairness proof
  run.py                    # runner
  aggregate.py              # aggregator -> markdown tables + results.csv
  runs/<task_id>__<cond>__r<n>/
     repo/                  # fresh copy of template/ for this run (copy the whole dir incl. .git)
     stdout.json            # the agent's `--output-format json` result object -- kind=chain instead writes
                             # stdout_step_NN.json/result_step_NN.txt/prompt_step_NN.txt per step
     stderr.txt
     result.json            # metrics, see below -- kind=chain adds a "steps": [...] list, see README
```

`kind=chain` is documented in full in README.md's "Long-session chain"
section (task layout, session-continuation mechanism per condition, and the
grading protocol); it is not repeated in full here to avoid the two drifting
apart.

## Grading protocol (`grade.py`)

Invocation: `python grade.py <repo_dir> <result_text_file>` (cwd irrelevant). Prints ONE JSON line to stdout:
`{"score": <float 0..1>, "passed": <int>, "total": <int>, "visible_ok": <bool>, "details": "<short string>"}` and exits 0 even on a score of 0. Never raises.

- kind=tests: copies `hidden/*.py` into `<repo>/tests_hidden/`, runs
  `python -m unittest discover -s tests_hidden -t <repo>` with cwd=<repo>; score = passed/total.
  Also runs the repo's visible suite `python -m unittest discover -s tests -t <repo>` and sets
  `visible_ok` (true iff every visible test passes). Where the task says a test file must not be
  edited, grade.py checks `git diff --name-only HEAD` and forces score 0 with details if it was.
- kind=answer: reads the result text; each required regex in the grader must match
  (case-insensitive); score = matched/required. `visible_ok` = true.
- kind=judge: grade.py is NOT used; the runner calls a blind judge (see run.py). rubric.md holds
  the rubric. grade.py may still exist to compute `visible_ok` (visible suite) -- runner calls it
  if present and merges.

## result.json (written by run.py)

```
{"task": ..., "cond": "vanilla"|"zirv", "rep": n, "model": "sonnet",
 "wall_s": float, "duration_ms": int, "duration_api_ms": int, "num_turns": int,
 "total_cost_usd": float,
 "input_tokens": int, "cache_creation_input_tokens": int, "cache_read_input_tokens": int, "output_tokens": int,
 "cache_creation_ephemeral_1h_input_tokens": int|null, "cache_creation_ephemeral_5m_input_tokens": int|null,
   # the `usage.cache_creation` 1h/5m split from the `-p` JSON result (issue #788's prompt-cache-TTL
   # lever); null when the result carries no `usage.cache_creation` object at all
 "subagents_spawned": int, "permission_denials": int, "is_error": bool, "exit_code": int,
 "zirv_cmds": {"workflow": n, "skill": n, "agent": n, "ctx": n, "other": n},   # Bash tool calls in the transcript starting with `zirv ...`
 "tool_calls": int,                                                            # total tool_use blocks in the transcript
 "claude_version": str|null, "effort": str|null, "effort_counts": {level: n}, "transcripts": [file, ...],
   # `claude --version`; the effort the transcripts show (null when none found); the session
   # transcripts copied to `<run>/transcripts/`. A chain's `duration_api_ms` is a sum of per-step deltas
 "score": float, "passed": int, "total": int, "visible_ok": bool, "details": str,
 "judge_score": float|null, "judge_reasoning": str|null,
 "quality_score": float|null, "quality_reasoning": str|null}   # kind=tests only; see "Work-quality judge" below
```

## Launch shapes (run.py), cwd = `<run>/repo`

vanilla:
`claude -p --output-format json --model <model> --settings {"disableAllHooks":true}` with the prompt on stdin.
(Note: `claude` is `C:\Users\josj\.local\bin\claude.exe`.)

zirv:
`zirv ctx exec --agent claude --prompt <prompt> -- --output-format json --model <model>` (prompt as one argv item; no shell).
stdout is the same JSON object; zirv writes its own lines to stderr only.

Per-run timeout 20 minutes; on timeout kill the process tree, mark `is_error` true, score 0.
Transcript for zirv/claude usage counting: `~/.claude/projects/*/<session_id>.jsonl` where session_id
comes from stdout.json.

## Blind judge (run.py, kind=judge)

`claude -p --output-format json --model sonnet --settings {"disableAllHooks":true} --max-turns 1
 --disallowedTools=Write,Edit,Bash,NotebookEdit,Read,Glob,Grep,Agent,WebFetch,WebSearch`
Prompt on stdin: rubric.md + the task prompt + `git diff HEAD` of the repo (cap 60 KB) + the agent's final
result text. It must answer ONLY a JSON object `{"score": 0-10, "reasoning": "..."}`; run.py parses it
(strip code fences) and stores judge_score/10 as `score`. The judge is never told which condition
produced the output.

## Work-quality judge (run.py, every kind=tests run)

A second, independent blind judge, run in addition to hidden-test grading
for every kind=tests run (not just kind=judge): same launch shape as the
blind judge above but `--model opus` and `quality_rubric.md` (shared across
every task) instead of a per-task rubric.md. Prompt: quality_rubric.md + the
task prompt + `git diff HEAD` of the repo, excluding `tests_hidden/` (cap 80
KB) + the agent's final result text. Same answer contract
(`{"score": 0-10, "reasoning": "..."}`); run.py stores `score/10` as
`quality_score` and the reasoning as `quality_reasoning` (a leading `[zirv]`
marker is stripped from the agent text before every judge call, this one
included) -- both `null` for
kind=judge/answer runs, which don't get this second judge. The hidden-test
`score` is unaffected either way: this is a second, independent metric, not
a replacement. `regrade.py --rejudge-quality <runs_root> [tasks...]`
recomputes it for existing runs (re-reading each run's `repo/`/`result.txt`)
without re-running any agent.

## Autoresearch trial mode (`--trial`/`--out`, issues #800-#805)

`run.py --trial <spec.json> --out <dir> [--cond <cond, default zirv-proxy>]`
runs exactly the one `(task, rep)` the spec names -- reusing the SAME
`do_one_run`/`do_one_chain_run` pipeline the grid above uses (proxy call,
workflow start, judges, chain stepping), not a second implementation -- into
`<dir>` instead of the grid's own `<bench_root>/runs/<task>__<cond>__r<rep>`
naming. `--tasks/--conds/--reps/--model` and `--trial/--out` are mutually
exclusive CLI modes; every grid invocation documented earlier in this file
is unaffected by this addition.

`spec.json`'s fields the trial reads: `task` (a task id from `corpus.toml`),
`rep`, `route.model` (falls back to `"sonnet"` if absent), `env` (a
candidate's overlay, merged LAST over the condition's own env --
`merge_spec_env`, so an overlay key always wins, including a `null` value
removing that variable), `timeout_secs`, `zirv_dir` (prepended to `PATH`,
identical to `--zirv-dir`), `state_dir` (sets `ZIRV_CTX_STATE_DIR` on this
process, inherited by every subprocess it launches, including judge calls),
and `strategy` (only `{"kind": "escalate", "to_model": ...}` is implemented;
absent means the plain single/chain path).

**Escalate strategy** (issue #804, non-chain tasks only): attempt 1 runs on
`route.model`; if it errors, OR the repo's VISIBLE test suite (`tests/`,
never a hidden one) has any failure beyond the template's one known
baseline (`test_rules.py::test_regex_rule_case_insensitive`), attempt 2
reruns in the SAME repo (no fresh template copy) on `strategy.to_model`
with a fixed continuation prompt naming what's still wrong. Both attempts
are agent receipts; `result.json`'s (and the trial's) cost includes the
full failed first attempt. `escalated`/`escalate_reason` (`"error"` or
`"visible_regression"`) land in `result.json` when it fires.

**Leakage fix** (issue #801): `tests_hidden/` is removed from the trial
repo immediately after grading, for every kind -- previously a `tests`/
`answer` task's own per-task `grade.py` copied hidden tests in to run them
but left the directory behind for the rest of the run (a chain task's
`grade_step_tests` already cleaned up between steps; single-shot tasks did
not, until now).

**Receipts** (`<out>/receipts.jsonl`, one JSON object per line, CONTRACT's
`SpendReport` receipt shape): one `"agent"` receipt per agent invocation
(one per chain step, one or two for an escalated trial), and one `"judge"`
receipt per judge/quality-judge call, INCLUDING a retried call (a judge
reply with no usable score is retried once; both attempts get a receipt so
a retry's cost is never dropped -- previously not captured at all).
`cumulative` is always `false`: a resumed chain step's raw `total_cost_usd`
is the WHOLE session's cost so far (verified from this file's own
pre-existing chain-stepping code -- `do_one_chain_run`'s comment: "`claude
--resume` reports total_cost_usd for the whole session so far, so a step's
own cost is the delta from the previous step"), and run.py already
subtracts the previous step's total before a receipt is ever built, so the
number reaching `receipts.jsonl` is already an increment, not a running
total.

**Spend**: `zirv workflow spend --state-dir <state_dir> --receipts
<out>/receipts.jsonl --campaign <c> --trial <t> --json` is tried first (the
one reconciler, folding in whatever intake/Jev/delegation spend the state
dir's own records hold); if that command is missing or fails, a fallback
`SpendReport` is built from `receipts.jsonl` alone, `completeness:
"partial"`, and `execution.unknown_count >= 1` ALWAYS (this process only
ever sees its own agent+judge calls, never intake/Jev spend, so "the full
execution cost is known" is never a claim the fallback makes).

**trial.json** score mapping: `tests` -> `(score, quality_score)`
(`quality_score` already 0..1); `answer` -> `(score, null)`; `judge` ->
`(judge_score / 10, null)` (`judge_score` is stored 0..10 in `result.json`,
unlike `quality_score`); `chain` -> `(the mean-of-steps score already in
result.json's "score", the one end-of-chain quality-judge score)` -- a
chain's work-quality judge runs ONCE over the whole session, never per
step, so there is no per-step quality figure to average.

**`--check-graders [--tasks t1,t2,...]`** (issue #801, no provider call):
for every task shipping a reference solution -- `reference.patch` for a
non-chain `tests`-kind task (t17-t22 today; t13 and t15 have none checked in),
`reference/step_NN.patch` per `tests`-kind step of a chain task (t23, t24,
t24b, t25) -- applies it to a pristine template copy and grades it (must
score full marks), then grades an UNCHANGED pristine copy (must score below
full). Prints a table and exits 1 on any failure; never weakens a grader to
make this pass. `answer`/`judge`-kind tasks have no reference patch to
check this way and are skipped, not failed.

## Task corpus and splits (`corpus.toml`, issue #801)

Every directory under `tasks/` appears exactly once in `corpus.toml`'s
`[[task]]` list, with `family` (`"ledgerlite"` today -- reports flag
`single_family` per the design spec's non-goals), `class` (`mechanical|
bounded|bug|feature|architecture|ambiguous|sensitive|long_session|
orchestration`), `split` (`dev|validation|holdout|orch`), `kind` (mirrors each
task's own `kind.txt`) and `lane` (below). Loaded/validated by `run.py`'s
`load_corpus_toml`/`validate_corpus` (stdlib `tomllib`), which also rejects an
unknown lane or a lane/kind mismatch.

**Lanes** say what a task is for:

- `long` -- `kind=chain` long-session tasks, the zirv-vs-vanilla headline:
  `t24_long_haul`, `t24b_long_haul`, `t26_household`, `t27_tax_season`.
- `orch` -- `kind=orch`, run by `orch.py` (interactive session through a ConPTY),
  never by `run.py` (`--tasks all` skips them): `o01_ledger_suite`.
- `jev` -- the short tasks `t13`, `t15`, `t17`-`t22`, used ONLY for measuring the Jev
  proxy (`zirv-jev-full` / `zirv-proxy` vs vanilla + superpowers), never in the
  zirv-vs-vanilla headline.
- `autoresearch` -- `t23_afternoon`, `t25_sticky_notes`: chains the autoresearch
  campaigns use, too short to count as `long` tasks.

The short tasks t01-t12, t14 and t16 were removed from the benchmark (archived
results under `results/` are untouched).

Splits are assigned by TASK GROUP, not individually, so near-duplicates
never straddle a split boundary: `t24_long_haul` and `t24b_long_haul` are
byte-identical except for two step prompts (see `tasks/README.md`), so
using one for iterative screening and the other as the "unseen" holdout
would leak almost the whole task into candidate selection -- both sit in
`holdout` together (both stay in the `long` lane). With the short tasks gone the
splits are: `dev` = t13, t15, t23 (the cheap screening set); `validation` = t17,
t18, t19, t25, t26; `holdout` = t20, t21, t22, t24, t24b, t27 (its own group); each still carries one
`long_session` chain task (t23, t25/t26, t24/t24b/t27) so a manifest's `[stages.*]
classes = ["long_session"]` filter has something on every stage, and the
non-chain stages keep `feature`/`architecture`/`ambiguous` tasks. Change from the
earlier rule: `dev` was 13 tasks (t01-t12, t23) and is now 3, and the former
`mechanical`/`bounded`/`bug` classes no longer have a task (the classes stay valid).

The `orch` split holds the orchestration task alone: it recombines the hidden
suites of tasks that already sit in validation/holdout, so it must not be
screened or held out beside them.

The protected evaluator set a campaign hash-pins at start and re-verifies
before every trial and before promotion (design spec #801) is, for this
harness: `run.py`, `corpus.toml`, every task's `grade.py`/`hidden/`/
`rubric*`/`reference*`, and `quality_rubric.md` -- a manifest's own
`[evaluator].protected` list names these paths for the campaign runner;
this file and `run.py`'s own leakage fix are what keep a live trial repo
from ever holding a hidden test past its own grading.
