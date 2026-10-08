# zirv brand guide

Status: settled, 2026-10-08. Evidence-derived; identity-level items approved by the operator on 2026-10-08 ("approve all").
Tokens: brand/tokens.css. Assets: brand/assets/. Changelog: last section.
This guide outranks the frontend profile and general design references for everything it settles. Its don'ts beat any reference suggestion.

Evidence read (2026-10-08): cli.zirv.io (HTML, `index-tumLBnu_.css`, Google Fonts `fonts.css`, `logo.png`, screenshots at 1440 and 390; site shows v2.38.0 so it is stale against the repo's 4.x); `src/style.rs`; `src/commands/ctx/dash/ui.rs`; `README.md`; zirv.io (see section 1, a different product). Marks: [settled] = backed by that evidence or by operator approval 2026-10-08.

## 1. Identity and promise
- What it is: one Rust binary that runs `.zirv/` scripts and supervises Claude Code and Codex sessions (rot scoring, handoff, mail, workflow). [settled] (source: README; cli.zirv.io meta description)
- Promise: "Coordinate your AI coding agents from one CLI." [settled] (source: cli.zirv.io H1). The measured claim "about 20% cheaper on large tasks ... work rated 10% better" is README-only and is quoted only with "(measured)".
- Personality: terse, not curt; precise, not clinical; evidence-first, not boastful. [settled] (derived from site copy and terminal output; operator approval 2026-10-08)
- Name: `zirv`, lowercase in wordmark, commands and the site header; "Zirv" capitalised at the start of a sentence in prose. [settled] (source: site header; site prose). zirv.io (Zirv Fitness, app id 6758999519, indigo/blue palette, Inter-like sans, pill buttons) is a different product that shares only a hexagon mark. Nothing from it is brand evidence for the CLI. [settled] The two products stay visually separate. [settled] (source: operator approval 2026-10-08)

## 2. Audience
- Primary: developers who already run Claude Code or Codex in a terminal and a repo with CI; they read real files and real output. [settled] (source: site copy "Every teammate and every CI runner gets the same commands", install via brew/cargo)
- Secondary: none shown by the evidence.

## 3. Voice and vocabulary
- Tone by situation: marketing: short declarative fragments, a claim then the proof ("A file, not a wiki page."). Product interface: plain labels and states, no decoration. Errors: say what and which value, state the next action. Empty states: a dash, never a zero, for an unknown value. Release notes: not shown by evidence. [settled for marketing and interface (site, style.rs); errors, empty states and release notes settled by operator approval 2026-10-08]
- Person and register: second person "you", imperative buttons, no exclamation marks, no emoji; em dashes allowed. [settled] (site)
- Grammar: sentence case for headings and buttons; mono UPPERCASE only for eyebrow labels; numbers as digits with units ("1h12m", "41%"); product name `zirv` in code font when it is a command. [settled for case and eyebrows; the rest settled by operator approval 2026-10-08]
- Words we use / avoid:

  | Use | Avoid | Why |
  | --- | --- | --- |
  | rot, verdict, handoff, session, transcript, supervise | "AI-powered", "magic", "seamless", "supercharge" | the product's own nouns; the site never uses hype words [settled for the first column; second column settled by operator approval 2026-10-08] |
  | "deterministic", "identical events give an identical verdict" | "smart", "intelligent" scoring | the scorer makes no model call [settled] |
  | "unedited", "complete", "measured" with a source | unqualified percentages | the site shows its evidence [settled] |

- Sample lines (real): heading "Three layers. One install."; heading "It notices the rot before you do."; button "Read the docs →"; note "Carry on. Nothing is written, nothing interrupts."; verdict "A one-line note in the status bar. You decide." [settled] (source: cli.zirv.io)

## 4. Logo
- Files: `assets/logo.png`, 600x600 RGBA raster, the only official mark file; the site's favicon and og:image are this PNG. [settled] (source: cli.zirv.io/logo.png)
- Construction: pointy-top hexagon, three nested hexagons (layers 01-03), outer in a cyan-to-blue gradient (#11e2ff to #2191dd), a navy #193c66 facet, white gap rings baked into the PNG. [settled] (source: logo.png, site copy "which is what the mark has been drawing all along")
- Wordmark: lowercase "zirv" in Archivo bold beside a 22-26px mark in header and footer; no dedicated wordmark asset. [settled for the lockup as the site draws it]
- Vector master, a one-colour or light-background variant, and an SVG favicon: none exist. `assets/zirv-mark.svg` is the approved vector master, reconstructed from the site's inline hexagon geometry (radius ratios 1 / .65 / .35) and the PNG gradient. [settled] (source: operator approval 2026-10-08)
- Clear space: the inner hexagon's width on every side. Minimum size: mark 22px on screen, 48px tall in a 1080p film (22 is the smallest the site draws). [settled] (source: operator approval 2026-10-08)
- Allowed backgrounds: `--color-bg`, `--color-bg-alt`, `--color-surface`. The PNG's white rings are baked in, so it is used on those dark grounds only. [settled by what the site does]
- Misuse: never stretch, rotate, recolour outside the palette, add shadows or glows, outline, rebuild the wordmark in another typeface, or place on a light or busy ground. [settled] (source: operator approval 2026-10-08)

## 5. Colour
Dark only. Pair ratios are WCAG 2.x, computed 2026-10-08. Role/token values live in tokens.css. [settled] unless noted (source: `index-tumLBnu_.css` :root).

| Role | Token | Value | Use | Verified contrast |
| --- | --- | --- | --- | --- |
| background | --color-bg | #080d13 | page ground | -- |
| background alt | --color-bg-alt | #0c131b | alternate section band | -- |
| well | --color-well | #05090d | terminal and code interior | -- |
| surface | --color-surface | #101a25 | cards, tags, code panels | -- |
| surface raised | --color-surface-raised | #16222f | terminal chrome | -- |
| ink | --color-ink | #dce9f0 | primary text | 15.7:1 on bg, 14.2:1 on surface |
| ink soft | --color-ink-soft | #9db4c3 | body, secondary | 9.1:1, 8.2:1 |
| ink muted | --color-ink-muted | #7c93a3 | labels, meta | 6.1:1, 5.5:1 |
| line | --color-line | #1e2e40 | hairlines, decorative only | 1.4:1 (below 3:1; never carries meaning alone) |
| line strong | --color-line-strong | #294059 | hover borders | 1.8:1 (same limit) |
| accent | --color-accent | #22d9f5 | links, eyebrows, headline emphasis, `$` prompt, active marker | 11.4:1 on bg, 10.3:1 on surface |
| accent gradient | --gradient-accent | #11e2ff, #1bb4ee, #2191dd at 28deg | primary button fill, one headline word | dark text on it: 11.4:1 (cyan end), 8.2:1 (mid) |
| accent deep | --color-accent-deep | #0c3a48 | selected tab, badge fill | -- |
| healthy | --color-healthy | #5fd08a | rot 0-39, ok | 10.1:1 on bg |
| advise | --color-advise | #e8b24b | rot 40-64, warning | 10.1:1 |
| compact | --color-compact | #e8843b | rot 65-84 | 7.2:1 |
| restart | --color-restart | #f2705b | rot 85+, error, dead | 6.7:1 |
| focus | --color-focus | #22d9f5 | focus ring | 11.4:1 [settled: operator approval 2026-10-08; the site declares none] |

- Proportion: about 85% ground and surfaces, ink for text, cyan for at most a few marks per screen; verdict colours only where a verdict is shown. [settled] (source: screenshots; operator approval 2026-10-08)
- Verdict colours always travel with their word (healthy, advise, compact, restart) and a glyph; colour alone never carries state. [settled by the site's verdict list; as a rule, operator approval 2026-10-08]
- Never: accent as a large field (only the button gradient fills), verdict colours as decoration, pure black or white grounds, the Zirv Fitness indigo. [settled] (source: operator approval 2026-10-08)
- Light theme: none; dark only. Not tokenised. [settled] (source: operator approval 2026-10-08)
- Terminal mapping: the terminal uses ANSI named colours (Cyan, Green, Yellow, Red, Magenta) so it inherits the user's theme; nothing maps them to the hex above. [settled that they differ] Rule: keep ANSI in the terminal, use the hex only on web and film, and treat the correspondence as accent=Cyan, healthy=Green, advise=Yellow, restart=Red. [settled] (source: operator approval 2026-10-08)
- Compact versus restart: the site shows four verdict colours; the dashboard collapses compact and restart into one red band. Both stay; the film uses the four-band set. [settled] (source: operator approval 2026-10-08)

## 6. Type
- Families: display Archivo 600/700/800 (headings 700 and 800); text IBM Plex Sans 400, 400 italic, 500, 600; mono JetBrains Mono 400/500/700, the dominant face (nav, labels, tags, terminal, buttons). [settled] (source: `--font-display/--font-body/--font-mono`). Files in `assets/fonts/` (latin subset, woff2, from @fontsource 5.3.0), all SIL OFL 1.1, licence files alongside. The site loads them from Google Fonts; self-hosting is this guide's change.
- Fallback stacks: "Segoe UI", system-ui; ui-monospace, SFMono-Regular, Menlo, monospace. [settled]
- Sizes (px): text 11, 13, 16, 19, 23, 28, 33, 40, 48, 58; mono 11, 12.5, 13, 15. [settled]. Body 16px. Heading tracking -0.015 to -0.035em. Eyebrows: mono, uppercase, 0.16em; tags 0.04em. [settled]. Line heights 1.6 body and 1.08 headings [settled: operator approval 2026-10-08; not read from the CSS].
- Rules: headline emphasis is one phrase in cyan, not a different weight; data, versions, paths, and commands in mono; tabular digits for numbers [settled: operator approval 2026-10-08]; prose measure 68ch [settled].

## 7. Layout and geometry
- Grid and widths: page 1140px, measure 720px, prose 68ch; label column 152px, rail 216px, sidebar 272px; breakpoints 1080, 900, 820, 560. Sticky hairline header; two-column hero (copy and a live terminal mock); rows of label column, body and tag chips; sections divided by 1px lines. [settled]
- Spacing (px): 4, 8, 12, 16, 20, 24, 32, 40, 48, 56, 64, 80, 96; gaps 8/12/16; card padding 16 by 20. [settled]
- Radii 4 to 10px (4 most used, then 8), 50% for status dots; borders 1px hairline. [settled]
- Elevation: none. Depth is surface steps plus hairlines; the only shadow is `inset 2px 0 0` accent on the active nav item. No blur shadows or glows. [settled]
- Density: dense, mono-heavy, terminal-first on every screen. [settled]
- A faint hexagon motif (three nested hexes, accent stroke at 8.5, 6.5 and 4.5% opacity, 640px, no parallax) may sit behind hero content. [settled]

## 8. Imagery and illustration
- Imagery is the product itself: real terminal output and real files, shown complete and "unedited", in a window with a `●●●` chrome bar. [settled] (site)
- Illustration, photography: none found; none are used until the operator asks. [settled as absence]
- Alt text: describes the content of the terminal or file shown. [settled] (source: operator approval 2026-10-08)

## 9. Icons
- Custom and minimal: the nested hexagon (outline, accent stroke 1.6px, round joins), plus text glyphs: `$` prompt, `●` live, `○` dead, `✔` step done, `✉` mail, `→` and `↗` links. [settled for the glyphs and hexagon]
- Terminal glyphs: working `⠋` cyan, idle `●` green, needs action `▲` yellow, done unread `◆` magenta, failed `✗` red, unknown `·` dim, rot verdict `✻`. Unknown values are the en dash `–`. [settled] (source: `src/style.rs`, `dash/ui.rs`)
- A general icon set: none until needed. [settled] (source: operator approval 2026-10-08)

## 10. Interface motion
- The site has exactly two keyframes: `blink` 1.15s steps(1) infinite (terminal cursor, on 55%) and `shimmer` 1.6s linear infinite (loading skeleton). No `transition`s. Both stop under prefers-reduced-motion. [settled]
- Terminal: rollover pulse 1.6s bold/plain; toast 5s; row flash 900ms; `Motion::Reduced` honoured everywhere. [settled] (source: `dash/ui.rs`)
- New web interface transitions (`--dur-press/small/medium`, `--ease-out/in/move`): short, fade and 4px translate only, no springs or bounces. [settled] (source: operator approval 2026-10-08)
- Reduced motion: remove blink, shimmer and transitions (state changes become instant). [settled for blink and shimmer]

## 11. Film motion
All of this section is [settled] by operator approval 2026-10-08; the evidence itself covers only the cursor blink, the shimmer, and the colours and terminal conventions the film reuses. A 12 s film, 1920x1080, 30 or 60 fps, copied tokens, dark ground `--color-bg`.

- Look: the screen is a cli.zirv.io page in motion. Ground #080d13, hairlines #1e2e40, mono labels, cyan as the single accent, a terminal window (well #05090d, chrome `●●●`) as the main set. Nothing glows, blurs, bevels or gradient-fills except the mark and the one gradient button.
- Pace: 0.5 s beat (24 beats in 12 s). Cuts and key lines land on beats. About 5 to 6 scene changes in 12 s; no scene shorter than 1 s except the verdict ladder.
- Eases: arrivals `power3.out` 0.5 s (`--gsap-enter`); exits `power2.in` 0.35 s; moves `power2.inOut` 0.8 s. Typed terminal text is linear, 35 ms per character, with the `blink` cursor (1.15 s, steps(1), on 55%). Verdict colour changes are hard cuts (`steps(1)`), never tweened, because the scorer is deterministic and the film should look it.
- Transitions: (1) hard cut on the beat; (2) hairline wipe, a 1px cyan line sweeping across and revealing the next surface; (3) type-on of terminal lines. Never: crossfade dissolves, zoom or whip pans, 3D spins, glitch, particles, lens flare, parallax.
- Type at 1080p: statement 140px Archivo 800, tracking -0.025em, one phrase in cyan; headline 88px Archivo 700; body 36px Plex 400; terminal 30px JetBrains Mono 400; eyebrow label 22px mono uppercase 0.16em. Hairline rules 2px at 1080p (1px scaled).
- Verdict ladder (the signature beat): four rows, healthy / advise / compact / restart, with bands 0-39, 40-64, 65-84, 85+; a `✻` glyph in the row colour (#5fd08a, #e8b24b, #e8843b, #f2705b) and its one-line action from the site. Rows appear on consecutive beats; a score counter in mono steps up in discrete increments (stepped, no easing), and the active row takes the verdict colour as a hard cut. A transcript of event lines scrolls beside it.
- Hexagon: the three nested hexes draw in layer by layer (01, 02, 03) over three beats as 2px cyan strokes, then settle into the mark; at the end the three layers are the three zirv layers named in the site copy (script, ctx, workflow). The faint 640px motif sits behind at the site's low opacities.
- Shape of the 12 s: 0-2 s cursor and `$ zirv ctx status` typed, mark draws; 2-5 s the supervised sessions table with live/dead markers; 5-9 s the verdict ladder; 9-11 s headline "Coordinate your AI coding agents, from one CLI." (agents in cyan); 11-12 s end card.
- End card: mark (about 160px) beside lowercase `zirv` in Archivo 800, the install line `brew tap glubiz/homebrew-tap && brew install zirv` in mono with the cyan `$`, hold 1.5 s on beat 22 to 24, last frame is the poster frame.
- Sound: silent by default; if sound is wanted, keyboard clicks and a single low tick on verdict changes, no music. [settled] (source: operator approval 2026-10-08)

## 12. Accessibility commitments
- Contrast: text 4.5:1 and UI 3:1 against the surface it sits on; every ink and accent pair above passes 4.5:1 on bg and surface. Hairlines are decorative and never the only cue. [settled for the verified pairs]
- Focus: a visible 2px accent ring (`--color-focus`); the site declares none, so this is [settled] by operator approval 2026-10-08.
- Reduced motion: honoured as in section 10. [settled]
- Targets 44px on touch; film captions for every spoken line. [settled] (source: operator approval 2026-10-08)
- State is never colour only: verdicts and statuses carry a word and a glyph. [settled] (source: operator approval 2026-10-08)

## 13. Don'ts
- No light theme, gradients as backgrounds, glows, blur, drop shadows, glassmorphism, or illustration. [settled, consistent with the site's flat hairline look; operator approval 2026-10-08]
- No hype vocabulary or emoji; no exclamation marks. [settled] (source: operator approval 2026-10-08)
- Do not use Zirv Fitness colours, fonts or copy; do not claim a number the repo does not measure. [settled] (source: operator approval 2026-10-08)
- Do not re-mark the logo or substitute the typefaces. [settled] (source: operator approval 2026-10-08)
- Do not fabricate terminal output in marketing; show real, complete output. [settled by the site's own "unedited" practice]

## 14. Approved proposals
The operator approved every proposal in the draft on 2026-10-08 ("approve all"): vector master of the mark (`assets/zirv-mark.svg`), logo rules (clear space, minimum size, misuse, mono variant derived from the SVG), canonical accent (#22d9f5, gradient for fills only), dark only, terminal-to-web colour mapping, four verdict colours (three in the dashboard), interface motion tokens, film motion, focus ring, line heights, voice rules, and separation from Zirv Fitness.

Still open (not a proposal): cli.zirv.io shows v2.38.0 while the repo is 4.x; the site needs a refresh.

## Changelog
| Date | Change | Reason | Source |
| --- | --- | --- | --- |
| 2026-10-08 | Guide established (draft) from cli.zirv.io CSS/screens, src/style.rs, dash/ui.rs, README; fonts self-hosted from @fontsource 5.3.0 | operator asked for the brand guide | brand-evidence/evidence.md, wt-design-skills |
| 2026-10-08 | All proposed items approved by the operator: vector master of the mark, logo rules, canonical accent, dark only, terminal-to-web colour mapping, four verdict colours, interface motion tokens, film motion, focus ring, line heights, voice rules, separation from Zirv Fitness. `assets/zirv-mark.proposed.svg` renamed `assets/zirv-mark.svg` | operator reply "approve all" | operator approval 2026-10-08 |
