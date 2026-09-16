# Disclaimer

zirv is provided "AS IS", without warranty of any kind, express or implied,
as stated in full in the [MIT License](LICENSE). Use it at your own risk.

## Autonomous supervision

`zirv ctx wrap`, `exec`, `loop`, and the dashboard supervise a live coding
agent on your machine: at a verified-idle turn boundary they can type a
`/compact` or a restart into the session, and they can inject one labelled
`[zirv ▸ mail] ...` advisory line when mail arrives. `exec`/`loop` can also
kill and relaunch the underlying process with a distilled handoff. None of
this happens unless you run one of these commands (or the dashboard, which
uses them) -- running your harness directly bypasses it entirely. Individual
pieces can be turned off: mail's advisory injection, for example, stops with
`[mail] enabled = false` in `ctx.toml`. See [Configuration](README.md#configuration)
and [Context Management](README.md#context-management-zirv-ctx) for the rest.

## Binary integrity

Every release asset ships with a `.sha256` checksum file; `zirv update`
downloads and verifies it before installing, and refuses to install an
unverified binary. Verify a manual download yourself before running it.

## Third-party harnesses and repo-owned configuration

zirv wraps third-party coding harnesses (Claude Code, Codex, and others) that
it does not control -- their own behavior, billing, and terms are between you
and their vendors. A checked-out repository's own `zirv` configuration,
system prompt, context files, memory, skills, and agents are untrusted input
by design: they can narrow what a session does but never widen a security
setting or grant themselves capability zirv itself did not already allow.
See the [Trust boundary](README.md#trust-boundary) table for exactly what
each surface can and cannot do.
