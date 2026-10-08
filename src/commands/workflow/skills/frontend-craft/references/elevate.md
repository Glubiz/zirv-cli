# Elevate

The first complete build is usually competent: right content, clean grid,
sensible palette. It is not yet designed. Elevate is one bounded pass that
turns competent into something a design director would stop on: specific,
alive, with contrast and a voice.

Run it after the first complete build of a new persuade or experience
surface and of a substantial operate surface. Skip it for trivial and
bounded changes. Work from the full-height capture and the first viewports
side by side, then do the five steps below once. Afterwards take the life
captures (critique.md, section 1), critique, fix in one batch, stop.

## 1. Replace the three most generic moments

Go through the page section by section and ask: could this moment appear,
unchanged, on any other product's page? Write down the three most generic
ones in this form:

    moment -> why it is generic -> what only this product could put there

Then redesign each. Typical replacements:

- A three-column feature row -> each feature doing its job in place: a
  small working instrument, a before-and-after, a drawn scene.
- A static hero artefact -> an artefact that responds (drag a handle,
  hover a reading, switch between two real states) or evolves as the
  visitor scrolls past it.
- A call-to-action band (headline left, buttons right) -> the action framed
  by its moment of use: the place, time or object where the visitor will
  actually need the product, drawn large.
- A text-only reassurance section -> a scene: the person on the other end,
  the device that receives the alert, the document that arrives.
- A table that lists -> a table that decides: ordered by urgency, the
  likely next action pre-filled with its reason, time drawn where the data
  is time-bound.
- "Something went wrong" -> what failed, what still works, what to do now,
  in the product's own voice.

A replacement must still be true: it shows what the product really does,
with example data labelled as example. Invention is not elevation.

## 2. Add the life layer

A page where nothing responds reads as a screenshot. Add all five, each
with its reduced-motion path (code in interface-motion.md):

1. Response on every interactive element: hover (on hover-capable
   pointers), press and focus-visible, with physical easing. Count them;
   every link, button, input, row and handle responds.
2. State changes that show cause: what moved goes where it went (View
   Transitions or FLIP), a confirmation grows out of the control that
   caused it, counts tick to their new value.
3. One scroll moment: an artefact that stays in view and evolves as the
   sections pass, or a reveal tied to scroll position. Content is visible
   by default; the effect is an enhancement.
4. Ambient motion from the subject's world, one per viewport at most: a
   process the product watches (parcels along a route, a clock, plants
   growing, a counter turning over). It pauses off screen, stops under
   reduced motion, and either stops within 5 s or has a pause control.
5. One delight that rewards attention, on the primary path: the
   environment of the artefact reacting to the visitor's input (a drawn
   plant flowering as a month slider moves, a jar filling as a quantity
   grows, a seal pressing when a form completes).

Performance budget: transform and opacity only; at most one
`requestAnimationFrame` loop on the page and nothing running while the tab
is hidden; canvas work under 4 ms a frame; the life layer under about 10 KB
of script; interactions answer within 100 ms.

## 3. Imagery and depth

- Layer the composition in three planes: atmosphere (light, colour field,
  grain), content (type and the artefact), foreground (the artefact or a
  detail overlapping the section edge by 48-160px). Recipes in
  layout-and-composition.md.
- One full-bleed moment per page: an illustration, a colour field or the
  artefact running edge to edge.
- An emotional anchor where the world has one: the person who uses the
  product, or the place where it is used. Without photography, draw it --
  an SVG scene built in 3-5 depth planes (color-and-surface.md), a figure
  as silhouette or detail (hands, a back, a figure small in a landscape),
  or a generative canvas scene of the world's process
  (interface-motion.md). Never stock filler, never generated people.
- With supplied photography: art-direct it. Crop to one focal point, grade
  it toward the palette, set type in its negative space, let it bleed off
  an edge.

## 4. Raise contrast

- Scale: one display moment with real tension -- 8-14vw on persuade pages,
  mixed weight, italic or width inside one line, tight leading, cropped by
  the edge or overlapping the artefact (typography.md).
- Weight and size: persuade body 18-20px, lede 22-28px, call-to-action
  labels 18px or more on buttons 52-60px tall with a verb and an object.
- Colour fields: at least one band filled edge to edge with the dominant or
  the accent; a contrast flip (dark to light or back) at the page's turning
  point.
- Density: alternate loud sections (one huge element and space) with quiet
  ones (dense, small, informative). List each section's composition in one
  word; no two neighbours may share one.

## 5. Give it a voice

- Write in the subject's vernacular: the words its practitioners use for
  their own work, not marketing words.
- One line per section may carry personality; facts stay exact.
- Buttons say verb plus object ("Book the sleeper", "Reserve the packet"),
  never "Learn more" or "Get started".
- States speak like the product. Empty: what it means and the next move.
  Error: what failed, what still works, what to do. Success: what changed
  and how to undo it. For a seed library's loan desk: "No packets out on
  loan. Returns show up here the moment they are scanned back in."
- Voice never comes from invented proof: no made-up numbers, quotes or
  logos.

## Operate surfaces

Life on a tool comes from the data responding, not from decoration:

- Anticipation: pre-fill the likely next action with its reason and make it
  one keystroke to accept. For a lab's sample freezer map: "Store in rack
  C, box 4 -- same study, nearest free slot". Explain the rule on request;
  never auto-apply.
- Time made visible: where the data is time-bound, draw the time instead of
  printing it. For a rail departures board: each train's remaining minutes
  as a bar that shortens, the platform change flagged the moment it
  happens.
- Satisfying feedback: the changed item moves visibly to where it now
  belongs (250-350 ms) -- the stored tube drops into its slot on the map,
  the box's free-slot count ticks down -- and the confirmation offers undo.
- Speed: optimistic updates, keyboard paths with visible hints, skeletons
  at final size, no spinner for a local action.
- Keep atmosphere and illustration out of the working area; personality
  goes into empty, error and success states and into the header.

## Bounded

One pass. Report the three moments you replaced, the five life-layer items
and where each lives, and the contrast changes. Then capture, critique, fix
in one batch, stop. If a step does not fit the surface (no world process
worth animating, for example), say so instead of inventing one.

Informed by impeccable (Apache-2.0), the Hyperframes skills (Apache-2.0) and
an Apache-2.0 frontend-design agent skill; ideas re-derived, not copied.
