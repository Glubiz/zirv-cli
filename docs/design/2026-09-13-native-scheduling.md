# Native scheduling: route identity, capacity dimensions, failure scope and reconciliation (N18)

**Date:** 2026-09-13 · **Issue:** #487 · **Roadmap:** #469

## Context

#455 gave zirv a pure placement model (`allocator.rs`) and a route-health
breaker (`health.rs`). Both were built for harnesses, and a harness is a
convenient animal: it has one name, one vendor, one usage window and one
failing hop. A native route has none of those simplifications.

- Two routes can be the same *account*, so ranking them as two capacities
  invents headroom that nobody has. Two *accounts* can be the same vendor, so
  coupling them refuses work the operator paid for twice. The old lookup keyed
  on the vendor's name and could not tell those cases apart.
- A subscription window is one of at least five things a route runs out of.
  The other four were simply absent, which in a ranking model reads as
  unlimited.
- "The endpoint is down", "this key is wrong", "this account may not use this
  model", "the prompt is too long" and "the model declined" all reached one
  per-harness breaker. Three of them are not evidence about the route at all.
- A harness's usage arrives through a transcript that is read once. A native
  request returns its usage inline and can be presented twice — a retry that
  actually succeeded, a replayed journal, two supervisors on one run.

This step is an extension of #455's model, not a replacement: there is still
exactly one `place`, one breaker and one reservation ledger.

## Decision

### 1. `route.rs` states what `allocator.rs` could only imply

One new pure module, `src/commands/ctx/route.rs`, holding the four things
above as types. `rot.rs` purity: no fs, clock, env or net; every function
takes its inputs and an explicit `now`, so a placement, an exclusion, a
breaker verdict and a reconciliation all replay from the evidence that
produced them.

`allocator::HarnessCapacity` gains a `RouteIdentity` and an optional
`RouteOffer`; `allocator::WorkUnit` gains a `Demand`. A harness row states the
identity the module always implied (`RouteIdentity::harness`: pool = provider,
no offer), so harness placement is unchanged by construction.

### 2. Capacity is looked up by billing pool, not by vendor

`CapacitySnapshot::providers` is keyed by *billing pool*. A harness's pool is
its provider, so every existing lookup resolves exactly as before, while two
native routes on one account resolve to one `ProviderCapacity` — one set of
windows, one `reserved_tokens` — and two accounts at one vendor stay separate.
`plan` reserves against the pool for the same reason, so a second unit placed
on a sibling route sees the balance the first one already drew down.

`pool_siblings` / `endpoint_siblings` make both couplings inspectable, and the
pool view reports any pool or endpoint that more than one route depends on.

### 3. Five dimensions, and an unmeasured one is never free

`Dimension` is `requests-per-minute | tokens-per-minute | concurrent-requests
| subscription-window | spend-ceiling`. `headroom` turns a `Reading` into a
`Headroom` carrying its `Provenance`:

- a fresh reading with a stated limit is `Measured`, at its real value;
- a reading past `pace.collector_max_age_secs` keeps half its value and is
  `Estimated` with the age as the reason — discounted evidence, not nothing;
- a dimension nothing has reported falls back to
  `fallback.unknown_headroom_pct` and is `Estimated`, so `0` still opts it out
  exactly as it already does for a blind harness.

Nothing degrades to 100%. Every route lists all five dimensions even when only
one has been reported, because the unmeasured dimension is the one most likely
to be binding and omitting it *is* the free-capacity illusion.

No new config keys: the two knobs this needs already exist.

### 4. Eligibility is judged before ranking

`route::eligible(offer, demand)` answers "can this route run this task at all"
— policy, then capability, then context room, then authorized billing — and is
called by `place` before any capacity is read, for the requested route and for
every candidate in the order walk. The result is `Exclusion::Ineligible`, with
its own reason, so a route that could never have taken the work never appears
as "outranked by".

Two deliberate strictnesses: an *unstated* context window cannot be shown to
fit, so it does not; and an *unknown* capability is not a yes.

Billing authorization is item 6's mechanism. Moving subscription work onto
metered API credit is a billing decision, so it is a typed refusal naming what
*was* authorized, never a silent reroute. An empty authorization set states no
constraint, which is every pre-N18 caller.

### 5. Failure scope is a typed mapping, not a class check

`route::route_failure(failure, identity)` maps the existing
`provider::adapter::FailureClass` + `FailureScope` onto one of six routings:

