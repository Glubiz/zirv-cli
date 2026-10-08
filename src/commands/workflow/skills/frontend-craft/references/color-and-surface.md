# Color and surface

A palette sampled from the subject's world, with one dominant colour and one
sharp accent, beats a timid, evenly spread palette every time. Surfaces carry
the narrative: a page with one pale tint on every section reads as a
template however good its type is.

## Build the palette from the world

1. Name 3-5 colours that physically exist in the subject's world: for sea
   kayaking, chart-paper buff, chart-ink blue, slate water at dusk, buoy
   orange, kelp; for a translation agency, paper, proof-mark red, ink. Pick
   the dominant from that list, not from a UI kit.
2. Structure it as:
   - a neutral ramp tinted toward the dominant hue (about 60% of the area),
   - one dominant colour for large fields: hero band, ink, primary surfaces
     (about 30%),
   - one sharp accent for the primary action and the signature only
     (10% or less),
   - a semantic status set, kept apart from the accent.
3. Write it as CSS custom properties before building anything, so every
   later value comes from the tokens.

## OKLCH construction

Use `oklch(L C H)`: L is perceptual lightness 0-1, so equal L steps look
equal and contrast is predictable; C is chroma; H is hue in degrees.

- Neutral ramp at C 0.005-0.02 on the dominant hue:
  L 0.985, 0.965, 0.93, 0.87, 0.78, 0.66, 0.54, 0.44, 0.34, 0.25, 0.18.
- Never pure #fff or #000. Light paper: L 0.96-0.99, C 0.005-0.015. Dark
  ground: L 0.14-0.22, C 0.01-0.03. Pure black behind light text halates;
  pure white glares.
- Dominant: C 0.06-0.16 at L 0.30-0.55 for ink and immersive bands.
- Accent: C 0.15-0.25, at least 60 degrees of hue away from the dominant,
  or its world-given complement (buoy orange against sea blue).
- Example, sea-slate world:

```css
:root {
  --h: 232;
  --paper: oklch(0.975 0.008 var(--h));
  --ink: oklch(0.24 0.03 var(--h));
  --ink-2: oklch(0.46 0.025 var(--h));  /* about 6.6:1 on paper */
  --line: oklch(0.88 0.012 var(--h));
  --deep: oklch(0.30 0.07 var(--h));
  --accent: oklch(0.70 0.17 55);        /* buoy orange: fills, not text */
  --accent-ink: oklch(0.52 0.16 45);    /* accent as text on paper */
}
```

- Interpolate gradients in OKLCH (`linear-gradient(in oklch, ...)`) so the
  midpoint between two hues does not turn grey.
- Stay inside sRGB by default; enrich only the accent inside
  `@media (color-gamut: p3)`.

## Contrast targets

- WCAG 2.2 AA: 4.5:1 for body text; 3:1 for text of 24px or more (18.66px
  bold); 3:1 for UI component boundaries, focus indicators and chart marks
  that carry meaning. Aim for 7:1 on long reading.
- Rules of thumb in OKLCH, always confirmed with a checker: on light paper
  (L 0.96 or more) text needs L 0.55 or less for 4.5:1; on a dark ground
  (L 0.22 or less) text needs L 0.68 or more. An accent at L 0.70 is a fill
  or large shape on light paper, never small text.
- Text over gradients, grain or imagery: measure against the worst pixel it
  can land on at every breakpoint. Fix with a scrim (a gradient behind the
  text at 40-70% of the ground colour) or move the text.
- Muted text on a coloured surface: derive it from the surface hue (same H,
  shifted L) instead of neutral grey, which looks dirty on colour.
- Dark themes are rebuilt, not inverted: surfaces step up by L 0.04-0.06 per
  elevation level (0.16, 0.21, 0.26), text at L 0.93, accent chroma down
  about 0.02 and lightness up 0.05-0.10 so it does not vibrate.

## Status semantics

- Hues: danger 25-30, warning 70-85, success 140-160, info 235-255. Move
  one if it collides with the dominant (a green brand needs a success mark
  that is not just green).
- Never colour alone (WCAG 1.4.1): pair every status with a word and a
  shape or icon -- filled versus hollow circle, a triangle for warning,
  a clock for due soon.
