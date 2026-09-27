# Motion graphics from wrapped agents -- research

Date: 2026-09-27. Status: research for workflow
`6a1aa876-84fc-40d0-9790-04ee8f70b705`, no code. Question: how can a
zirv-wrapped agent (claude, codex, copilot, ...) generate motion graphics --
MP4/WebM video and GIF -- the way `/brag` and other Claude motion-graphics
skills do?

## Answer

Do not build a renderer. Every serious agent motion-graphics tool renders
agent-authored web content (HTML or React) in headless Chrome, frame by frame,
and encodes with FFmpeg. zirv's job is the part those tools leave to each host:
a harness-agnostic skill that routes any wrapped agent to the renderer, a
preflight that says what is missing, and plumbing so renders are governed
(heavy-operation permits) and surfaced (artifacts).

Recommended backend: **HeyGen Hyperframes** (Apache-2.0, plain HTML/CSS/JS,
deterministic seek-per-frame capture, native MP4/WebM/MOV/GIF output). Remotion
stays an opt-in alternative because of its license.

## What the prior art actually does

**`/brag`** (`latent-spaces/brag`, MIT; `aceman23/brag-videoadgenerator-claude`
is a fork). A Claude Code skill that turns a repo into a 15-25 s launch video.

- Layout: `skills/brag/{SKILL.md, references/step-1..4-*.md, audio.md,
  tones.md, scripts/analyze_music_cues.py, assets/{music,sfx}}`, plus a
  single-file `brag-slim` variant that uses no Hyperframes and tells the model
  to "make the whole video yourself ... with whatever tools are on the machine".
- Four gated steps: inspect the code, plan a storyboard (`brag-plan.md`),
  write a `composition-brief.md` and author the composition with Hyperframes'
  own domain skills, then `npx hyperframes check` (contrast + layout gate),
  `snapshot`, `preview`, `render --output ../brag.mp4`; the poster frame comes
  from a plain `ffmpeg -ss <t> -frames:v 1`.
- Music and SFX are bundled files (ende.app music, Kenney.nl SFX); nothing is
  generated and no API key is needed. Optional narration uses Kokoro via
  Hyperframes.
- Requirements: Node 22+, FFmpeg on PATH, `npx hyperframes`.

**Hyperframes** (`heygen-com/hyperframes`, Apache-2.0 for code; bundled
media/fonts excluded; npm `hyperframes` 0.8.80, `engines.node >=22`).

- "Video as code": HTML/CSS/JS with a seekable timeline (GSAP, CSS keyframes,
  WAAPI, Anime.js, Lottie, Three.js). Puppeteer seeks each frame in headless
  Chrome and FFmpeg encodes it, so identical input gives identical output.
- `render --format mp4|webm|mov|gif|png-sequence|hls`; WebM and MOV (ProRes
  4444) keep alpha. Its own example for docs/PR GIFs is
  `hyperframes render --format gif --fps 15 --gif-loop 0 --output demo.gif`
  (`packages/cli/src/commands/render.ts`).
- Agent-relevant commands: `init`, `lint --json`, `check`, `snapshot`,
  `preview`, `render`, `doctor` (Node/Chrome/FFmpeg, with a Windows branch),
  `browser` (manages its own Chrome), and `docs <topic>` (inline reference in
  the terminal -- lets any agent pull reference text without installing
  skills).
- Ships 21 agent skills (`npx skills add heygen-com/hyperframes`) for Claude
  Code, Codex, Cursor, Gemini CLI; installed per host.

**Remotion** (React authoring, headless Chrome Headless Shell, `--codec=gif`
built in). Free only for individuals, for-profit orgs with **up to 3
employees**, non-profits and evaluation; anyone else needs a paid Company
License. Official agent skills in `remotion-dev/skills`.

**Others**, one line each: Motion Canvas / Revideo (MIT, TS/React canvas;
third-party MCP only); Manim (MIT, Python, math explainers; community MCP
servers); Lottie pipelines (animation JSON, not a video renderer);
`Claude-Code-Video-Toolkit` and `remotion-video-mcp` (third-party bundles
around Remotion/Manim/FFmpeg).

## Options

