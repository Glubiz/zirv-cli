# Native execution enforcement

**Date:** 2026-09-11 · **Issue:** #473 · **Roadmap:** #469 N04

## Outcome

`src/commands/ctx/runtime/enforcement.rs` is the mandatory authorization
boundary for every native effect. Provider responses remain untrusted data;
N05 and later tool registries turn them into typed `ExecutionAction` values,
then execute only after the broker returns an `Authorization`.

The broker reuses the existing authorities instead of creating native-only
copies:

- `CtxConfig::{policy,safety}` supplies the canonical narrowing-only policy
  and command classifier. `ConfigPolicySource` reloads both at every effect.
- `seat.rs` supplies session, role, runtime and generation identity.
- `permit.rs` supplies the RAII writer permit. The permit now exposes only
  the worktree it covers, letting the broker verify exact ownership without
  exposing or duplicating permit state.
- `provider::credential` remains the only provider-secret resolver. Provider
  credentials are never placed in a tool environment.

## Authorization order

Every call fails closed in this order:

1. Verify the persisted seat still names this native session, role and exact
   generation, and is not parked.
2. Reload policy; any unreadable or unparsable policy layer refuses effects.
3. Resolve action paths through existing symlinks or junctions. Missing leaf
   components are appended only after the nearest existing ancestor is
   canonicalized.
4. Enforce read/write/artifact/network claims. Protected credential, state
   and git-administration roots take precedence over ordinary file tools.
5. Require a live writer permit covering the exact linked worktree for every
   declared repository or git-metadata mutation.
6. Apply the canonical capability stances and, for processes, the existing
   `SafetyPolicy` classifier.
7. If required, verify an operator approval signed by a process-local
   authority. The signature covers approver, issue/expiry time and an exact
   scope digest.
8. For a process, require a verified platform launcher and return the
   immutable sandbox policy consumed by N05. Missing containment is a typed
   `IsolationUnavailable` error, never an unsandboxed fallback.

Approval scope includes the full typed action/arguments, canonical resolved
paths, session, task, role, generation, resource-claim fingerprint and current
policy fingerprint. Changing any one invalidates reuse. Headless sessions
cannot satisfy `ask`; parent grants cannot authorize a child because the
child identity produces a different digest.

## Filesystem and git scope

Ordinary file tools read only declared roots and write only the assigned
worktree or explicitly operator-declared outside roots. State, standard
credential directories, provider credential files and git administration are
protected even when they appear below a broader root.

Linked-worktree git access is discovered with `git rev-parse
--path-format=absolute`. Process sandboxes receive write access only to that
worktree's private git directory plus the common object/ref/log stores needed
for normal commits. A main checkout is rejected because its git directory is
also the common directory; granting it wholesale would expose hooks, config
and every linked worktree. Structured file tools never write git metadata.

Authorization returns canonical paths. N05 must operate on those paths and
preserve its own expected-content/open-time checks; it must not reopen the
unresolved model spelling.

## Process and network containment

The platform launcher receives a scrubbed environment, read roots, exact
writable roots, masked credential/state roots and a binary network decision.
Host-scoped network allowlists are enforced by brokered HTTP tools only;
arbitrary processes are refused unless the operator granted unrestricted
process network access. Descendant cleanup remains separate from containment.

| Platform | Mechanism | Current claim |
|---|---|---|
| Linux | bubblewrap (`bwrap`), read-only host bind, exact writable rebinds, protected masks, namespace network isolation | Supported only when an executable `bwrap` is detected; otherwise explicit unavailable |
| macOS | Seatbelt via `/usr/bin/sandbox-exec`, default deny, protected-path denies, exact write allows, optional network allow | Supported only when the system launcher exists; otherwise explicit unavailable |
| Windows | Zirv restricted-token/AppContainer helper contract; Job Objects remain cleanup only | The broker contract is implemented, but native process execution remains unavailable until the Zirv helper is installed and its platform gate passes |

Zirv therefore makes no blanket cross-platform native-coding claim at N04.
Brokered effects are portable; arbitrary process support is a runtime
capability and must be shown as unavailable when its verified mechanism is
missing. N05 owns process lifecycle and the Windows helper implementation;
N22 owns packaging it. Neither may bypass this gate.

