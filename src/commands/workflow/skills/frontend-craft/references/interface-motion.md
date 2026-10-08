# Interface motion

Motion earns its place by explaining something: that the system heard you,
where a thing went, where you are. One orchestrated moment beats scattered
effects, and motion that answers the user's own action is worth more than
motion that plays at them.

## What motion is for

- Feedback: respond to every press, toggle and drag within 100 ms.
- State change: show that something opened, moved, was added or removed.
- Spatial continuity: keep the user's place when views or order change.
- Orchestration: one arrival sequence on persuade and experience surfaces.

If an animation does none of these, delete it.

## Duration by size and distance

| Change | Duration |
| --- | --- |
| Press, hover, toggle feedback | 80-150 ms |
| Small state (checkbox, tooltip, chip, row highlight) | 150-200 ms |
| Medium (dropdown, popover, toast, card expand) | 200-300 ms |
| Large (drawer, modal, sheet) | 300-400 ms in, 200-300 ms out |
| Arrival sequence, whole | up to 1.2 s; each piece 400-700 ms |

Travel of 100px or less takes 200 ms or less; 100-400px takes 250-350 ms;
a full viewport 400-500 ms. Nothing that answers input takes longer than
500 ms. Exits run at about 70-80% of the entry duration: leaving should
never hold the user up.

## Easing as cubic-bezier

```css
:root {
  --ease-out: cubic-bezier(0.22, 1, 0.36, 1);    /* default: enter, respond */
  --ease-out-strong: cubic-bezier(0.16, 1, 0.3, 1); /* large, dramatic arrivals */
  --ease-in: cubic-bezier(0.5, 0, 0.75, 0);      /* exits: accelerate away */
  --ease-move: cubic-bezier(0.65, 0, 0.35, 1);   /* on-screen moves, reorders */
  --dur-1: 120ms; --dur-2: 200ms; --dur-3: 320ms;
}
```

- Decelerate into place (`--ease-out`) for anything entering or responding;
  accelerate out (`--ease-in`) for anything leaving; ease both ways
  (`--ease-move`) for things moving between two on-screen positions.
- Linear only for continuous mechanical progress: a progress bar tied to
  time, a spinner's rotation.
- No bounce, elastic or springy overshoot on product UI: it reads as a toy
  and delays the settled state. The detector flags "bounce", "elastic",
  "spring(" and `cubic-bezier(0.68, ...)`. A small overshoot
  (`cubic-bezier(0.34, 1.3, 0.64, 1)`, transforms only) is acceptable on one
  element of a playful brand.

## Stagger

30-60 ms between items, with the whole stagger capped at 300-400 ms. Past
about 8 items, reveal the rest together. Order by importance, not DOM
order: the thing the user needs first arrives first.

## Properties

- Animate `transform` and `opacity` (plus `clip-path` for reveals). They run
  on the compositor without reflow.
- Never animate `width`, `height`, `top`, `left`, `margin` or `padding`;
  the detector flags layout properties and they stutter on long pages.
- Name the properties: `transition: transform 200ms var(--ease-out),
  opacity 150ms linear;`. Never `transition: all` (flagged): it animates
  things you did not mean to animate.
- For disclosure heights, transition `grid-template-rows` from `0fr` to
  `1fr` on a wrapper instead of animating `height`.
- `@starting-style` handles entry transitions for elements that appear;
  `document.startViewTransition()` handles cross-view and reorder
  transitions with a fallback where unsupported.

## The arrival sequence (persuade pages)

One sequence, finished well inside 1.2 s:

- 0 ms: headline lines rise 24-40px from behind a clip mask, 600 ms
  `--ease-out-strong`, 80 ms between lines.
- 200 ms: the artefact draws in -- SVG strokes via `stroke-dashoffset`
  over 800-1000 ms `--ease-move`, data marks scale or fade in with a 40 ms
  stagger.
- 500 ms: supporting copy and the primary action fade and rise 12px over
  400 ms.

`zirv frontend render` captures at 2 s of virtual time. A sequence still
running at 2 s is captured mid-flight and judged as broken; one that has
not started (waiting for scroll) is captured as empty.

## Scroll reveals

- Content is visible by default. Add the hidden starting state only through
  a class set by script when IntersectionObserver exists and reduced motion
  is not requested.
- Reveal once, never on every pass; reveal anything already in the
  viewport immediately; travel 12-24px over 500 ms or less.
- Otherwise full-height captures, print and readers without script all see
  holes in the page.

## Motion as the product answering

The strongest motion responds to the user: dragging a limit slider
recomputes a window and its bar glides to the new span (200-300 ms
`--ease-move`); assigning a person moves the row out of "Unassigned" and
into its new group with a 250 ms move and a brief highlight. Use FLIP
(measure first and last positions, animate the transform between them) or
view transitions for reorders. FLIP reads layout, which the detector flags
as advisory: batch reads before writes and say so.

## Reduced motion

Under `@media (prefers-reduced-motion: reduce)`: remove travel, parallax,
zooms and loops; turn movement into opacity changes of 150 ms or less, or
make them instant; keep essential feedback such as focus and pressed
states. The detector flags motion with no reduced-motion path. A floor that
catches what you missed:

```css
@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after {
    animation-duration: 1ms !important;
    animation-iteration-count: 1 !important;
    transition-duration: 1ms !important;
    scroll-behavior: auto !important;
  }
}
```

## Loops

Infinite animation (flagged) is for live status that carries meaning only:
a "live" pulse, a recording dot, a syncing indicator. At most one per
screen, paused when off screen, stopped under reduced motion. Anything that
moves on its own for more than 5 s needs a pause control (WCAG 2.2.2).

## Interruption and input

- Use transitions, which reverse from wherever they are, for state toggles;
  keyframe animations restart and jump.
- Never block input while something animates.
- Hover effects need a non-hover twin: focus-visible for keyboards, a tap
  state for touch.
- Animated counts only when the change is the story; tabular numerals stop
  the digits jittering; the final value sits in the DOM, announced through
  `aria-live="polite"` when it matters.

## Cliches and the better move

- Every section fades up on scroll -> one arrival sequence plus motion on
  interaction.
- Hover lift and shadow growth on every card -> feedback only on things
  that act.
- Bouncy springs everywhere -> decelerating ease, settled fast.
- Spinner-only loading -> skeletons at final size.
- Parallax backgrounds -> static atmosphere.
- Animated gradient or glow loops -> stillness; spend the motion on the
  product's response.

Informed by impeccable (Apache-2.0), the Hyperframes skills (Apache-2.0) and
an Apache-2.0 frontend-design agent skill; ideas re-derived, not copied.
