# Video composition

A video frame is not a web page. It is seen once, at a distance, often on a
phone or as a small GIF, with no scroll, zoom or second read. Web habits --
UI-sized type, small cards floating in empty space, everything centred on a
dark gradient -- produce "a web page filmed". Translate them: type 2-3x
larger, borders 2x thicker, padding 3x larger, one idea per frame.

## Frame and safe areas

- 1920x1080 for 16:9, 1080x1920 for 9:16, 1080x1080 for square.
- Keep text inside title-safe: at least 96px from the left and right and 54px
  from the top and bottom at 1920x1080. Working margins of 120-160px look
  intentional.
- For 9:16 social placements keep key content out of the top 12% and the
  bottom 20%, where platform UI sits.
- `render` cannot scale down, so the canvas size is the output size. If a GIF
  will be viewed small, design every label at 2.2% of frame height or more
  (24px at 1080p; 9px on a 720x405 canvas, the one floor) and check a snapshot at the size
  it will be seen.

## Type at video scale (1080p)

Sizes are a share of frame height, with 1080p pixels as the worked example
and the same shares scaled to a 720x405 GIF canvas (multiply by 0.375).
Scale margins and borders the same way.

| Role | Share of height | 1080p | 720x405 |
| --- | --- | --- | --- |
| Statement (1-4 words) | 11-18%, 60-80% of frame width | 120-200px | 45-75px |
| Headline | 6-11% | 64-120px | 24-45px |
| Hero numeral | 17-30% | 180-320px | 68-120px |
| Body, callouts | 2.6-4% | 28-42px | 11-16px |
| Labels | 2.2% or more (the floor) | 24px or more | 9px or more |

- At most 6-8 words per line and one statement per beat.
- Weights: headlines 700-900, or 200-300 at statement size for contrast;
  body 400 (300 only at 36px or more on a light ground). Contrast of about
  300 against 900 reads instantly.
- Tracking: display -0.02 to -0.05em; caps labels +0.04 to +0.1em. Line
  height for display 0.9-1.0.
- Two families: an expressive display face and a text or mono face, chosen
  by the subject's era, material and voice (`zirv skill read frontend-craft
  references/typography.md` lists open-licence families by character). One
  expressive face per scene.
- Embed both from local woff2 files with `@font-face` (for example in
  `assets/fonts/`). A font named without `@font-face` silently renders as a
  fallback. The scaffold's Inter and system stack are placeholders: replace
  them.
- Numbers: `font-variant-numeric: tabular-nums` so counters do not jitter.
- Time is hierarchy: the first thing to appear is read as the most
  important.

## Three layers in every scene

- Background: atmosphere -- a tinted gradient with grain, a light source or
  glow, ghost type at 3-8% opacity, hairlines or a grid at 12-25% opacity.
  Two to five persistent decoratives, moving only under the ambient rule in
  motion-principles.md.
- Midground: the subject -- the product's artefact, a diagram, the thing
  the scene is about.
- Foreground: type, callouts, pointers, numbers.

Give each scene at least two focal points with one clearly dominant. A scene
with one small element in the middle of a dark field is dead space.

## Layout

- Anchor to edges and a baseline grid: type set left at the margin,
  artefacts large and cropped by the frame edge (cropping implies scale and
  continuation). Centre only a single statement, and even then consider an
  asymmetric lock-up.
- UI is a prop, on screen for at most a third of the film
  (concept-and-world.md). When it appears, crop to the 3-6 lines that
  matter and scale them 2-3x (terminal text 32-44px). Move between regions
  with the camera instead of shrinking a whole window into the frame.
- Vary the composition beat to beat: a full-frame statement, an object
  cropped by two edges, type wrapped around an object, a split. The same
  split in every scene is a slideshow.
- Borders 2-4px, padding 60-140px, radii 12-24px.
- Real content from the brief in every artefact: the actual commands,
  scores, names and states. No lorem, no fake chrome clutter.

## Colour

- Declare background, foreground and one accent before anything else.
- Tinted neutrals: no `#000` or `#fff`. A dark ground sits at OKLCH L
  0.14-0.22 with chroma 0.02-0.04, lit from one side.
- One accent hue for the subject's key state. Add a second hue only when it
  carries meaning (before against after, warning against safe).
- "Muted is fine. Flat is not." A flat navy field with small grey text is
  flat; a deep ground with a light source, grain and one hot accent is
  muted and alive.
- Text holds 4.5:1 against what is behind it in every frame, including mid
  transition.
- Banding is an H.264/MP4 concern: 8-bit H.264 bands smooth dark gradients
  into visible steps. For an MP4-only deliverable, overlay grain at 4-8%
  opacity (an SVG `feTurbulence` tile or a noise PNG), let gradients span at
  least 0.08 of lightness, and avoid a full-frame linear gradient on
  near-black.
- A GIF carries no grain or noise overlay: its 256-colour palette dithers
  anyway, and per-frame noise defeats palette and frame-to-frame
  compression. Measured: a static 4%-opacity `feTurbulence` grain took a
  12 s, 1920x1080, 15 fps GIF from 9.0 MB to 63 MB; a slow camera push added
  only about 0.4 MB. Check the GIF's size after rendering (about 10 MB or
  less) and cut grain, noise and full-frame textures first.

## Show the subject's world

Each scene's hero is an object from the subject's world or a line of type
that performs, not a screen. Pick the most characteristic object and make
it big. Metaphors work when built from the world's own objects (paper that
piles up, a lock that clicks, a jar that fills), not from generic shapes;
concept-and-world.md has the method.

## End card

- It is built, not shown: the carrier from the final beat becomes the
  wordmark, letters land or strokes draw, and the line lands with weight
  on a beat (concept-and-world.md).
- Wordmark in the embedded display face at 180-280px (30-45% of frame
  width); the line at 56-80px, 7 words or fewer.
- Lock it up asymmetrically on a margin, or compose it with the final
  object. A centred wordmark with an accent dot and an underline rule on
  a radial gradient is the default to avoid.
- After the build, a final settle of 1-1.5 s ending on a still poster
  frame. For a looping GIF, either settle or make the last frame flow into
  the first.

## Proof frames

Snapshot every 0.5 s plus the midpoint of every transition and the
signature move in flight. Judge them at full size and at the size they will
be watched: type size against the table above, empty space, contrast, the
pause test, and the density check (neighbouring frames differ
meaningfully).

## Cliches and the better move

- A web page filmed (UI-sized type, small cards in dead space) -> crop,
  scale up 2-3x, and push the camera.
- Everything fades in and dims out -> a verb per element.
- Dark navy gradient as the whole style -> a palette from the subject's
  world, with light and grain.
- Centred wordmark with an accent dot -> an end card built from the film's
  carrier.
- Text left, panel right in every scene -> one world, camera travel,
  compositions that change on the beat.
- Identical card grids, neon cyan and purple, gradient text, left-edge
  accent stripes -> one artefact, one accent, solid type.

Informed by the Hyperframes skills (heygen-com/hyperframes, Apache-2.0);
ideas re-derived, not copied.