| Option | Verdict |
|---|---|
| A. Do nothing; operators install brag or the Hyperframes skills per host | Works today in a wrapped Claude session with Node 22 + FFmpeg. Per-host install, no zirv preflight, no artifact or permit integration, codex/copilot each need their own install. |
| **B. Thin built-in `motion-graphics` skill + small plumbing, Hyperframes backend** | **Recommended.** Reaches every wrapped harness through zirv's own skill index; no renderer code in zirv. |
| C. Native `zirv motion render` in Rust (own CDP frame capture + FFmpeg) | Reject: reimplements Hyperframes/Remotion for no gain. |
| D. Mutating `render` tool on the zirv MCP server | Reject: the MCP bridge is contractually read-only -- "All tools are read-only ... Use the zirv CLI for mutations." (`src/commands/ctx/mcp.rs:44-49`). |
| E. Remotion as default backend | Reject as default: pushes a paid-license obligation onto most company users. Fine as an operator's own opt-in. |

## Recommended design (option B)

1. **Built-in skill** `src/commands/workflow/skills/motion-graphics/SKILL.md`,
   added to `CATALOGUE` (`src/commands/workflow/skill.rs:1623`, bump the
   `CATALOGUE_LEN`/`BUILTIN_LEN` test constants). Text-only by necessity:
   compiled-in skills get `resources: Vec::new()` (`skill.rs:396-399`) and are
   capped at 8 KiB (`MAX_INSTRUCTION_BUDGET`, `skill.rs:19`). Method, in brief:
   - preflight `npx --yes hyperframes@<pinned> doctor`; stop and report what is
     missing rather than improvising;
   - scaffold with `hyperframes init` in the output directory; pull reference
     with `hyperframes docs <topic>` instead of relying on host-installed skills;
   - plan scenes and duration first, then author one seekable timeline;
   - gates before a full render: `lint --json`, `check`, then `snapshot` and
     actually look at stills from every scene and mid-transition;
   - add audio per the Audio section below;
   - `render --format mp4`, and for GIF `render --format gif --fps 15
     --gif-loop 0` (width ~480-720 px); never call `ffmpeg` directly;
   - register outputs with `zirv artifact render <file> --kind ...` and
     present with `zirv artifact present`.
2. **Preflight in `zirv setup`/status**: reuse `executable_exists`
   (`src/commands/setup.rs:456-479`) to report `node` (>= 22) and `ffmpeg`.
   Chrome needs no zirv probe -- Hyperframes manages its own via `browser`;
   `frontend_render::discover_browser` exists if we later want to hand it a
   system Chrome.
3. **Heavy-operation permit**: add a Hyperframes render pattern next to
   `BUILTIN_HEAVY_PATTERNS` (`src/commands/ctx/permit.rs:78`) so parallel
   workers queue renders like `cargo test` instead of running several Chrome +
   FFmpeg pipelines at once.
4. **Artifacts**: add `ArtifactKind::Video` and map `mp4|webm|mov` to it in
   `infer_kind` (`src/commands/workflow/artifact.rs:136`; GIF already maps to
   `Image`). Registration is path-only and must stay inside the repo
   (`artifact.rs:172-174`); MCP `artifact_read` is UTF-8-only, which is fine --
   video is located, not streamed.
5. **Docs**: README section for the skill and its host requirements.

No permission change is needed: `npx`/`node`/`npm` are already in
`SHIPPED_POSTURE_ALLOW` (`src/commands/ctx/adapters/mod.rs:717-724`), so renders
run prompt-free in interactive and delegated workers alike. FFmpeg is *not*
allowed and falls to `headless_default = ask`, which would silently block a
delegated worker -- hence "never call `ffmpeg` directly"; Hyperframes invokes
FFmpeg itself as a child of `node`, outside the host's Bash gate.

## Risks and gaps

- **Supply chain**: `npx` runs prompt-free, so an unpinned `npx hyperframes`
  executes whatever npm serves that day. Pin the version in the skill text and
  bump it with zirv releases.
- **Trust boundary**: a repository may supply its own render skill/templates,
  but repo skills are `repository-untrusted`, explicit-load-only and never
  synced into a host's native skill list (README trust-boundary table); that
  stays as is.
