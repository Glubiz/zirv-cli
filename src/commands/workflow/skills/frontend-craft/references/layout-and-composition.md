# Layout and composition

Composition is decided by the shape of the content, not by a section
template. Repeated text-left, list-right sections separated by hairlines are
the signature of a page nobody designed.

## Start from the content's shape

For each section, name the shape of what it says, then lay it out in that
shape:

- a sequence (timeline, steps, a day) -> a horizontal track or a numbered
  path the eye follows;
- a place (route, map, floor plan) -> full-bleed, annotated in place;
- a comparison -> a table or small multiples with shared axes;
- a single claim -> type as the image, large, with nothing competing;
- an object (the product's artefact) -> large, cropped by the edge,
  annotated with callouts that point at real parts.

The hero is the most characteristic artefact of the subject's world, shown
doing its job with product truth (example data labelled as example). It
takes 50-70% of the 1440x1000 first viewport. The headline, the primary
action and at least part of the artefact are visible in both the 1440 and
390 first viewports.

## Grid and asymmetry

- 12 columns at desktop with 24-32px gutters, 8 at tablet, 4 on phones.
  Page margins `clamp(20px, 5vw, 96px)`; content up to 1200-1440px wide;
  text columns capped at 65ch inside it.
- Split asymmetrically: 7/5, 8/4, 5/7 alternating, or text starting at
  column 2 with the artefact spanning 6-12. A 6/6 split on every row is
  the template look.
- Break the grid on purpose once or twice per page: bleed the signature to
  the viewport edge, overlap a band boundary by 48-120px, or hang a figure
  outside the text column.
- Anchor to a strong left edge (inline-start). Centre only single short
  statements.

## Layers, overlap and full-bleed

A page of flat bands, each with text on the left and an empty right half,
has no depth. Compose in three planes -- atmosphere (light, colour field,
scene), content (type, the artefact), foreground (an artefact or detail that
crosses a boundary) -- and let them overlap.

```css
.page {
  display: grid;
  grid-template-columns:
    [full-start] minmax(20px, 1fr)
    [content-start] min(100% - 40px, 1280px) [content-end]
    minmax(20px, 1fr) [full-end];
}
.page > * { grid-column: content; }
.page > .bleed { grid-column: full; }

/* stack layers in one cell instead of positioning them absolutely */
.stage { display: grid; }
.stage > * { grid-area: 1 / 1; }
.stage .atmosphere { z-index: 0; }
.stage .copy { z-index: 1; align-self: end; }

/* the artefact crosses into the next band */
.artefact { position: relative; z-index: 2;
  margin-block-end: clamp(-160px, -10vw, -48px); }
```

- One full-bleed moment per page: a scene, a colour field or the artefact
  running edge to edge.
- Overlap a band boundary by 48-160px with the artefact or the display
  line, and crop at least one element with the viewport edge.
- Fill the empty half: if a section's right half is empty at 1440, the
  artefact, a scene or a pull figure belongs there -- or the text column
  moves and the section becomes a full-width statement.
- Check every overlap at 390 and 768: overlapping elements must not cover
  text or controls, and text over a layer keeps 4.5:1.

## Rhythm across sections

- Write each section's composition in one word (statement, split,
  full-bleed, track, table, scene, grid). No two neighbours share one.
- Alternate loud sections (one huge element, generous space) with quiet
  ones (dense, smaller, informative). Two loud sections in a row cancel out.
- Change surface with the role (color-and-surface.md): the turn in the
  story is where a contrast flip or a colour field lands.

## Scale and hierarchy

- One dominant element per viewport, at least 3x the visual weight of the
  next. Squint test: blur the capture by about 8px; one primary, one
  secondary and then the rest must still read.
- Headline to body: 3-8x on persuade surfaces, 1.5-2x on operate surfaces.
- Reading order follows a Z or F path; the primary action sits at the end
  of the first path, not floating in a corner.

## Spacing system

- 4px base: 4, 8, 12, 16, 24, 32, 48, 64, 96, 128, 160.
- Proximity: space between groups is at least 2x the space inside a group.
  Label to field 4-8px, field to field 16-24px, group to group 32-48px,
  section to section 96-160px at desktop and 56-80px on phones.
- Vary the pace: a dense instrument band (48-64px padding) after a spacious
  hero gives rhythm; identical padding on every section flattens the page.
- Use logical properties (`margin-inline`, `padding-block`,
  `inset-inline-start`): physical left/right breaks right-to-left layouts
  and the detector flags it.

## Persuade pages: a narrative, not a template

Four to six sections, each a different composition:

1. Arrival: claim, artefact, primary action.
2. Demonstration: the product doing its job, interactive where it is cheap
   (a slider that recomputes a real result, a toggle between two real
   states).
3. Mechanism: how it works, drawn in the world's own form (a plan, a
   timeline, a document passing between hands).
