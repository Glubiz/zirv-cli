# Intent

## Problem

zirv-wrapped agents cannot produce motion graphics. `src/` has no mention of a
renderer (no ffmpeg, Hyperframes or Remotion), so an operator who wants a
launch video or a README GIF has to install a third-party skill per host
(`/brag`, the Hyperframes skills) plus Node 22 and FFmpeg by hand. Nothing
checks those prerequisites, nothing queues a CPU-heavy render against other
heavy work, and nothing records the result as an artifact. Codex and Copilot
seats get no skill at all. Evidence and option analysis: `research.md`.

## Desired outcome

One release PR in which any wrapped agent, asked for "a 15-second launch video"
or "a GIF of X", loads the built-in `motion-graphics` skill and leaves in
`<repo>/motion/<slug>/`: committed composition source, a rendered MP4 (with
narration and operator-supplied music when available) and a GIF, both
registered as zirv artifacts.

## Requirements

1. Built-in catalogue skill `motion-graphics` (instructions <= 8 KiB), the
   same for every harness, driving a pinned `npx --yes hyperframes@0.8.80`:
   `doctor` preflight, `init`, `docs <topic>` for reference, plan then
   author, `lint --json` / `check` / `snapshot` gates with the stills actually
   inspected, audio, `render --format mp4`, `render --format gif --fps 15
   --gif-loop 0`, then `zirv artifact render` and `zirv artifact present`.
   The skill never calls `ffmpeg` directly, never installs packages, and never
   downloads music.
2. Audio: narration with `hyperframes tts` (local Kokoro, no account) or the
   operator's ElevenLabs key when it is set in the environment; music and SFX
   only from `<repo>/motion/assets/audio/`; `beats` for cut timing and
   `normalize-audio` for loudness. Without Python/Kokoro the skill renders
   without narration and says so.
3. Output: the Hyperframes project is `<repo>/motion/<slug>/` (committed);
   renders go to `<repo>/motion/<slug>/renders/`, which the skill git-ignores
   in that folder.
4. `zirv setup` reports `node` (flagging < 22), `ffmpeg`, and Python 3 with
   the Kokoro modules as optional, using `setup::executable_exists`.
5. A Hyperframes render is a heavy operation (`permit::BUILTIN_HEAVY_PATTERNS`),
   so it queues under `max_heavy_operations`; `lint`/`check`/`snapshot` do not.
6. `ArtifactKind::Video`; `artifact::infer_kind` maps `mp4|webm|mov` to it
   (`gif` stays `Image`).
7. README section for the skill: requirements, first-run downloads, audio
   sources and licences; `Cargo.toml` version bumped above `main`.

## Non-goals

A renderer inside zirv; a render tool on the MCP bridge (read-only by
contract); Remotion support; generated music (MusicGen weights are
CC-BY-NC-4.0, the other engines need HeyGen or Google accounts); bundled audio
assets; HeyGen cloud render or publish; porting `/brag`; media preview in dash.

## Constraints

- Compiled-in skills carry no resource files and at most 8 KiB of
  instructions (`skill.rs:19`, `skill.rs:396-399`); the discovery digest stays
  within 32 KiB.
- `npx` runs prompt-free, so the Hyperframes version is pinned in the skill
  text and bumped deliberately with a zirv release.
- No new permission allow-list entries; `ffmpeg` stays unlisted.
- Repo-owned surfaces may only narrow: a repository cannot grant the skill
  capabilities or override its pin.

## Assumptions

- Hyperframes 0.8.80's `render --format`, `tts`, `beats`, `normalize-audio`,
  `docs` and `doctor` behave as their source describes; the end-to-end check
  below proves it.
- Kokoro's first-run download (310 MiB model + 26 MiB voices into
  `~/.cache/hyperframes/tts`) is acceptable and disclosed in the README.
- Hyperframes manages its own Chrome (`hyperframes browser`); zirv does not
  hand it a system browser.

## Open questions

- End-to-end verification on this Windows machine needs Node 22+ and FFmpeg
  (it has Node 19.6.0, no FFmpeg). Default: verify in Docker (`node:22` +
  FFmpeg + Chrome deps, `--shm-size=512m`) and on Windows once the operator
  has installed both.

## Acceptance criteria

- [ ] `zirv skill load motion-graphics` prints the skill; `skill_list` shows it
      for claude and codex seats; catalogue test constants updated; its
      instructions are <= 8 KiB.
- [ ] `zirv artifact render x.mp4` records kind `video`; a `.gif` records
      `image`.
- [ ] `permit::is_heavy` is true for `npx --yes hyperframes@0.8.80 render
      --format gif ...` and false for `npx hyperframes lint`.
- [ ] `zirv setup` reports node (with the < 22 warning), ffmpeg, and optional
      Python/Kokoro.
- [ ] In Docker, a wrapped agent following the skill turns a sample brief into
      `motion/demo/` with an MP4 that has an audio stream (Kokoro narration,
      checked with `ffprobe`) and a 15 fps GIF, both registered as artifacts.
- [ ] `cargo build`, `cargo nextest run --no-fail-fast`, `cargo fmt -- --check`,
      `cargo clippy --all-targets -- -D warnings` pass (failure names diffed
      against `main`); README updated; version bumped.
