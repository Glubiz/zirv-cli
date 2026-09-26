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
`quality_score` and the reasoning as `quality_reasoning` -- both `null` for
kind=judge/answer runs, which don't get this second judge. The hidden-test
`score` is unaffected either way: this is a second, independent metric, not
a replacement. `regrade.py --rejudge-quality <runs_root> [tasks...]`
recomputes it for existing runs (re-reading each run's `repo/`/`result.txt`)
without re-running any agent.
