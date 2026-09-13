# Native workers, shared ownership and delegation receipts (N10)

**Date:** 2026-09-13 · **Issue:** #479 · **Roadmap:** #469

## Context

N09 (#478) made a native session real: `zirv ctx exec --runtime native` runs
a whole conversation with no coding harness installed. What it could not do
was participate in a fleet. The roadmap's contract for this step is blunt
about what has to hold:

> A native coordinator may use native or wrapped workers; a wrapped
> coordinator may use native workers. Every exclusive task/resource has one
> owner.

Three closed issues define the contracts a worker has to satisfy to count as
part of that fleet: #452 (durable launch receipts and terminal delivery),
#453 (shared ownership, no duplicated work), #454 (bounded results and
task-addressed continuation). #468 is the live failure that says why delivery
has to be boundary-aware: an advisory that could not be typed into a pane
with a permission dialog open was dropped and never retried.

Before this step, a native session was also structurally unable to write to
a repository at all: `run_headless` passed `None` for the execution broker's
writer lease, so every repository write failed the "a live writer permit for
this exact worktree is required" check. That is the right default for a
session nobody granted a tree to -- and exactly the wrong one for a delegated
worker that was.

## Decision

### One delegation service, two runtimes

`ctx::delegation` is the whole contract, and it is deliberately the only
implementation of it. The CLI verbs and the native tools both call the same
functions; neither grows its own copy.

A delegation has a **stable handle** minted by zirv -- `delegation`, plus an
`attempt` counter. It is not a provider conversation id: those change on
every resume and are not addressable by a parent. The durable record lives at
`<state>/delegations/<repo-slug>/<handle>.json` and carries the launch
receipt, the ownership taken, every attempt, every delivery published, every
delivery consumed, every deferred message, and every `outcome_unknown`
effect.

**Order matters and is the design.** `record_launch` writes before a worker
starts; `publish_terminal` writes the outcome before it mails anything. A
crash after persistence and before delivery leaves an undelivered outcome a
later call republishes. The opposite order would lose it, and no amount of
transport reliability would fix that.

### Delivery identity, not exactly-once transport

Mail is at-least-once and is not going to stop being at-least-once. Every
terminal publication carries `<handle>:<attempt>:<revision>`.
`consume_delivery` returns `true` once per identity and `false` forever
after, so a duplicated transport delivery is idempotent at the consumer while
a genuinely different outcome -- a later attempt, or a corrected revision of
the same attempt -- still reads as new. A consuming `zirv ctx inbox` applies
this, which is what makes an *unchanged* legacy orchestrator safe against
duplicate delivery without learning a new verb.

The dedup fails **open**: an identity naming no record here is shown. Hiding
a message nobody can account for would turn a bookkeeping gap into lost mail,
which is the failure this exists to prevent.

### Ownership is the existing mechanisms, not new ones

Criterion (b) -- a native and a legacy worker must never both hold one
exclusive claim -- is met by making the native path take the *same* claims,
not by inventing a native registry that would have to be kept in sync:

- **Task**: `task::claim_locked`, taken once by `agent::run_with` before the
  runtime fork, so both forks are already behind one claim.
- **Checkout**: `permit::acquire_writer`'s per-tree claim. The native fork
  then *moves the permit into the execution broker*, which already refuses
  any repository write not covered by a lease for that exact worktree. The
  exclusion is therefore enforced twice, at admission and at effect time, and
  the native worker gains the ability to write at all.
- **Provider tokens**: `reservation::reserve_within`, settled from the run's
  real usage. Reserving needs the provider before the route resolves inside
  the loop, so `native::route_provider` answers it from operator
  configuration alone -- no credential store, no network. A missing
  credential still fails at the request, where it should.

### The runtime fork sits after everything shared

`agent::run_with` resolves `--runtime` first, then continues exactly as
before through flag validation, `--workdir`/`--worktree` allocation, prompt
assembly (`--attach-artifact`, `--result-schema`, `--task`), envelope
resolution and the task claim. Only then does it fork. Everything below the
fork in the harness path -- adapter selection, cross-harness rerouting, the
spawn gate, the dashboard pane join -- assumes a vendor CLI exists, and a
native worker has none.

