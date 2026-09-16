# Session tracking: `sessions.rs` / `session/` / `dash/`

The name "session" is genuinely overloaded across three different things
here, and none of the three cross-references the other two by name --
this file is that missing cross-reference.

## `sessions.rs` -- the on-disk registry

- **Scope:** `<state>/sessions/<short8>.json`, one file per live supervisor.
- **Owns:** best-effort bookkeeping (`SessionGuard`, `Record`) that `wrap`/
  `exec`/`run_loop`, every `dash/pane.rs` pane, and `session/host.rs` (the
  persistent runtime, below) all register into and read from -- it is the
  one shared registry, not specific to any single supervisor.
- **Does not own:** whether the process a record names is actually still
  alive, or the process itself; a registry write/read failure never fails a
  launch.

## `session/` -- the opt-in persistent runtime (`zirv session`, issue #352)

- **Scope:** a local service that OWNS the pty/ConPTY process directly, so a
  session survives the client that was looking at it.
- **Owns:** `serve`/`list`/`attach`/`detach`/`stop` and the durable identity
  record (`namespace.rs`) that lets a client reattach to the same terminal.
  Gated on `[session] persistent` (operator-only, off by default) -- with
  the gate off, every other surface behaves exactly as if this module did
  not exist.
- **Does not own:** the ordinary, non-persistent child process model `dash`/
  `wrap`/`exec` use every day; this is an alternate runtime, not the default
  one.

## `dash/` -- the interactive multiplexer (`zirv chat`)

- **Scope:** one dashboard process attaching N interactive panes, each an
  ordinary ConPTY child the dashboard owns directly (via `wrap`-style
  supervision per pane), rendered through its own embedded `vt100` screen.
- **Owns:** the event loop, pane layout/focus, and registering each pane's
  own `sessions::Record`/`SessionGuard` in the shared registry above.
- **Does not own:** the registry format itself (`sessions.rs`), or process
  survival past the dashboard exiting -- a pane only outlives the dashboard
  when `session/`'s persistent runtime is deliberately turned on.
