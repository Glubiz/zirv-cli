# Critique

One bounded pass that turns captures into a ranked, concrete fix list. Use
it on the plan before building and on the render after building. A clean
detector is a floor, not a verdict.

## 1. Gather the evidence

- All fresh `zirv frontend render` captures (390, 768, 1440).
- A full-height capture at 1440 and 390 for any page longer than one
  screen (recipe in layout-and-composition.md).
- Captures of loading, empty and error states (query parameters, toggles
  or fixtures), and of focus: tab through the primary path.
- The plan: subject, world, type pair, palette, layout sketch, signature.

Look at everything side by side, first at thumbnail size, then at 100%.
Compare against the plan, not against what you remember building.

## 2. Five-second read

For each first viewport, write three answers without reading body copy:
what is this, who is it for, what do I do next? If any answer needs the
paragraph text, the hierarchy failed.

## 3. Interchangeable-product test

Mentally swap in a competitor's name and logo, then an unrelated product's.
If the page still works, it is anonymous. The hero artefact, the type, the
palette and the composition must belong to this subject's world. Second
question: is this what a bare "make a page for X" request would produce?
If yes, it is the model average, however tidy.

## 4. Squint test

Blur the capture by about 8px. You should see one dominant element, a clear
second, then everything else. Equal weights everywhere means no hierarchy;
three things shouting means none of them is heard.

## 5. Timidity is a defect

Correct but quieter than the plan is a finding. Check specifically:

- Fallback type in the capture where the plan names a face: the font is not
  loading (hosted CDN blocked, wrong path, missing @font-face).
- The display size shrank to "safe", or every heading has the same weight.
- The atmosphere, material or illustration from the plan is missing, or the
  whole page sits on one pale tint.
- The signature moment is absent or reduced to a decoration.
- The hero shows a generic device mockup or abstract shape instead of the
  world's artefact doing its job.

Fix toward the plan, not away from it.

## 6. Cliche scan

Each one is a finding unless the brief or the established system earns it:

- One headline word in the accent colour -> emphasis from scale, weight,
  line break or italic.
- Tracked all-caps eyebrows and column headers -> sentence case, or a label
  that carries information.
- Hero plus three cards; identical rounded cards with soft shadows -> the
  artefact as hero; compositions derived from content.
- Hairline-separated text-left, list-right sections repeated -> a shape per
  section.
- A flat pale tint on every band -> surfaces by role, one immersive band.
- System or Inter type by habit -> type chosen by the world.
- Purple-blue gradients, neon glow, glass, gradient text -> a palette from
  the world, solid material surfaces.
- Status pills everywhere; a red edge stripe on an alert card that repeats
  table rows -> urgency grouped in the data, status as icon plus word.
- A heavy select on every row -> an inline assign action with a combobox.
- Everything centred -> a strong edge.
- Invented metrics, testimonials, counts or logos -> remove; demonstrate.

## 7. Score the touched dimensions

Score 1-5; 4 means "a good studio would ship it", 5 means "this could only
be this product". Anchors for 3 and 5:

- product-specificity: 3 right content in a generic frame; 5 artefact, type
  and palette belong only to this subject.
- user-journey: 3 the primary path exists but competes; 5 arrival, job and
  success are one obvious path with recovery visible.
- hierarchy: 3 readable, several elements shout; 5 one dominant per
  viewport, passes the squint test.
- system-coherence: 3 mostly consistent with stray values; 5 every size,
  colour, radius and space comes from the tokens.
- typography: 3 legible but habitual faces or a flat scale; 5 faces chosen
  by the world, real scale contrast, 60-75ch measure, tuned tracking and
  numerals.
- color-contrast: 3 passes AA but timid or evenly spread; 5 dominant plus
  sharp accent from the world, AA everywhere including over imagery,
  status not by colour alone.
- layout-rhythm: 3 tidy grid with one section shape repeated; 5 layout
  derived from content, varied pace, deliberate asymmetry.
- interaction-affordance: 3 controls work but look alike or hide; 5 actions
  look actionable, feedback within 100 ms, keyboard path complete.
- state-completeness: 3 loading, empty and error exist but are generic; 5
  every state explains itself and offers the next step without layout
  shift.
- responsive-composition: 3 the desktop layout shrinks; 5 each width is
  recomposed and the signature survives at 390.
- accessibility: 3 basics with gaps; 5 semantic, labelled, focus-visible,
  reduced motion, zoom and contrast verified.
- content-clarity: 3 accurate but generic copy; 5 specific, in the world's
  vocabulary, actions say what they do, nothing invented.
- resilience: 3 breaks on long strings or slow data; 5 long, short and
  localized content, slow fonts and failures all hold.

## 8. Operate surface checks

- Can the user make the screen's main decision without scrolling at 1440?
- Is urgency visible in the data's order and grouping, not in a duplicate
  panel?
- Are numbers right-aligned in tabular numerals, dates in one format?
- Is the main action reachable by keyboard, with focus returning sensibly?
- Is half the canvas empty at 1440? Then a region that serves the job is
  missing.

## 9. Floor checks

Tab through: focus visible everywhere, order logical. Spot-check contrast on
muted text and on text over atmosphere. 200% zoom without loss. The longest
real string in every label. Reduced motion. Each state. Fix every blocking
detector finding; give a reason for each advisory one you keep.

## 10. Write the fix list

Each finding: where (viewport, region, selector), what is wrong as seen,
why it matters to the user or the plan, and the concrete fix (a property and
value, or a structural move). Rank by impact; batch them; apply in one
round; render once more; stop. No open-ended polish loops, and no new
direction in the fix round -- if the plan itself was wrong, say so.

Informed by impeccable (Apache-2.0), the Hyperframes skills (Apache-2.0) and
an Apache-2.0 frontend-design agent skill; ideas re-derived, not copied.
