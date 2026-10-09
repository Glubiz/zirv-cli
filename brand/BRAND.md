# zirv brand guide

Status: revised 2026-10-09 to the visual direction the operator approved in the cli.zirv.io design session of 2026-10-08/09 (the "opus" mock, refinement 3, "much better"). It supersedes the 2026-10-08 guide in everything except the logo. Items marked [settled] are backed by the approved mock or by the operator's stated feedback; items marked [proposed] are carried over or derived and need the operator's read.
Tokens: brand/tokens.css. Assets: brand/assets/. Changelog: last section. Each section ends with what changed against the 2026-10-08 version.
This guide outranks the frontend profile and general design references for everything it settles. Its don'ts beat any reference suggestion.

Evidence read (2026-10-09): the approved mock `mocks/opus/index.html` (`:root` tokens, component CSS, behaviour; embedded docs content skipped), its `design-note.md`, `captures/v5/*.png` at 1440, 768 and 390, and `render/README.md` (real dashboard render pipeline); `README.md` (benchmark paragraph, verdict thresholds); the 2026-10-08 guide's own evidence (`src/style.rs`, `src/commands/ctx/dash/ui.rs`). The mock files live in the design session's scratchpad, not in the repo; the render pipeline's source is the ignored test `site_hero_dump` in `src/commands/ctx/dash/tree_view/orch.rs`.

## 0. Rules from the design session
These are the operator's feedback, stated as rules. They beat any default.
1. Stand out from generic AI sites without a heavy themed metaphor. Rejected and never to return: a railway signal-box panel; a hexagon "hive" with orbiting sessions ("a bit too cheezy"); a dark hex-geometric split hero with a small 3D graphic (generic). The hexagon is the logo and the verdict glyph, not a scene.
2. Never draw zirv's UI from memory. Every terminal or dashboard depiction is a real render of zirv's own output (for the dashboard, its ratatui draw code through a test backend) or real command output, with example data labelled as examples.
3. No coloured left-border callouts and no accent-bar boxes ("very AI"). See section 7 for the one narrow nav exception, which the operator must confirm.
4. Present the whole product in proportion. Rot detection is one feature among peers (scripts, sessions, workflows, safety, pacing, benchmark), never the headline and shown once on a landing at the weight of pacing and handover.
5. Claims carry their source. Benchmark figures come from README and the results write-up, with significance markers and caveats, never cherry-picked.
6. Large display type never clips descenders (the headline "g" fix, section 6).
Changed: new section; rules 1 and 3 to 6 are new, rule 2 hardens the old "do not fabricate terminal output".

## 1. Identity and promise
- What it is: one Rust binary around coding agents: a `.zirv/` script runner; supervision of 13 harnesses with a dashboard, delegation, mail and memory; the intent-to-deploy workflow with gates, skills and team composition; a harness-neutral command-safety policy and trust boundary; usage pacing, cross-harness fallback and handover; rot detection; setup and migration; `zirv benchmark`. [settled] (source: design-note.md "Subject"; README)
- Thesis and promise: "Agents write the code. zirv runs the rest." The second line is the thesis; scripts, sessions, workflows, safety and pacing are "the rest". [settled] (source: approved mock H1). The earlier promise "Coordinate your AI coding agents from one CLI." is retired as a headline; it stays valid as a description.
- Measured claim: only in the form of section 8's benchmark rules.
- Personality: calm, exact, a little dry; evidence-first, not boastful. [settled] (source: design-note.md "Voice")
- Name: `zirv`, lowercase in wordmark, commands and the site header; "Zirv" capitalised at the start of a sentence in prose. [kept] zirv.io (Zirv Fitness) is a different product and stays visually separate; nothing from it is evidence. [kept]
Changed: promise and headline; product description widened from "scripts plus ctx" to the whole surface; personality wording.

## 2. Audience
- Developers who run Claude Code or Codex for hours, often several sessions at once; they distrust hype and check claims. [settled] (source: design-note.md "Audience")
Changed: sharpened from "developers who already run Claude Code or Codex in a terminal".