Under `--runtime native` the positional `<name>` is re-read as the provider
**route**, with the reserved value `native` deferring to the `[roles]` entry
for `--role`. A native worker has no harness to name, and silently ignoring
the argument would be worse than repurposing it. `--route` overrides.
`--max-restarts` and a trailing `-- <flags>` passthrough are refused rather
than ignored, the same rule `exec::run_native` already applies.

### Seven typed tools, zero new logic

`delegate`, `send`, `wait`, `result`, `follow_up`, `interrupt`, `close` join
the native registry (16 tools to 23). Each is a validated argument shape in
front of the corresponding service method. Notable choices:

- Every one crosses the broker as `ExecutionAction::Delegate`, so a native
  session cannot delegate around the seat generation fence and policy its
  other tools run behind.
- The handle is validated as `[A-Za-z0-9_-]{1,128}` at the argument boundary
  *and* in the store, so provider output can never name a file outside its
  own repository's delegation directory.
- `delegate` is `RetryPolicy::NeverAfterStart`. A worker that may already be
  running must not be re-dispatched by a blind retry; that is how one task
  gets paid for twice.
- `delegate`'s production launcher is one `agent::run_with` call -- literally
  the function `zirv agent` runs -- behind a `WorkerLauncher` seam that
  exists so tests can drive the whole tool surface without starting anything.

### Continuation is addressed, never guessed

`follow_up` resolves deterministically from the record: directed mail while
the worker is live; a journal resume for a finished **native** worker
(appending a continuation attempt, never overwriting the prior outcome); and
otherwise an explicit replacement checkpoint that states it has none of the
original's hidden context. An unknown handle is an error. There is no
"most recent session" fallback anywhere on this path, because one would
silently answer a follow-up with the wrong worker.

### Boundary-aware delivery

The #468 predicate moved out of `dash` into `attention::blocking` /
`attention::block_reason`. The dashboard pane sweep and `delegation::send`
now answer "is this a safe boundary to deliver at" from one rule. A blocked
message is queued **durably** on the record and retried by `drain_queued`;
`zirv ctx inbox` drains at every checkpoint. Both the deferral and the later
delivery log a decision row carrying the same message id, so a missed message
is diagnosable from `logs/decisions.jsonl` alone.

## What is verified

Deterministic, fixture-backed, no provider call and no spawned worker:

- launch receipt durable before anything runs; ownership recorded and
  released by `close` while receipts and `outcome_unknown` effects survive;
- publish-once/replay-as-duplicate; a later revision is a new delivery; a
  crash between persistence and delivery republishes the same outcome without
  inventing a second one; duplicate consumption is idempotent;
- an approval-open target queues, stays queued while the latch is open, and
  is delivered exactly once at the next boundary;
- follow-up addresses the original delegation (resume for native, checkpoint
  for a finished legacy worker), and an unknown handle errors;
- both halves of the exclusive-claim rule -- writer permit and task card --
  refuse the second runtime, in either order;
- the seven tools are registered with closed schemas, the delegation resource
  claim and the right retry policies, and drive the service end to end for
  both runtimes through a real `NativeToolClient` (real registry, real
  broker, real seat fence);
- an unchanged `zirv ctx inbox` reads a native worker's outcome plus its
  bounded evidence reference and drops the duplicate copy.

## What is deferred

- **A native worker in a dashboard pane.** A pane hosts a harness TUI; a
  native session has no terminal UI until N11 (#480). `--runtime native`
  therefore always runs inline and never joins a dashboard.
- **A bounded structural-result resume retry for native workers.** The
  harness fork's one-shot `headless_resume_cmd` retry has no native analogue
  yet; a native contract failure is a `follow_up` against the durable handle
  instead.
- **Overlap advisories (#453's inferred-duplicate half).** Exact ownership is
  enforced; "these two tasks look similar" is not attempted here.
- **Spend accounting parity.** The reservation is settled from the run's
  output tokens; the full per-segment cost ledger a harness delegation writes
  is N18 (#488) work.
- **Live provider coverage.** Every test here is fixture- or
  service-level; no route was exercised against a real endpoint.
