# Typography

Type is the fastest way a page tells the visitor whose world they are in. A
habitual face (Inter, system-ui, Roboto) says "template" before a word is read.

## Choose type from the world

1. Ask three questions about the subject, and write one line for each:
   - Era: when does its visual language come from? (1950s survey charts,
     1970s transit signage, a 1990s terminal, today's lab report)
   - Material: where do letters physically live there? (engraved brass,
     stencil on a hull, chart labels, receipt paper, LCD, letterpress, enamel
     signs, handwritten logbooks)
   - Voice: how does the product speak? (calm expert, urgent operator,
     patient guide, playful maker)
2. List three candidate families per role (display, text, optional mono)
   with a one-sentence reason tied to those answers. Drop any candidate you
   would pick for any product: that is habit, not choice.
3. Pair by contrast of structure, not only weight: serif with sans, condensed
   with normal width, mono with humanist. Two families is the norm; a third
   only as a mono for data. One family with width or optical-size axes can
   cover both roles.
4. Operate surfaces need a workhorse text face: tabular figures, distinct
   1/l/I and 0/O, medium-large x-height. Personality goes into the heading
   face, the numerals or a single display moment, never into dense body text.

## Families by character -- examples, not a shortlist

Models converge on any name they are shown. Treat each family below as a
pointer to its neighbourhood; if your first pick appears here, look at two
alternatives before committing. All are SIL Open Font License and available
as @fontsource packages.

- Instrument and engineering: B612 and B612 Mono (drawn for cockpit
  displays), Overpass (highway-sign lineage), Barlow and Barlow Condensed,
  Red Hat Mono, Martian Mono.
- Civic and editorial grotesques: Public Sans, Libre Franklin, Schibsted
  Grotesk, Hanken Grotesk, Familjen Grotesk, Instrument Sans.
- Humanist sans: Source Sans 3, Fira Sans, Alegreya Sans, Commissioner,
  Atkinson Hyperlegible (low-vision legibility).
- Geometric: Jost, Outfit, Urbanist, Lexend, Albert Sans.
- Condensed and poster: Big Shoulders Display, Sofia Sans Condensed,
  Antonio, Archivo at width 62-75.
- Wide and technical display: Unbounded, Michroma, Krona One, Archivo at
  width 125.
- Text serifs: Source Serif 4, Literata, Newsreader, Spectral, Crimson Pro,
  Alegreya, Petrona.
- Display serifs: Bodoni Moda, Young Serif, Gloock, Rozha One, DM Serif
  Display, Libre Caslon Display.
- Slab and typewriter: Zilla Slab, Bitter, Aleo, Besley, Courier Prime.
- Mono for code and data: IBM Plex Mono, JetBrains Mono, DM Mono, Fragment
  Mono, Azeret Mono.
- Rounded, only when softness is the world: Varela Round, M PLUS Rounded 1c.

Overused defaults -- use only when the established system already does:
Inter, Roboto, Arial, system-ui, Open Sans, Lato, Montserrat, Poppins, Space
Grotesk, Playfair Display, EB Garamond, Syne, and Fraunces or Instrument
Serif on cream. The detector flags Inter, Arial and system-ui as advisory.

## Scale

- Body size: persuade and read 18-20px; product UI 16px; dense operate
  surfaces 13-14px. Nothing that must be read below 12px.
- Ratio between steps: 1.2 for dense UI, 1.25-1.333 for product pages,
  1.414-1.618 for editorial and persuade pages. At 1.333 from 18px:
  13.5, 18, 24, 32, 42.7, 56.9, 75.8, 101.
- Display: the hero headline is 4-8x body on persuade surfaces -- typically
  72-128px at 1440 wide and 40-56px at 390. One fluid rule covers it:
  `font-size: clamp(2.75rem, 1.6rem + 5.2vw, 7rem)` (about 46px at 390,
  100px at 1440, capped at 112px).
- The detector flags `font-size` of 7rem or more as advisory. Larger type is
  right when type is the hero in your plan: keep it, say why in the report,
  and check real copy wraps cleanly at 390 and 768.
- Size contrast beats weight contrast: adjacent levels differ by at least
  1.25x; the headline is at least 3x body on persuade pages and 1.5-2x on
  operate surfaces.

## Setting

- Line height: body 1.5-1.65 (serif, long measure or light-on-dark +0.05);
  UI text 1.3-1.45; headings 1.05-1.2; display 0.92-1.02, checking that
  descenders and accents (Å, Ø) do not collide.
- Measure: body 60-75ch (`max-width: 65ch`), 45ch minimum on narrow screens,
  captions 35-50ch, headlines 8-16ch so they break into deliberate lines.
- Tracking by size: 64px and up -0.02 to -0.035em; 32-63px -0.01 to
  -0.02em; 16-31px 0; 12-15px +0.005 to +0.01em; all caps +0.06 to +0.1em.
  Never tighter than -0.04em: readability drops and the detector flags
  -0.05em.
- Weights: 2-3 per family. Text 400, emphasis 600. Display either heavy
  (700-900) or light (200-300) at very large size -- the contrast with body
  is the point; a page where everything is 700-800 has no hierarchy.
- Wrapping: `text-wrap: balance` on headings, `text-wrap: pretty` on
  paragraphs, `hyphens: auto` with a correct `lang` on narrow columns.
  Control headline breaks with a max-width in ch, not `<br>`.
- Numerals: `font-variant-numeric: tabular-nums` for tables, timers, prices,
  scores and any column; lining figures in UI; old-style figures in long
  serif prose if the face has them; `slashed-zero` for codes and IDs.
- Case: all caps only for labels of 1-3 words, at 12px or more, tracked
  +0.06em. Column headers and section labels in sentence case scan faster.
- Optical size: `font-optical-sizing: auto` on families with an opsz axis
  (Bodoni Moda, Literata, Newsreader, Source Serif 4) so display cuts get finer
  detail and text cuts get sturdier strokes.

## Self-hosting and loading

Why: `zirv frontend render` blocks every external host, so a font from any
hosted font CDN renders as fallback in the captures and the review judges
type you did not choose. Self-hosted fonts are also faster (no extra
connection) and send no visitor data to a third party.

- Project with a bundler: `npm install @fontsource/<family>` (or
  `@fontsource-variable/<family>`) and import only the weights you use.
- Static page: in a scratch folder run `npm pack @fontsource/<family>`,
  extract it with `tar -xzf`, and copy the needed
  `package/files/<family>-latin-<weight>-normal.woff2` next to the page, for
  example into `fonts/`. npm needs network access to registry.npmjs.org.
- Declare each file:

```css
@font-face {
  font-family: "Barlow Condensed";
  src: url("fonts/barlow-condensed-latin-600-normal.woff2") format("woff2");
  font-weight: 600;
  font-style: normal;
  font-display: swap;
}
```

- `font-display: swap` for text and headings (the detector flags a missing
  policy). Preload the one or two files used above the fold:
  `<link rel="preload" as="font" type="font/woff2" href="fonts/..." crossorigin>`.
- Subsets: `latin` covers Danish, Norwegian, Swedish, German, French; add
  `latin-ext` for names like Łódź or Kraków.
- Budget: at most 4 files and about 150 KB of woff2 before first paint;
  prefer one variable file over four static weights.
- Cut layout shift with a metric-matched fallback: a second @font-face on a
  local font with `size-adjust`, `ascent-override` and `descent-override`
  tuned so line breaks match.
- If no font file can be obtained (no network), use a deliberate local stack
  with character -- for example `"Iowan Old Style", "Palatino Linotype",
  Palatino, serif` -- and report the gap. Never link a CDN as a fallback.
  Remember the render machine may lack these fonts too; check the capture.

## Operate surfaces

- Body 13-14px at line-height 1.35-1.45; row identifiers 600; secondary lines
  muted but still 4.5:1.
- Numbers right-aligned with tabular numerals; units in a smaller muted span
  after the number; decimals aligned.
- Dates: one absolute format everywhere ("Thu 8 Oct, 14:59"), relative time
  as the secondary line ("in 6 h"), time zone stated once in the header.
- Mono only where characters are compared one by one: codes, IDs, language
  pairs, hashes. Mono for a "technical vibe" just lowers legibility.

## Cliches and the better move

- One headline word in the accent colour -> let type carry emphasis through
  scale, weight, a line break or italic; give the colour to the artefact.
- A tracked all-caps eyebrow above every heading -> delete it, or turn it
  into a label that carries information (a step in a real sequence, a unit).
- Default system or Inter type -> a family whose letterforms you can defend
  in one sentence about the world.
- Every heading the same heavy weight -> a scale with real steps and one
  extreme display moment.
- Mono everywhere for a technical feel -> mono only for data compared by
  character.
- Gradient-filled text -> solid ink; if the headline needs more, change its
  size, face or placement.

Informed by impeccable (Apache-2.0), the Hyperframes skills (Apache-2.0) and
an Apache-2.0 frontend-design agent skill; ideas re-derived, not copied.