## 3. Voice and vocabulary
- Calm, exact, a little dry. Section labels cite where the content comes from (`zirv workflow --help`, `README.md: what wrapping costs (measured)`). [settled] (source: design-note.md "Voice", mock)
- Tone by situation: marketing: a plain claim, then its proof. Interface: plain labels and states. Errors: say what and which value, then the next action (the 404 page quotes zirv's real did-you-mean error, never an invented command). Empty and unknown values: a dash, never a zero. [settled for the first two and the 404; the rest kept]
- Person and register: second person, imperative buttons, no exclamation marks, no emoji. Sentence case for headings and buttons; numbers as digits with units; thresholds exact (40/60/80; gates at 50% and 80% of the window). Mono for commands, paths, versions and labels. [kept; thresholds settled by README]
- Words:

  | Use | Avoid | Why |
  | --- | --- | --- |
  | session, harness, workflow, gate, verdict, handoff, supervise, "runs the rest" | "AI-powered", "magic", "seamless", "supercharge", "smart" | the product's own nouns; no hype [kept] |
  | "measured" with its source, "example data", "a real render" | unqualified percentages; "up to" without its denominator | claims carry their source (rule 5) |
  | "deterministic", "identical events give an identical verdict" | "intelligent" scoring | the scorer makes no model call [kept] |

- Real lines from the approved design: "Agents write the code. zirv runs the rest."; "Same model. Lower cost per task, and work rated higher."; buttons "Read the docs", "What it covers"; caption "A real render of zirv's dashboard, drawn by its own ratatui code. The session is example data: the repository, tasks, models and times are examples."; "Reset the example"; "✓ Nothing needs you right now". [settled] (source: mock, README)
Changed: voice summary and sample lines replaced; "It notices the rot before you do." and "Three layers. One install." retired as headlines (rule 4); source-citing section labels added; hype table kept and extended.

## 4. Logo
The logo is unchanged. [settled by the operator: "the logo stays"]
- Mark: pointy-top hexagon, three nested hexagons, gradient #11e2ff, #1bb4ee, #2191dd at 28deg. Files: `assets/zirv-mark.svg` (vector master; the mock's header mark is the same geometry) and `assets/logo.png` (600x600 raster, white gap rings baked in). [kept]
- Wordmark: lowercase `zirv` in the display face (Schibsted Grotesk 800, 23px, tracking -0.025em) beside a 30px mark, 10px gap, in header and footer. [settled] (source: mock `.brand`). Changed from Archivo bold at 22-26px; the mark does not change.
- Clear space: the inner hexagon's width. Minimum size: mark 22px on screen, 48px tall in a 1080p film. [kept]
- Allowed backgrounds: now `--paper`, `--paper-2` and `--night`. Use the SVG on paper (the gradient's cyan end is low contrast on paper; a logo is exempt from text contrast, and the mock does this). Use the PNG only on night grounds, because its white rings are baked in. [changed; settled by what the mock does]
- One live exception to "never rotate": in the site header the mark turns -30deg on hover with `--ease-spring` over 420ms; no other rotation, no other mark animation. The mark's three hexagons also serve as the verdict glyph (section 8). [settled] (source: mock `.brand:hover .mk`)
- Misuse: never stretch, recolour outside the palette, add shadows or glows, outline, rebuild the wordmark in another typeface, or place on a busy ground. [kept; "light ground" removed from the misuse list]

## 5. Colour
Two grounds: a cool light "paper" for reading and "night" panels for terminals, the benchmark band, install and footer. Values are oklch in tokens.css; hex below is the sRGB conversion, contrast is WCAG 2.x computed 2026-10-09. [settled] (source: mock `:root`)

| Role | Token | Value | Use | Contrast |
| --- | --- | --- | --- | --- |
| page | --paper | #f6f8fa | page ground | -- |
| band, well | --paper-2, --paper-3 | #eef1f5, #e3e8ed | alternate band, code wells, inline code | -- |
| hairline | --line, --line-2 | #d8dde2, #bdc3ca | rules, control borders | 1.7:1 for line-2: decorative, never meaning alone |
| ink | --ink | #0f1621 | text, one weight of "black" | 17.0:1 on paper, 16.0:1 on paper-2 |
| ink 2 | --ink-2 | #48505c | body, secondary | 7.6:1, 7.2:1 |
| ink 3 | --ink-3 | #626a73 | labels, meta | 5.2:1, 4.9:1 |
| night | --night, -2, -3 | #0b1729, #122136, #1e2f44 | dark panels, raised, chips; primary button fill | -- |
| night line | --night-line | #2e3d51 | hairline on night | decorative |
| night ink | --night-ink, -2, -3 | #ecf1f5, #abbdcb, #889bab | text on night | 15.7:1, 9.3:1, 6.3:1 on night |
| cyan | --cyan | #27e3fd | zirv's own signal on night: prompt, active tab, "better" bars | 11.5:1 on night |
| cyan ink | --cyan-ink | #006a94 | cyan as text, link underline, marks on paper | 5.7:1 on paper |
| focus | --focus | #007cbb | focus ring on paper (cyan on night) | 4.3:1 |
| advise | --advise / -g / -t | #f8bd40 / #cd8800 / #8f5300 | on night / glyph on paper / text on paper | 10.5:1 on night; 2.8:1 glyph; 5.8:1 text |
| compact | --compact / -g / -t | #f68c36 / #d45e00 / #a53e00 | same roles | 7.4:1; 3.6:1; 6.0:1 |
| restart | --restart / -g / -t | #f75e54 / #d02c2a / #ac1a1c | same roles | 5.7:1; 4.8:1; 6.7:1 |

- Proportion: paper and ink carry the page; night panels give each page a few dark surfaces; cyan marks zirv's own signal; warm colours appear only where zirv intervenes or asks (a verdict, an approval, a worse-than-baseline bar). No decorative warm colour. [settled] (source: design-note.md "Type, palette, system")
- Verdict colours always travel with their word and a glyph. The `-g` glyph colours of advise (2.8:1) fall below 3:1 for non-text, so a glyph in `-g` is never the only cue and always sits beside its word in `-t`. [settled as a rule; contrast figures computed]
- Healthy has no colour: it is the unlit mark, its strokes `--line-2` on paper and `--night-line` on night; only the word "healthy" is set in `--ink-2` (`--night-ink-2` on night). The previous green is dropped on the web. [changed]
- Verdict thresholds: score 40 or more advise, 60 or more compact, 80 or more restart; at the token ceiling a score of 60 or more escalates to restart (source: README, "Verdicts"). The 2026-10-08 guide's bands 65 and 85 were from the stale site and are corrected. [settled by README]
- Terminal renders carry zirv's own palette: foreground #d5d9e0 on #15181e, in a window body of #101216. These are the real dashboard colours and are not remapped to the tokens. [settled] (source: render/README.md)
- Never: cyan as a large field on paper; verdict colours as decoration; pure black or white grounds; the Zirv Fitness indigo; a gradient anywhere except the mark. [kept, adapted]
- Gradients: the only gradients are the logo's and the usage bar fill in the pacing instrument (`oklch` blend from a darker cyan to `--cyan`). [settled] (source: mock `.ubar .f`)
Changed: dark-only palette (#080d13 ground and the old ink, accent #22d9f5, healthy green, four verdict colours) replaced by paper and night; compact and restart now carry separate colours with the mark; the "light theme: none" rule is gone (the page is light by default, with night panels); no dark-mode variant of the page is defined.

## 6. Type
- Families: Schibsted Grotesk for display and body (weights 400 to 900, variable, italic available); Fragment Mono for code, labels, keycaps and data; Cascadia Mono only inside terminal renders (frames keep 1ch glyph widths; the mock requests the full font with `&text=` so box-drawing, braille, block and geometric glyphs arrive, and wraps the three glyphs no font carries, ⚑ ✉ ✻, in a 1ch box). Fallbacks: Helvetica Neue, Helvetica, Arial; SF Mono, ui-monospace, Menlo, Consolas. [settled] (source: mock `--sans`, `--mono`, link tag)
- Display: two lines, the second in italic 400 and indented 0.95em; roman 820 weight, size clamp(2.75rem, 1.05rem + 6.2vw, 6.6rem), line-height 0.93, tracking -0.036em, balanced wrapping. Roman 820 against italic 400 is the signature contrast. [settled]
- Descender fix (rule 6): a display line that is clipped, revealed or `overflow:hidden` gets `padding-bottom: 0.24em` and an equal negative margin (`--descender-pad`), so g, y, p and j are never cut; with line-height 0.93 this is mandatory. Verify at the smallest and largest viewport and in the reveal animation. [settled] (source: mock `.display .clip`)
- Scale: h2 clamp(2.1rem, 1.25rem + 2.7vw, 3.6rem) / 760 / 1.02 / -0.032em; doc h1 clamp(2.1rem, 1.5rem + 1.8vw, 3rem) / 780; doc h2 27 / 720; h3 21 / 700; lede clamp 19 to 21.5px; body 18px / 1.6 (docs 17px / 1.68, 74ch); benchmark numerals clamp(4.4rem, 2.2rem + 6.6vw, 9.6rem) / 820 / 0.88 / -0.05em with a lowercase italic lead-in. Mono labels 13.5px; code 14px. [settled]
- Rules: numbers in tables and counters use tabular figures; commands, paths and versions in mono, no ligatures; section labels are mono sentence case led by a small hexagon, not uppercase. [settled] (source: mock `.sec-label`, `font-variant-ligatures:none`)
- Licensing: both families are open-licence Google Fonts. Self-hosted files are not yet in `assets/fonts/`; the previous Archivo, IBM Plex Sans and JetBrains Mono files remain there unused until replaced. Open item.
Changed: Archivo, IBM Plex Sans and JetBrains Mono replaced by Schibsted Grotesk, Fragment Mono and Cascadia Mono (terminal only); the uppercase mono eyebrow is retired for sentence-case mono labels; scale replaced; the display descender rule is new.

## 7. Layout and geometry
- Grid: 12 columns, gutter `clamp(16px, 4.4vw, 64px)`, maximum width 1320px, column gap 32px, section padding clamp(64px, 8.5vw, 136px). Breakpoints 1240, 1100, 1000, 900, 860, 760, 640, 560, 520. [settled] (source: mock)
- Sections: surfaces alternate (paper, paper-2, a night band) with a hairline between; each section has a label, a title, an aside of one or two lines. Landing order: arrival with the real dashboard bleeding into the next band; scripts; sessions; workflows; safety; long runs; measured; install. [settled] (source: design-note.md "Landing, in order")
- Hero: headline, then lede left with install and actions right, then the terminal window across the full container, overlapping the next band by 120px (72px below 1100). [settled]
- Radii: 4 to 8 for controls and code, 12 for buttons and wells, 14 to 20 for panels, 999 for chips. Hairlines 1px. [settled]
- Elevation: surface steps and hairlines first. A soft wide low-opacity shadow only on the hero terminal window, popovers and the calculator; a 1px inner top highlight on night panels; a 2px lift on button hover. No glow, blur or glassmorphism. [settled; changed from "no shadows"]
- No coloured left-border callouts and no accent-bar boxes (rule 3). Blockquotes and notes are plain: hairline top rule or `paper-2` well, never a coloured bar. Conflict to resolve: the mock's docs `blockquote` (3px `--cyan-ink` bar on a tinted well) and its search-result, doc-nav and table-of-contents "current" markers (2px cyan-ink inset or border) use a coloured side bar. The guide forbids the blockquote bar. The current-item marker on a nav or TOC row is allowed as a position indicator, not a box; [proposed] pending the operator's confirmation.
- Density: spacious on the landing, dense in tables and docs. [settled]
- Removed: the faint hexagon motif behind hero content. It risks the rejected "hive" reading (rule 1). [changed]

## 8. Components and imagery
- Buttons: min-height 54px, radius 12, 18px semibold. Primary: night fill, night ink. Quiet: transparent with a `--line-2` border. Cyan fill (`--cyan` on `--night` text) only on night panels, for the selected state. Hover lifts 2px and the arrow moves 3px; pressed is scale 0.98. [settled]
- Command line: a night bar with a cyan `$`, the command in mono, a Copy button that confirms with "Copied" growing from the button. Install text is a real install command, never invented. [settled]
- Keycaps, chips, tabs: keycaps are night-2 with a 3px bottom border and carry zirv's real `Ctrl+A` chords; segmented controls and tabs sit in a night-2 track with a sliding `--cyan` indicator. [settled]
- Tables: hairline rows, header in paper-2, row hover to paper-2; yes and no are a filled and an outlined dot with their word, depth is a bar of small ticks. [settled]
- Lifecycle track: eight steps on a line; gates are a short vertical bar, steps a ring; the gates named are real (artifacts, `zirv test changed`, reviewer seat, `zirv verify`, deploy tier). [settled]
- Safety ladder: three rungs, runs silently / asks / refused, from README's shipped posture lists, marked by an outlined dot, an advise dot and a restart dot, each with its word. [settled]
- Verdict glyph: the mark's three hexagons lit ring by ring (healthy none, advise innermost, compact two, restart all three), in the verdict colours. [settled] (source: mock `.hx`)
- Benchmark band, as a pattern: README's three figures as display numerals with the paragraph verbatim; then a full-bleed night band charting every task group of the latest round (cost, time and judge side by side) from the results write-up's table, one shared scale, filled bar = significant and outline = within noise, `--cyan` better and `--advise` worse, both footnotes, the 15 s intake-timeout note, a Jev off (default) / Jev on toggle that carries the #869 correction, and links to the write-up and the protocol. The setup is stated beside it: 78 runs on Claude Sonnet 5, against Claude Code with the superpowers plugin (v6.4.1), a blind Claude Opus judge not told which setup produced the work, paired bootstrap 95% CI. Figures: about 20% cheaper on ten large coding tasks, up to 42% on a 9-step session, 9-11% higher rated; hidden-test pass rates matched and wall-clock speed was on par overall (README); round 7 shows XL time +19% worse for Jev off (significant; one ~15 s intake timeout, per the results write-up), stated beside the cost and quality figures, never omitted. Section label: `README.md: what wrapping costs (measured)`. Never show a figure without its denominator, marker and source; never pick the best subset. [settled] (source: mock, design-note.md, README section "What wrapping costs (measured)")
- Imagery is the product, real and complete: the dashboard as a real render (command bar `~/project $ zirv chat`, size label "orchestrator view · 120x36", caption naming it a real render with example data), real script files and real `--help` output. Frame a (orchestrator view with a force-push approval pending) and frame b (after "allow") are byte-for-byte renders; answering with `y` or the card button swaps them, and reset restores. Below a 640px terminal width the window shows frame d, zirv's own 72x40 compact AGENT TREE page, scrolling sideways. [settled] (source: design-note.md "Refinement 3", render/README.md)
- No illustration, stock photography or 3D. Alt text describes the content of the render or file. [kept]
- Icons: the nested hexagon, plus text glyphs (`$`, `✓`, `○`, `⚑`, `✉`, `→`, `↗`); terminal glyphs and their colours stay as in `src/style.rs` and `dash/ui.rs`. No general icon set. [kept]
Changed: the `●●●` terminal-mock chrome stays as window dots; terminal renders are now real ratatui renders, not drawn; the benchmark pattern, tables, track, ladder and verdict glyph are new.

## 9. Interface motion
- Response: every link, button, tab, keycap row and table row answers hover (120ms linear), press (60ms, scale 0.98) and focus-visible. [settled]
- Cause: answering the dashboard request resolves it in place; the tab indicator slides (320ms `--ease-move`); "Copied" grows from the button; doc pages swap with a view transition (120ms out, 260ms in, 8px rise). [settled]
- Arrival: headline lines rise 130% over 720ms with `--ease-out-strong`, 90ms apart; the hero copy and the window rise 14px with fade over 520 and 640ms. [settled]
- Ambient: braille spinners turn and the waiting card's border breathes, a few seconds only, as README documents for the dashboard. [settled]
- Easing: `--ease-out` (0.22, 1, 0.36, 1) for arrivals, `--ease-move` for travel, `--ease-in` for exits; `--ease-spring` only for small controls (toggle, header mark, verdict bump). No springs on layout, text or page transitions. [changed from "no springs"]
- Reduced motion: no rise, spinner, breathing, hover lift or transitions; the final state at once (durations 1ms). [settled]
- Terminal: rollover pulse 1.6s, toast 5s, row flash 900ms, reduced motion honoured. [kept]
Changed: from "two keyframes, no transitions" to a full response, cause, arrival and ambient layer; spring allowed for small controls.

## 10. Accessibility commitments
- Contrast: text 4.5:1 and UI 3:1 on the surface it sits on, using the table in section 5. The advise glyph is the single sub-3:1 case and is always beside its word. [settled]
- Focus: a visible 2px ring, offset 3px: `--focus` on paper, `--cyan` on night; the terminal window body shows an inset cyan ring. Skip link first on every page. [settled]
- Targets 44px minimum on touch; keycaps and chips at least 32 to 40px. [kept, adapted]
- State is never colour only: verdicts, statuses and chart bars carry a word or a fill pattern (filled for significant, outline for within noise). [settled]
- `prefers-reduced-motion`, `prefers-contrast: more` (terminal dim text steps up) and `forced-colors` (hexagon strokes and controls take system colours) are honoured. [settled] (source: mock)
- Film: captions for every spoken line. [kept]
Changed: ring colour and contrast table; forced-colors and prefers-contrast are new.

## 11. Film motion
Timings, eases, transitions and the verdict-ladder beat are kept from 2026-10-08. [kept] Adaptations to the new look are [proposed], not read from the approved mock:
- Ground `--night` instead of the old #080d13; hairlines `--night-line`; text `--night-ink`; cyan `--cyan` as the single accent; terminal scenes use real renders on #15181e.
- Statements in Schibsted Grotesk 820 with the 0.93 line-height and the descender padding; labels in Fragment Mono; terminal text in the renders' own face.
- The verdict ladder uses the verdict glyph (rings lit in order) and the three warm colours; healthy is the unlit mark. Bands 0-39, 40-59, 60-79, 80+ per README. Verdict colour changes remain hard cuts.
- End card: mark with lowercase `zirv` in the display face, the real install line, hold 1.5 s. Silent by default.
- Terminals and dashboards in film are real renders, example data labelled (rule 2).

## 12. Don'ts
- No signal-box, hive, orbiting-session or hex-geometric hero; no small 3D graphic; no themed metaphor. (rule 1)
- No terminal or dashboard drawn from memory or fabricated; no example data left unlabelled. (rule 2)
- No coloured left-border callouts or accent-bar boxes. (rule 3)
- No rot-first headline; no section that gives one feature the weight of the product. (rule 4)
- No benchmark figure without source, significance marker and caveat; no cherry-picked subset. (rule 5)
- No clipped descenders in display type. (rule 6)
- No hype vocabulary, emoji, exclamation marks; no glow, blur, glassmorphism or decorative gradient; no pure black or white grounds.
- Do not use Zirv Fitness colours, fonts or copy; do not claim a number the repo does not measure; do not re-mark the logo.

## 13. Open items
- Fonts: Schibsted Grotesk and Fragment Mono need self-hosted woff2 files and licence texts in `assets/fonts/`; the mock loads them from Google Fonts. The old font files should then be removed.
- The mock's docs blockquote and "current item" markers against rule 3 (section 7).
- The mock lives in the design session's scratchpad; the render pipeline (patch and README) is not yet in the repo.
- The site shipped version (v4.58.0 in the mock header) against the live cli.zirv.io, which still shows an older release.
- Film adaptations in section 11 and the contrast figures are [proposed] until the operator reads them.

## Changelog
| Date | Change | Reason | Source |
| --- | --- | --- | --- |
| 2026-10-08 | Guide established (draft) from cli.zirv.io CSS/screens, src/style.rs, dash/ui.rs, README; fonts self-hosted from @fontsource 5.3.0 | operator asked for the brand guide | brand-evidence/evidence.md, wt-design-skills |
| 2026-10-08 | All proposed items approved by the operator: vector master of the mark, logo rules, canonical accent, dark only, terminal-to-web colour mapping, four verdict colours, interface motion tokens, film motion, focus ring, line heights, voice rules, separation from Zirv Fitness | operator reply "approve all" | operator approval 2026-10-08 |
| 2026-10-09 | New visual direction replaces the dark-only guide, logo unchanged: paper and night grounds (oklch), Schibsted Grotesk / Fragment Mono / Cascadia Mono (terminal), 12-column 1320px layout, new component and motion layers, descender fix, verdict colours as warm steps with the hexagon glyph (healthy unlit), README thresholds 40/60/80 replace 65/85, benchmark and real-render rules, the six design-session rules. Replaces: every colour, type, layout, elevation and motion token of 2026-10-08 except the logo gradient, film timings and spacing-independent film tokens | operator approval of the opus mock (refinement 3, "much better") and design-session feedback 2026-10-08/09 | mocks/opus/index.html, design-note.md, captures/v5, render/README.md, README.md |
