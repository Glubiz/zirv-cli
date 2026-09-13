# Persistent runtime: server-owned PTYs with detach, reattach and crash-resilient clients (issue #352)

**Date:** 2026-09-13 · **Issue:** #352 (builds on protocol v1 #353; prerequisite of #489, step N20 of the native-runtime roadmap #469) · **Status:** implemented behind an experimental operator-only gate

## 1. Context

Until now zirv had no daemon, deliberately: the dashboard owned the
pty/ConPTY pairs, and quitting it ended every session it held, writing a
roster so the operator could relaunch them later. That is a correct design
for a program whose sessions are cheap. It is the wrong one for sessions
that carry an hour of context, a workflow gate and a spend budget: closing a
window, losing an SSH connection or crashing a TUI should not cost any of
that.

Protocol v1 (#353) deliberately shipped without a daemon and with PTY
ownership still in the dashboard, and added five attachment methods
(`session.attach|detach|takeover|resize|screen`) plus the
`session.attach` capability that gates them, so that the daemon could arrive
without a second, private wire. This change is that daemon.

## 2. Decision

A local runtime service — `zirv session serve` — owns the terminals, and
every UI is a client of it over protocol v1.

```
zirv session serve     the service: PTYs, supervisors, registry records, topology
zirv session list      what it holds
zirv session attach    this terminal becomes a client (Ctrl+A d detaches)
zirv session detach    release clients; the agent is untouched
zirv session stop      the one verb that ends a process (or the service)
zirv chat              attaches to / creates this repo's runtime session
zirv chat --no-session the previous behaviour, unchanged
```

### 2.1 The service owns what the dashboard owned — including the registry record

`session::host::RuntimeHost` is deliberately assembled from the primitives
`dash::pane::Pane::spawn` and `wrap` already use: `portable_pty`,
`sessions::scrub_supervision_env`, `wrap::answer_inherit_cursor_probe`,
`supervise::ChildGuard`, `adapters::guard_cmd_shim_reparse`,
`wrap::quit_child`, `priority::apply_to_child`, `signal::SignalServer`,
`wrap::publish_socket_path` and `sessions::SessionGuard`.

That last one is the load-bearing piece for issue #352's policy criterion.
Pacing, budgets, rot scoring, mail addressing, writer permits, `zirv ctx
status` and workflow policy all read the **session registry**, not the
dashboard. A runtime-owned session files exactly the record a pane does —
with the child's own pid and start time — and the SERVICE holds the guard.
So detaching every client changes nothing any of those subsystems can see,
and the harness's turn signals keep being drained by the service's own pump
loop whether or not anybody is watching.

### 2.2 Launch composition stays where it already is

`session.start` routes to the host when one is attached, and the host turns
the spec into a command line by calling the existing chat launch path
(`chat::resolve_adapter` → `chat::build_launch` →
`chat::dash_orchestrator_pane` → `dash::build_turn_env`). Two consequences,
both intended:

- there is no second place where context compilation, prompt injection, the
  sandbox posture and the conversation pin can drift; and
- **a client never hands the runtime an argv.** The endpoint is owner-only,
  but "owner-only" is not a reason to accept an arbitrary command line over a
  socket when every launch a runtime needs to make is an adapter's own.

The runtime opens orchestrator seats. Worker panes carry a task prompt, a
work group, a budget and a report address that `dash::fulfill_spawn_request`
assembles; hosting those is #489 (N20), and `launch_spec` refuses a
non-orchestrator role by name rather than silently opening something else.

### 2.3 Identity: start identity, not pid

`<state>/runtime/<name>.json` carries owner (pid, start time, user),
version, protocol, endpoint, creation time, last-client time, instance id and
whether history is on. `namespace::classify` is a pure function over an
injected probe:

| probe result | verdict | may a new service claim it? |
|---|---|---|
| pid dead | `Gone` | yes |
| pid alive, start time differs beyond 300 s | `Recycled` | yes |
| pid alive, start time matches | `Live` | no |
| pid alive, either side has no start time, heartbeat quiet | `Unverified` | no |

A pid-only check gets the second row wrong, and getting it wrong means a
crashed runtime blocks its own successor forever the moment some unrelated
program inherits its number. `Unverified` is its own answer rather than
folded into either neighbour: claiming "live" or "gone" without evidence is
inventing it, and the safe side of that particular coin is refusing to seize
a namespace.

Every service start mints a fresh `instance` uuid (never derived from pid,
name or endpoint), and every restored session gets a **new** session id that
merely records its predecessor. A crashed service's session identities can
therefore never be republished by its successor.

### 2.4 Persistence tiers, stated honestly

| Tier | Event | What actually happens |
|---|---|---|
| 1 | A client detaches, crashes or closes | The original PTY and process keep running; reattaching repaints from the runtime's own live `vt100::Parser`. Nothing is relaunched — proved by the pid being unchanged across the detach (`detaching_leaves_the_process_and_its_screen_alive_for_the_next_client`). |
| 2 | The runtime restarts | The processes are gone. Topology (sessions, agents, roles, cwd, rows/cols) is restored from `<state>/runtime/<name>-topology.json`, and a session is RESUMED only when it carries a verified harness conversation reference, via the adapter's own `resume_args`. Everything else is reported as not resumed, by name. |
| 3 | Rendered terminal history across a runtime restart | `[session] history`, **off by default**, with an unconditional warning at every start that names what it writes (API keys, tokens, file contents). Tier 1 does not need it: the screen never left memory. |
| 4 | Replacing the runtime binary under live sessions | Not supported, not attempted, not implied. Stop, upgrade, start. |

"Restore topology" is never described as "the process survived". That
distinction is a predicate (`TopologyEntry::is_resumable`) and a partition
(`partition_resumable`), not a convention, so it is testable and cannot be
softened by a hopeful log line.

### 2.5 Detach is not stop

Two verbs, two behaviours, and the difference is enforced in three places:
`session.detach` on the server only ever moves entries in `clients` /
`controller`; `RuntimeHost::stop` is the only function that reaches the
termination ladder and the only one that releases the registry guard; and the
service's own shutdown drains topology and leaves every session running
unless the operator passed `--stop-sessions`.

`stop` confirms (and refuses without `--yes` when stdin is not a terminal);
`detach` never confirms, because it cannot lose work.

### 2.6 Many observers, one controller

Attaching as a controller when somebody else holds the seat is refused with
`busy` — never a silent displacement — and `session.takeover` is the explicit
way to take it, announced to every client as one `controller_changed` event
per real change. Observers may not type and may not resize somebody else's
terminal; they render a clipped view instead.

### 2.7 Shutdown signalling

The serve loop watches for `<state>/runtime/<name>.shutdown`, which
`zirv session stop --runtime` writes. A file rather than a new protocol
method: v1's method table is frozen and fixture-pinned, and inventing a
private `server.shutdown` frame for one CLI verb is exactly the "split the
daemon through private messages" shape issue #352 rules out. Anyone who can
write that file into the owner-only state directory could already connect to
the endpoint and stop every session individually.

## 3. What is verified

Tests are named in `session::{host,service,client,mod}` and in `chat`:

- **Tier 1, over a real terminal** (ConPTY on Windows, a unix pty elsewhere):
  `detaching_leaves_the_process_and_its_screen_alive_for_the_next_client` —
  the child's registry pid is alive before the detach and the same pid is
  alive after it, the registry record survives, and the reattached screen
  still carries the marker the child printed. A relaunch would change the pid.
- **Seats:** `many_observers_may_watch_but_only_one_client_holds_the_keyboard`
  — three observers, a refused second controller (`busy`), observers denied
  both input and resize, and a takeover that moves the seat and demotes the
  previous holder.
- **Stop:** `stopping_ends_the_process_and_releases_its_registry_record`, and
  a second stop reporting `false` rather than failing.
- **Identity:** `a_recycled_pid_is_stale_even_though_the_process_is_alive`,
  `a_pid_with_no_start_identity_is_unverified_rather_than_guessed_at`,
  `a_live_runtime_is_refused_and_a_recycled_pid_is_replaceable`,
  `a_second_service_refuses_a_namespace_the_first_still_owns`,
  `every_service_start_mints_a_fresh_instance_identity`,
  `a_restored_session_gets_a_new_identity_and_only_records_its_predecessor`.
- **Tier 2:** `a_restore_reports_what_it_cannot_resume_instead_of_respawning_it`
  (layout — rows, cols, cwd — restored; nothing resumed without a verified
  reference), plus
  `resume_argv_uses_the_adapters_verified_flag_and_refuses_to_guess`.
- **Tier 3:** `terminal_history_is_off_by_default_and_warns_when_it_is_not`
  and `the_persistent_runtime_and_its_history_are_both_off_by_default`.
- **Protocol:**
  `a_served_runtime_serves_the_attachment_surface_over_the_real_transport`
  (attach, screen, detach and stop over the actual socket/pipe) and
  `a_client_that_never_heard_of_attachment_disables_it_locally` (a
  previous-minor client negotiates the surface away without a round trip).
- **Gate and compatibility:** `every_verb_refuses_while_the_gate_is_off`,
  `chat_uses_the_runtime_only_with_the_gate_on_and_a_real_terminal`,
  `no_session_is_an_explicit_opt_out_that_defaults_to_off`.
- **Platform specifics:**
  `a_detached_unix_pty_child_survives_because_the_service_still_holds_the_master`
  (`#[cfg(unix)]`, written conservatively — it cannot be compiled on the
  Windows machine this was developed on) and
  `a_conpty_session_resizes_without_disturbing_the_child` (`#[cfg(windows)]`).

### 3.1 Render/event fan-out

`render_fanout_scales_from_one_session_to_fifteen` is `#[ignore]`d because it
spawns 15 real ptys. Measured on the development machine (Windows 11,
i9-13900K, **debug** build, `cargo test --bin zirv
session::host::tests::render_fanout -- --ignored --nocapture`):

| sessions | one full pump pass | one screen render per session (all sessions) |
|---|---|---|
| 1 | 2.7 µs | 59 µs |
| 15 | 62 µs | 824 µs (≈55 µs each) |

Fan-out is linear in the number of sessions on both axes — 15× the sessions
costs ≈23× the pump and ≈14× the render — which is the only property worth
asserting. The test asserts the shape, not a wall-clock threshold: a
millisecond bound on a shared CI box is a flake generator. At the service's
25 ms pump interval, 15 sessions cost ~0.25 % of one core.

## 4. What is deferred

- **The dashboard is still its own PTY owner.** Converting
  `dash/` (≈30 kloc, with `Pane` threaded through spawn requests, budgets,
  permits, mail, delegation and the roster) into a protocol client is step
  N20 (#489). Until then the two modes do not mix: with the gate on, `zirv
  chat` attaches to the runtime and the dashboard is not used on that path;
  with it off, everything behaves exactly as before. So issue #352's
  "closing or crashing the dashboard leaves managed sessions running" is true
  of runtime-owned sessions — nothing a client does can end one — and not yet
  of dashboard-owned panes.
- **Worker panes.** The runtime opens orchestrator seats only (§2.2).
- **Mail INJECTION into a detached session.** Mail addressing, delivery
  files and the registry address all keep working headless; the dashboard is
  still what types a delivered message into a pane. A detached session
  receives mail in the queue and sees it when a client attaches.
- **One namespace per state directory.** The endpoint is derived from the
  state directory (protocol v1's rule, so nothing a checkout controls can
  redirect it), so a second namespace needs its own `ZIRV_CTX_STATE_DIR`.
  The record format already carries the name.
- **Update handoff (tier 4)** and remote transport, a web client and
  cross-machine migration, all of which issue #352 lists as non-goals.