4. Assurance: real constraints and safety features shown as concrete
   scenes, not an icon grid.
5. Action: the primary action again, with the context that makes it
   obvious.

No invented testimonials, logos, user counts, ratings, press quotes or
prices. If the brief lacks proof, demonstrate instead of claiming.

## Operate surfaces: tools, tables, dashboards

Start from the job: which decision does the user make here, how often, on
what data? Put that data first and make the decision one step.

- Density: rows 36-40px by default (32 compact, 44-48 touch); text
  13-14px; cell padding 8-12px horizontal; row dividers at low contrast or
  zebra striping at L +0.015. Dense rows never shrink targets: interactive
  controls inside rows keep a 44px hit area on coarse pointers (padding or
  a pseudo-element extending the hit area) and at least 24x24px on fine
  pointers (WCAG 2.5.8).
- Alignment: text left, numbers right with tabular numerals, one date
  format; headers aligned like their data; sticky header; the identifying
  column first and pinned on narrow screens.
- Urgency lives in the data: group or sort by what needs action (for a
  bike workshop: "Promised today 3", "Waiting for parts 2", "Ready for
  pickup 5") with group headers that carry a count and the bulk action. A separate alert card that repeats table rows
  doubles the reading and splits the action.
- Status: icon or shape plus word plus colour, in a fixed-width column.
  Pills on every value turn the table into confetti.
- Inline assignment: an "Assign" text button in the cell opens a combobox
  popover -- type to filter, arrows to move, Enter assigns, Esc cancels,
  focus returns to the cell. Show each option's current load ("Ana, 1 bike
  promised today") so the choice is informed, and put the suggested choice
  first with its reason. Update optimistically with a 5-8 s undo, and let
  the row travel to its new group (interface-motion.md). A full select
  element on every row is heavy, noisy and slow to scan.
- Time made visible: where work is time-bound, draw it against a now-line
  (a day strip per row or a lane per person) so lateness is seen, not
  computed.
- Filters: segmented views with counts, reflected in the URL.
- Use the canvas: at 1440 a 900px table leaves half the screen empty. Add a
  second region that serves the job -- an assignment or detail panel, a
  capacity strip per person, a timeline of today's deadlines. At 390 rows
  become two-line items with deadline and action on the trailing edge; the
  second region becomes a sheet.
- States: loading is skeleton rows at final height (no layout shift);
  empty says what it means and offers the next action; error says what
  failed, keeps the last data with its timestamp, and offers retry;
  partial data marks stale rows.
- Keyboard: Tab follows reading order; optional j/k row movement with a
  visible row focus; list shortcuts in a `?` dialog.
- Cards only for independent, movable objects. Never cards in cards; group
  with space, alignment and a hairline instead.

## Responsive recomposition

- Recompose, do not shrink: at 390 reorder (artefact before secondary
  copy), stack splits, turn horizontal tracks vertical or into a snapping
  scroller, keep touch targets 44px.
- Container queries for components that live at several widths.
- Test the longest real strings (German, Finnish), 200% zoom and RTL;
  give flex and grid children with text `min-width: 0`.
- Use `100dvh` or `100svh`, not `100vh`, and no fixed widths of 800px or
  more; the detector flags both.

## Capturing for judgement

`zirv frontend render` captures only the first viewport at 390x844,
768x1024 and 1440x1000 after 2 s of virtual time: no scroll, no hover. For
any page longer than one screen, also take a full-height capture with the
browser recorded in the render report, for example:

```sh
"<browser>" --headless=new --hide-scrollbars --virtual-time-budget=2000 \
  --window-size=1440,5000 --screenshot=full-1440.png file:///abs/path/index.html
```

Repeat at 390 wide. Elements sized in vh or dvh stretch in a tall window:
judge the hero from the normal capture and the flow from the tall one.
Content hidden until it scrolls into view will look missing -- make content
visible by default (see interface-motion.md). Hover, focus, mid-arrival and
scroll states need the life captures in critique.md.

## Cliches and the better move

- Hero plus three feature cards -> the artefact as hero; features
  demonstrated in place.
- Bento grid without information logic -> a grid only when cell size
  encodes importance.
- Icon tile above every heading -> no icon, or an inline glyph that
  disambiguates.
- The same section shape repeated -> a composition per content shape, no
  two neighbours alike.
- Bands of left-aligned text with an empty right half -> layers, overlap,
  a full-bleed moment.
- Everything centred -> a strong edge and an asymmetric split.
- Half an operate screen empty -> a second region that serves the job.
- Big number, small label stat strip -> only numbers the product owns, in
  context.
- Middle-dot meta strings ("12 open . 3 waiting") -> make the counts the
  filters they describe.

Informed by impeccable (Apache-2.0), the Hyperframes skills (Apache-2.0) and
an Apache-2.0 frontend-design agent skill; ideas re-derived, not copied.
