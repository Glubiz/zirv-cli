# Latency targets: hook, dash, wrap

Reproducible timing for issue #432's three latency-sensitive paths (the
Claude hook process, the dashboard's per-tick UI loop, and `wrap`'s
passthrough overhead over a raw pty), extending the same convention
`docs/benchmarks/native-runtime-baseline.md`, `docs/benchmarks/build-cost.md`,
and `docs/benchmarks/token-cost.md` already establish -- this is one
convention, not a second one.

**Non-negotiable rule, unchanged from the other three documents**: a number
below is either something actually observed on a real machine with the
command shown next to it, or the row says "not yet measured" with the reason
and the command that would produce it. No estimated or "should be roughly"
number belongs here.

## 1. Hook process latency (measured)

`zirv ctx hook pretool` runs synchronously inside Claude's own tool-call
path on every guarded tool use, so its wall time is latency the operator
waits through, not background work.

Reproducible command (`hyperfine` is the standard tool for this and is
recommended when available; it was not installed on the recording machine
below, so the fallback loop that follows was used instead and is less
statistically rigorous -- no percentiles, just a mean over a fixed run
count):

```sh
echo '{"tool_name":"Bash","tool_input":{"command":"ls -la"},"cwd":"/tmp","session_id":"bench-fixture","agent_id":"","agent_type":""}' > fixture.json

# Preferred, if hyperfine is installed:
hyperfine --warmup 3 'zirv ctx hook pretool < fixture.json'

# Fallback used for the recording below:
N=30
start=$(date +%s%N)
for i in $(seq 1 "$N"); do zirv ctx hook pretool < fixture.json > /dev/null; done
end=$(date +%s%N)
echo "avg_ms=$(( (end - start) / 1000000 / N ))"
```

**Recording**: 2026-09-16, macOS (aarch64-apple-darwin), debug build
(`cargo build`, no `--release`) of commit `22bfdda1` (`origin/main`), 30
consecutive invocations of the fallback loop above with a benign `Bash`
fixture (no deny/ask match, the cheapest real path through `pretool_decision`):
**avg 12ms/invocation** (370ms/30 runs). This is a debug binary; a release
build's own timing is not yet measured, and a debug number should not be
read as expected release-build latency.

**Target**: no formal SLO exists yet. As a placeholder until the maintainer
sets one from more data: a regression that pushes the debug-build average
materially above 12ms on the same machine is worth investigating before it
ships, the same advisory posture `build-cost.md`'s CI job uses for binary
size and build time.

## 2. Dash tick and wrap passthrough overhead (not yet measured)

The code has built-in poll/tick interval **constants** -- these are design
choices, not measurements, and are listed here only so a reader does not
confuse one for the other:

| Constant | Value | File |
| --- | --- | --- |
| `INPUT_POLL_IDLE_WAIT` | 50ms | `src/commands/ctx/dash/mod.rs` |
| `KEYLOG_SLOW_TICK` | 100ms (threshold for logging a slow tick, not a budget) | `src/commands/ctx/dash/mod.rs` |
| `SPAWN_REQUEST_POLL` | 250ms | `src/commands/ctx/dash/mod.rs` |
| `PUMP_POLL` | 100ms | `src/commands/ctx/wrap.rs` |

No paired input-to-paint (dash) or input-to-child (wrap) latency number has
been recorded on this machine. `docs/superpowers/notes/2026-09-05-dash-ux-audit.md`
item 6 already specifies the correct method for exactly this measurement --
replay identical PTY fixtures on main and a candidate change, record
input-to-child and input-to-paint p50/p95/max across pane counts and sizes,
and compare against a same-machine baseline -- and that document's own words
are the rule to follow here too: **"Do not present unmeasured latency as a
passed gate."** That protocol has not been executed yet; this section is
the placeholder until it is, not a substitute for it.

## 3. Paired-measurement write-up template

For a headline claim (rot-scoring effectiveness, token savings, a latency
improvement) with no accepted number yet, record it as a dated file under
`docs/benchmarks/` (this directory, no separate `Measurements/` subfolder --
one location for every benchmark, matching the three files already here) in
this order:

1. **Verdict first** -- one sentence: the change helped, hurt, or measured
   near-zero effect, stated plainly even when the answer is disappointing.
2. **Method** -- paired sessions with the feature on and off, same task,
   same machine, same harness version; name the exact command(s) used.
3. **Root cause** -- with data, not speculation, for why the result came
   out the way it did.
4. **Fixes, in priority order** -- if the verdict was negative, what closes
   the gap, ordered by expected impact.

`docs/benchmarks/build-cost.md` §5 and `docs/benchmarks/native-runtime-baseline.md`
§4 are the existing examples of "verdict first, with real numbers" to follow.
