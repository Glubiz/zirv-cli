# Plan

Implements `intent.md`. Integration branch `research/motion-graphics`
(PR #814); version 4.39.0 (main is 4.38.0).

1. **Plumbing** -- worker A, worktree `D:/GitHub/zirv-motion-a`, branch
   `feat/motion-plumbing`. `ArtifactKind::Video` + `infer_kind`
   (`mp4|webm|mov`); Hyperframes render in `permit::BUILTIN_HEAVY_PATTERNS`;
   `zirv setup` probes for node (>= 22), ffmpeg, optional Python + Kokoro;
   README section; `Cargo.toml`/`Cargo.lock` 4.39.0 (A is the only manifest
   writer). Verify: `cargo nextest run` for artifact/permit/setup, clippy.
2. **Skill** -- worker B, worktree `D:/GitHub/zirv-motion-b`, branch
   `feat/motion-skill`. `src/commands/workflow/skills/motion-graphics/SKILL.md`
   (<= 8 KiB) from Hyperframes' own docs and skills; `CATALOGUE` entry and test
   constants. Verify: `cargo nextest run` for `workflow::skill`, clippy.
3. **Integrate** -- orchestrator merges 1 and 2 into the integration branch
   (rebase, no merge commits) and builds the binary.
4. **End to end** -- worker C, given only "load `motion-graphics` and make
   a 10 s narrated demo plus a GIF": Windows run in a scratch repo with the new
   binary (Node 22, FFmpeg, Kokoro installed), then the same composition
   rendered in Docker `node:22` with FFmpeg and Chrome deps. Evidence: files,
   `ffprobe` audio stream, `zirv artifact list` kinds.
5. **Review and ship** -- one sonnet review of the branch diff, fix confirmed
   findings, the four gates once (failure names diffed against `main`), PR
   #814 retitled as the feature release.