- Tinted badge when needed: background L 0.94-0.96 at C 0.03-0.05, text in
  the same hue at L 0.40-0.45, C 0.12. In a table a fixed-width column of
  icon plus word scans faster than a field of pills.
- Reserve red for what needs action now. If everything is red or amber,
  nothing is urgent.

## Surfaces with depth

Give each band a surface that matches its role: an immersive hero (deep
dominant with atmosphere), an instrument band (paper or panel holding the
product artefact), a reading band (quiet paper), an action band (dominant or
accent field). Change of surface is how a long page paces itself.

Atmosphere recipes:

- Layered light: a base linear gradient plus 2-3 radial gradients offset
  toward an implied light source, L differing by 0.03-0.08 between layers.

```css
.hero {
  background:
    radial-gradient(120% 80% at 80% 0%, oklch(0.42 0.06 220 / 0.9), transparent 60%),
    radial-gradient(90% 70% at 10% 100%, oklch(0.20 0.05 250), transparent 70%),
    linear-gradient(in oklch, oklch(0.30 0.06 235), oklch(0.18 0.04 250));
}
```

- Grain: a static SVG noise tile over the surface at opacity 0.04-0.08,
  `mix-blend-mode: overlay` on dark grounds and `multiply` on light ones. It
  adds material and hides gradient banding.

```css
.grain { position: relative; isolation: isolate; }
.grain::after {
  content: ""; position: absolute; inset: 0; z-index: -1;
  pointer-events: none; opacity: 0.06; mix-blend-mode: overlay;
  background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='200' height='200'%3E%3Cfilter id='n'%3E%3CfeTurbulence type='fractalNoise' baseFrequency='.8' numOctaves='3' stitchTiles='stitch'/%3E%3C/filter%3E%3Crect width='100%25' height='100%25' filter='url(%23n)'/%3E%3C/svg%3E");
}
```

- Material: paper (warm L 0.96, fibre grain, a faint printed graticule as
  1px lines at L -0.05), a chart (contours, soundings, a compass rose drawn
  in SVG with the palette's inks), an instrument (narrow linear ramps and a
  1px highlight edge), a screen (only when the world is a screen).
- Drawn illustration: SVG built from the world's real geometry -- a
  coastline, stream arrows, a route, a document, a workflow -- in the
  palette's inks. It beats stock imagery, renders offline and stays sharp.
- Real imagery: only photographs the project supplies, with a scrim under
  any text. Never placeholder stock, never generated people.
- Elevation: at most 3 levels. Tint shadows with the surface hue and layer
  them: `0 1px 2px oklch(0.25 0.03 var(--h) / 0.08), 0 8px 24px -4px
  oklch(0.25 0.03 var(--h) / 0.12)`. On dark grounds, raise elevation with
  lighter surfaces, not shadows.
- Borders: 1px at L about 0.08 below the surface, used for structure. Prefer
  spacing and tone shifts to boxing every group.

Caveats:

- `backdrop-filter` and large `filter: blur()` repaint on every scroll frame;
  keep them off large, sticky or scrolling areas.
- Never animate gradients or grain; keep both static.
- Under `prefers-contrast: more`, drop grain and scrims behind text. Under
  `forced-colors: active`, borders and focus must still show.

## Detector findings

`zirv frontend check` flags every gradient (`craft/unjustified-gradient`),
purple-blue gradients, gradient text, coloured glows and decorative blur as
advisory. Advisory means: justify the choice against the plan in your report
("dusk-sea atmosphere behind the hero; grain prevents banding") and remove
only what you cannot justify. Blocking findings, such as missing focus
visibility, must be fixed.

## Cliches and the better move

- A flat pale tint on every section -> vary the surface by role; give the
  page one immersive band.
- Purple-blue gradient, neon glow on near-black -> a palette sampled from
  the world.
- Cream, serif and terracotta by reflex; near-black with an acid accent by
  reflex -> use them only when the world is that.
- Accent sprinkled on links, icons and headings -> accent on the primary
  action and the signature only.
- Grey text on a coloured band -> muted text derived from the band's hue.
- Glassmorphism cards -> solid material surfaces.
- A red left-edge stripe to flag a card -> put urgency into the data: group,
  sort, and label with word plus icon.

Informed by impeccable (Apache-2.0), the Hyperframes skills (Apache-2.0) and
an Apache-2.0 frontend-design agent skill; ideas re-derived, not copied.