## Credential separation

The tool environment removes explicitly configured provider variables and
common token/secret/password/API-key/credential variables, SSH agent and
askpass channels, cloud profiles, and credential-config directory overrides.
Linux bubblewrap clears the environment before re-adding the scrubbed map.
macOS and Windows launchers receive only the scrubbed map. Provider transports
obtain credentials out of band from `provider::credential::resolve`.

## Verification evidence

Inline deterministic tests cover deny/ask/allow behavior, exact writer scope,
outside-root refusal, symlink escape, protected state, credential scrubbing,
stale-generation fencing, policy/action approval invalidation, fail-closed
isolation, Linux and Seatbelt profile construction, and linked-worktree git
scope. CI runs the complete broker suite on Linux, macOS and Windows in the
`Native Enforcement` matrix. The Windows run validates portable broker and
filesystem semantics and the honest unavailable result until the helper is
shipped; it does not mislabel a Job Object as sandbox evidence.

## Successor: interactive approvals for in-process sessions (issue #490, N21)

N04 defined `ApprovalMode::Interactive` but shipped no way to reach it from a
live session: `runtime::native::session_broker` built every native session
`Headless`, so an action that needed approval was refused with
`ApprovalUnavailable` and could not be approved away at all. N21 item B closes
that, without widening anything N04 fenced.

**The contract.**

- `InteractiveApprovals` is the whole of the interactive path: a channel the
  operator's pane drains, a session-scoped set of remembered scope digests,
  and a cancellation flag. `ExecutionBroker::with_interactive_approvals`
  installs it; it MUST share the broker's own `ApprovalAuthority`, which
  `session_broker` is the one place that pairs.
- The approval MODE is **derived from the gate**, not passed beside it:
  `session_broker` takes `Option<Arc<InteractiveApprovals>>` and is
  `Interactive` exactly when one is supplied. A broker cannot claim it will
  ask with nobody listening, nor hold a dialog it never consults.
- When `validate_action` says an action needs approval and no grant was
  supplied, `authorize_at` raises a typed `ApprovalPrompt` (carrying the same
  `ApprovalRequest` whose `scope_digest` a grant is signed against) and
  **blocks the calling tool call** until a decision arrives. The grant it
  mints is verified against the broker's own authority before it admits
  anything, so the dialog can never describe less authority than what runs.
- A decision is applied **exactly once**: `ApprovalPrompt::decide` consumes
  the prompt and the reply channel holds one message.
- Three decisions. `Once` admits this call. `Remember` admits it and
  suppresses the next request with a byte-identical `scope_digest` for the
  rest of this session -- never persisted, never widened to a path or a
  directory, and gone when the session ends. `Deny { guidance }` fails the
  call with `BrokerError::Denied(guidance)`; the pane commits the same
  guidance as steering, so the running loop picks it up between requests.
- Interruption cancels every blocked call: each returns `Cancelled`, the call
  fails closed with `ApprovalRequired`, and a decision arriving afterwards
  finds the waiter gone and releases nothing. `InteractiveSession::interrupt`
  cancels the gate with the turn; `shutdown` closes the channel first, so a
  worker thread is never parked on an answer no dialog will draw.
- A grant minted here expires after `INTERACTIVE_GRANT_TTL_SECS` (300s), and
  `prepare_process` re-checks the expiry, so a stale answer cannot admit a
  later action.

**What is unchanged.** Headless sessions keep today's behaviour exactly -- the
`ApprovalMode::Headless` arm is evaluated before the gate is ever consulted,
so a headless refusal cannot be approved away no matter who installed a
dialog. Repository-owned configuration may only narrow: nothing in
`<repo>/.zirv/` can install a gate, raise the mode, or mint a grant. The
protocol path (`session.approve`) is untouched.

**Verification.** `an_interactive_tool_call_blocks_until_the_dialog_answers_yes`,
`remembering_a_scope_suppresses_the_next_identical_request_but_not_another`,
`denying_an_interactive_approval_fails_the_call_with_the_operators_guidance`,
`an_interrupt_while_blocked_cancels_the_call_and_releases_nothing`,
`a_headless_session_refuses_even_with_a_dialog_installed` and
`a_closed_prompt_channel_fails_the_call_closed`.