- **brag as a zirv bundle does not fit**: operator-global bundles are capped at
  64 KiB per file / 256 KiB total (`skill.rs:22-31`), so brag's bundled music
  cannot load as a zirv bundle. Operators who want brag install it as a native
  Claude Code skill; it then works inside wrapped Claude sessions unchanged.
- **GIF fallback**: for converting an existing MP4 (for example from
  Remotion), use FFmpeg's two-pass palette method with matching filters:
  `ffmpeg -i in.mp4 -vf "fps=15,scale=640:-1:flags=lanczos,palettegen" p.png`
  then `ffmpeg -i in.mp4 -i p.png -filter_complex
  "fps=15,scale=640:-1:flags=lanczos[x];[x][1:v]paletteuse" out.gif`. Do not
  depend on gifski: its encoder core is AGPL-3.0-or-later.
- **Verification**: this Windows dev machine has Node 19.6.0 and no FFmpeg, so
  nothing here was rendered. The implementation PR must render one MP4 and one
  GIF end to end -- on Linux (Docker `node:22` + FFmpeg; Chrome needs
  `--shm-size=512m`) and on Windows after installing Node 22 and FFmpeg.
  Hyperframes' Windows runtime support is asserted by its `doctor` code, not
  verified here.

## Audio

Small enough to include: Hyperframes already does the audio work, so zirv's
share is skill text plus one optional preflight probe.

- Mixing: `<audio>` elements in the composition carry timing and volume
  attributes and are mixed into the MP4/WebM/MOV at render. GIF has no audio
  track.
- Beat sync: mark the track `data-timeline-role="music"` and
  `hyperframes beats` writes `beats/<audio>.json` to time cuts against.
- Loudness: `hyperframes normalize-audio` matches clips by integrated LUFS.
- Narration: `hyperframes tts "<text>" --voice <id> --output vo.wav`. Engine
  order (`packages/cli/src/audio/providers.ts`): HeyGen Starfish (account) ->
  ElevenLabs (key) -> **Kokoro-82M, local** (weights Apache-2.0; needs
  Python 3 plus `pip install kokoro-onnx soundfile`).
- Music generation: HeyGen library (account) -> Lyria (Google key) ->
  MusicGen local (`transformers torch`, several GB; `facebook/musicgen-*`
  weights are **CC-BY-NC-4.0, non-commercial**). Not a default.

Scope for zirv: narration via Kokoro (or the operator's own ElevenLabs key
from the environment, never echoed); music and SFX only from files the
operator puts in `<repo>/motion/assets/audio/`, never downloaded by the agent;
no music track when none is supplied. Setup reports Python 3 + the Kokoro
modules as optional ("narration unavailable"), and the skill renders without
narration rather than installing packages itself.

## Decisions

1. Backend: **accepted** -- Hyperframes (Remotion stays out of scope).
2. Output location: **accepted** -- composition source committed in
   `<repo>/motion/<slug>/`, rendered media git-ignored there.
3. Scope of the first PR: **accepted** -- steps 1-5 above plus audio as
   scoped in the Audio section, in one release.

## Sources

- https://github.com/latent-spaces/brag (README, LICENSE, `skills/brag/SKILL.md`, `references/step-3-compose.md`, `references/step-4-deliver.md`, `skills/brag-slim/SKILL.md`)
- https://github.com/aceman23/brag-videoadgenerator-claude
- https://github.com/heygen-com/hyperframes (README, LICENSE, `packages/cli/README.md`, `packages/cli/src/commands/{render,doctor,docs,browser,tts,beats,normalize-audio}.ts`, `packages/cli/src/audio/providers.ts`, `packages/cli/src/tts/python.ts`); `npm view hyperframes version engines`
- https://huggingface.co/hexgrad/Kokoro-82M, https://huggingface.co/facebook/musicgen-small (license fields via the Hub API)
- https://github.com/remotion-dev/remotion/blob/main/LICENSE.md, https://github.com/remotion-dev/skills, https://www.remotion.dev/docs/render-as-gif
- https://github.com/ImageOptim/gifski (LICENSE), https://blog.pkh.me/p/21-high-quality-gif-with-ffmpeg.html
- https://github.com/motion-canvas/motion-canvas, https://github.com/ManimCommunity/manim, https://github.com/wilwaldon/Claude-Code-Video-Toolkit, https://github.com/dev-arctik/remotion-video-mcp