| Failure | Routing | Breaker? |
| --- | --- | --- |
| Transport / first-event or idle timeout | `Endpoint`, class `Transport` | yes |
| Overloaded / Provider (5xx, proxy) | `Endpoint`, class `Server` | yes |
| Authentication / Permission / Entitlement | `Credential` | yes, that credential |
| ModelAccess / Configuration | `Model` (on that credential) | yes, that model |
| RateLimited | `RateLimit` on the *pool*, with `Retry-After` | no — capacity |
| ContextOverflow | `Compaction` | no — a compaction signal |
| Cancelled / InvalidToolArguments / InvalidStream | `Ignored` | no |

A failure that names a *narrower* scope than its class implies is believed (an
adapter that knows which hop failed knows better than a status code); one that
names a wider scope is not, because one bad model must never take an endpoint
down.

`health::RouteKey` gains a `RouteScope` (`harness` by default, so every record
on disk still loads and every harness record keeps its unprefixed path).
Endpoint, credential and model are therefore separate breaker records, with
#455's bounded retries, `Retry-After`, cooldowns, half-open trials and
hysteresis unchanged around each of them.

### 6. Reconciliation is keyed by the provider's request id

`route::reconcile` folds one settled request into a `Reconciliation`, exactly
once, keyed by the provider's own request id — the only identifier that
survives a retry, a resumed journal and two supervisors on one run. It splits
`billable_tokens` from `unpriced_tokens` (a subscription window or a local
runtime is real usage with no invoice: reported, not dropped, not inflated)
and keeps `reserved_tokens` beside them so the drift between estimate and
settlement is visible rather than implied.

The native loop calls it once per response, and reports the result in
`NativeFinalStatus.reconciliation`. The report-only cost ledger is untouched
and is still never a spawn gate.

## What is verified

- `route::tests` — one account is one pool and two accounts at one vendor are
  not; an unknown dimension is a labelled conservative bound; a stale reading
  is discounted evidence; the tightest dimension binds whichever one it is;
  each ineligibility names its own reason; unauthorized billing is refused,
  not taken; the five failure classes reach five distinct decisions and only
  three are breaker input; an endpoint outage is scoped to that endpoint
  alone; a failure's own scope may narrow the routing but never widen it; a
  request reconciles exactly once however often it is replayed; the seen-id
  ring is bounded; identical evidence replays to the identical verdict and
  only the clock can change one.
- `allocator::tests` — two routes on one account are one capacity and two
  accounts are not; a placement on one route reserves against the whole pool;
  an endpoint outage couples only the routes on that endpoint; an unauthorized
  route is excluded before ranking with its own typed reason; all five
  dimensions are reported with the unmeasured ones as estimates and the
  binding dimension is always one of the listed ones.
- `health::tests` — a native scope is part of the breaker key, and a harness
  record keeps the path it has always had.
- `runtime::native::tests` — a two-request turn reports two reconciled
  requests and a billable total equal to the usage the loop accumulated.

## What is deferred, and why

- **`fallback::capacity_snapshot` does not yet emit native rows.**
  `route::offers_from_config` is the conversion, tested, and the snapshot
  producer that turns offers into `HarnessCapacity` rows lands with the step
  that gives native routes their per-minute usage readings — until a route can
  report one, a native row's four extra dimensions would all be estimates and
  the row would rank on the subscription window alone, which is what the
  harness path already does. `capacity_snapshot` is on the dashboard's
  once-a-second path, so adding a config load and an inventory build to it is
  a cost to pay once there is something to read.
- **Ranking still reads the windows.** `route_dimensions` / `route_binding`
  are diagnostic: they report every dimension and which one binds, but `place`
  still ranks on `projected_headroom`. Moving the ranking onto the binding
  dimension is a behaviour change that wants real per-minute readings behind
  it, for the same reason.
- **Reservation reconciliation is in-process.** `Reconciliation` lives in the
  loop and is reported in its final status; folding it back into
  `reservation.rs`'s on-disk ledger (settling the reservation with the *actual*
  rather than removing the entry) is the next step, and is why
  `reserved_tokens` is already carried beside the settlement.
- **Billing posture in the loop is `Api`.** The loop holds a pool id, not the
  account config that states a posture, so every native request is reported as
  billable. Over-reporting billable usage is visible to an operator;
  under-reporting is not.
