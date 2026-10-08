# Interface motion

A page where nothing responds reads as a screenshot of a page. Motion earns
its place by answering the user, showing cause and effect, or showing the
subject's world at work; it never decorates for its own sake. This file is
the life layer: five parts, each with CSS-first code, a reduced-motion path
and a performance budget.

1. Response on every interactive element.
2. State changes that show cause.
3. One scroll moment.
4. Ambient motion from the subject's world.
5. One delight that rewards attention.

## Durations

| Change | Duration |
| --- | --- |
| Press, hover, toggle feedback | 80-150 ms |
| Small state (checkbox, tooltip, chip, row highlight) | 150-200 ms |
| Medium (dropdown, popover, toast, card expand) | 200-300 ms |
| Large (drawer, modal, sheet) | 300-400 ms in, 200-300 ms out |
| Item travelling to a new place in a list | 250-350 ms |
| Arrival sequence, whole | up to 1.2 s; each piece 400-700 ms |

Travel of 100px or less takes 200 ms or less; 100-400px takes 250-350 ms;
a full viewport 400-500 ms. Nothing that answers input takes longer than
500 ms. Exits run at about 70-80% of the entry duration.

## Easing tokens

```css
:root {
  --ease-out: cubic-bezier(0.22, 1, 0.36, 1);       /* enter, respond */
  --ease-out-strong: cubic-bezier(0.16, 1, 0.3, 1); /* large arrivals */
  --ease-in: cubic-bezier(0.5, 0, 0.75, 0);         /* exits */
  --ease-move: cubic-bezier(0.65, 0, 0.35, 1);      /* on-screen moves */
  /* damped spring (damping ratio 0.68): about 5% overshoot, 350-500 ms */
  --ease-spring: linear(0, 0.066, 0.218, 0.402, 0.581, 0.736, 0.858, 0.946,
    1.004, 1.037, 1.052, 1.054, 1.049, 1.04, 1.03, 1.02, 1.012, 1.006,
    1.002, 0.999, 0.998, 0.997, 0.997, 0.997, 1);
  --dur-1: 120ms; --dur-2: 200ms; --dur-3: 320ms;
}
```

- Decelerate into place (`--ease-out`) for anything entering or answering;
  accelerate away (`--ease-in`) for exits; `--ease-move` between two
  on-screen positions; linear only for continuous mechanical progress.
- The spring is for transforms of small things that are handled: toggles,
  chips, handles, a card landing. It gives a physical settle without a
  bounce. Never on opacity, colour or large panels.
- No bounce or elastic curves and no `cubic-bezier(0.68, ...)`: the detector
  flags "bounce", "elastic", "spring(" and that curve as advisory, and they
  delay the settled state. `linear()` is not flagged.

## 1. Response on every interactive element

Every link, button, input, row, tab and handle answers hover (on
hover-capable pointers), press and keyboard focus.

```css
.btn {
  transition: transform 180ms var(--ease-out), background-color 150ms linear,
    box-shadow 200ms var(--ease-out);
}
@media (hover: hover) and (pointer: fine) {
  .btn:hover {
    transform: translateY(-2px);
    box-shadow: 0 8px 18px -8px oklch(0.25 0.03 var(--h) / 0.4);
  }
}
.btn:active { transform: translateY(0) scale(0.97); transition-duration: 60ms; }
.btn:focus-visible { outline: 3px solid var(--focus); outline-offset: 3px; }

a { text-underline-offset: 0.18em; transition: text-underline-offset 150ms var(--ease-out); }
a:hover { text-underline-offset: 0.3em; }

.switch-thumb { transition: transform 420ms var(--ease-spring); }
.row { transition: background-color 120ms linear; }
.row:hover, .row:focus-within { background: var(--row-hover); }
.handle:active { transform: scale(1.15); cursor: grabbing; }
```

- Press feedback must also work on touch (`:active`), and nothing important
  may be hover-only.
- Disabled controls look disabled and do not respond.

## 2. State changes that show cause

When something changes place or state, the user should see it go there.

View Transitions for in-page updates, with an instant fallback:

```js
function commit(update) {
  const still = matchMedia("(prefers-reduced-motion: reduce)").matches;
  if (!document.startViewTransition || still) return update();
  document.startViewTransition(update);
}
```

```css
/* give each moving item a unique name, e.g. style="view-transition-name: job-1042" */
::view-transition-group(*) {
  animation-duration: 300ms;
  animation-timing-function: var(--ease-move);
}
::view-transition-old(root), ::view-transition-new(root) { animation: none; }
```

Name only the items that can move (up to about 50); turning off the root
cross-fade keeps the rest of the page still.

FLIP when the DOM nodes are kept (keyed rendering) and View Transitions are
unavailable:

```js
function flip(elements, mutate) {
  const first = new Map([...elements].map((el) => [el, el.getBoundingClientRect()]));
  mutate();
  for (const [el, a] of first) {
    const b = el.getBoundingClientRect();
    const dx = a.left - b.left, dy = a.top - b.top;
    if (dx || dy) el.animate(
      [{ transform: `translate(${dx}px, ${dy}px)` }, { transform: "none" }],
      { duration: 300, easing: "cubic-bezier(0.65, 0, 0.35, 1)" });
  }
}
```

FLIP reads layout, which the detector flags as advisory: read everything
first, write once, and say so.

A confirmation grows out of the control that caused it (the toast needs
`transform-origin: 0 0`):

```js
function growFrom(source, toast) {
  const a = source.getBoundingClientRect(), b = toast.getBoundingClientRect();
  toast.animate([
    { transform: `translate(${a.left - b.left}px, ${a.top - b.top}px) scale(${a.width / b.width}, ${a.height / b.height})`, opacity: 0 },
    { transform: "none", opacity: 1 },
  ], { duration: 280, easing: "cubic-bezier(0.22, 1, 0.36, 1)" });
}
```

Counts that change tick visibly: swap in the new number, then rise it 6px
and fade it in over 200 ms; tabular numerals stop the row jittering, and
the real value is always in the DOM (`aria-live="polite"` when it matters).

## 3. One scroll moment

Content is visible by default; scroll effects are an enhancement. The
dependable pattern is an artefact that stays in view and changes state as
the steps beside it pass:

```html
<section class="story">
  <figure class="story-art" data-step="1" aria-hidden="true">...</figure>
  <div class="story-steps">
    <article data-step="1">...</article>
    <article data-step="2">...</article>
    <article data-step="3">...</article>
  </div>
</section>
```

```css
.story { display: grid; grid-template-columns: 7fr 5fr; gap: 48px; }
.story-art { position: sticky; top: 12vh; align-self: start; }
.story-art .path { transition: stroke-dashoffset 600ms var(--ease-move); }
.story-art[data-step="2"] .path { stroke-dashoffset: 0; }
@media (max-width: 700px) {
  .story { grid-template-columns: 1fr; }
  .story-art { top: 0; z-index: 1; max-height: 40vh; }
}
```

```js
const art = document.querySelector(".story-art");
const io = new IntersectionObserver((entries) => {
  for (const e of entries) if (e.isIntersecting) art.dataset.step = e.target.dataset.step;
}, { rootMargin: "-45% 0px -45% 0px" });
document.querySelectorAll(".story-steps [data-step]").forEach((el) => io.observe(el));
```

The first step's state must be a complete picture on its own: that is what
the first-viewport capture, readers without script and print will see.

Scroll-driven animations add polish where supported:

```css
@supports (animation-timeline: view()) {
  @media (prefers-reduced-motion: no-preference) {
    .reveal { animation: rise linear both; animation-timeline: view();
      animation-range: entry 0% entry 100%; }
    .atmosphere { animation: drift linear both; animation-timeline: view();
      animation-range: cover; }
  }
}
@keyframes rise { from { opacity: 0; transform: translateY(32px); } }
@keyframes drift { from { transform: translateY(-40px); } to { transform: translateY(40px); } }
```

The `entry` range finishes once an element is fully inside the viewport, so
a full-height capture still shows everything.

## 4. Ambient motion from the subject's world

One process the product watches, shown moving: a flow along a path, a
clock, a queue draining, light changing. At most one per viewport. It
pauses off screen, stops under reduced motion, and either stops within
5 s or has a visible pause control (WCAG 2.2.2).

```css
.flow path { stroke-dasharray: 4 12; animation: flow 1.6s linear 3; }
@keyframes flow { to { stroke-dashoffset: -16; } } /* dash + gap = 16: seamless */
.is-offscreen .flow path, .is-paused .flow path { animation-play-state: paused; }
@media (prefers-reduced-motion: reduce) { .flow path { animation: none; } }
```

```js
const watch = new IntersectionObserver((entries) => entries.forEach((e) =>
  e.target.classList.toggle("is-offscreen", !e.isIntersecting)));
document.querySelectorAll("[data-ambient]").forEach((el) => watch.observe(el));
```

For a continuous loop (`infinite`, flagged as advisory), add a pause button
(`aria-pressed`, toggling `.is-paused`) and say why the loop carries meaning.

A now-line on time-bound data is ambient without animating: update a custom
property once a minute.

```js
const dayStart = new Date().setHours(6, 0, 0, 0), dayLength = 14 * 3600e3; // the shown span
const tick = () => document.documentElement.style.setProperty(
  "--now", String((Date.now() - dayStart) / dayLength));
tick(); setInterval(tick, 60_000);
```

```css
.now-line { position: absolute; inset-block: 0; width: 2px; background: var(--accent);
  inset-inline-start: calc(var(--now) * 100%); }
```

A generative canvas scene can draw the world's process (a current, a
crowd, growth) when no imagery exists. Seeded, capped, paused when unseen:

```js
const canvas = document.querySelector("canvas.scene"); // aria-hidden="true"
const ctx = canvas.getContext("2d");
const still = matchMedia("(prefers-reduced-motion: reduce)");
let seed = 7, visible = true;
const rand = () => (seed = (seed * 16807) % 2147483647) / 2147483647;
const dots = Array.from({ length: 240 }, () => ({ x: rand(), y: rand() }));
function fit() {
  const dpr = Math.min(devicePixelRatio, 2);
  canvas.width = canvas.clientWidth * dpr; canvas.height = canvas.clientHeight * dpr;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
}
function step(t) {
  const w = canvas.clientWidth, h = canvas.clientHeight;
  ctx.fillStyle = "rgba(14, 30, 48, 0.08)"; ctx.fillRect(0, 0, w, h); // fading trails
  ctx.strokeStyle = "rgba(190, 220, 235, 0.55)"; ctx.beginPath();
  for (const d of dots) {
    const a = 0.5 + 0.6 * Math.sin(d.y * 7 + t * 0.0004); // field direction
    const dx = Math.cos(a) * 0.0015, dy = Math.sin(a) * 0.0015;
    ctx.moveTo(d.x * w, d.y * h); ctx.lineTo((d.x + dx) * w, (d.y + dy) * h);
    d.x = (d.x + dx + 1) % 1; d.y = (d.y + dy + 1) % 1;
  }
  ctx.stroke();
}
function loop(t) { step(t); if (visible && !still.matches && !document.hidden) requestAnimationFrame(loop); }
fit(); for (let i = 0; i < 120; i++) step(i * 16); // a formed first frame for captures
new IntersectionObserver(([e]) => { visible = e.isIntersecting; if (visible) requestAnimationFrame(loop); })
  .observe(canvas);
document.addEventListener("visibilitychange", () => {
  if (!document.hidden && visible) requestAnimationFrame(loop);
});
```

Keep it under 4 ms a frame (about 300 strokes), cap the pixel ratio at 2,
and keep any text over the canvas at 4.5:1 against its brightest frame.

## 5. One delight

A small reward on the primary path: the artefact's environment reacting to
the visitor's input. A registered custom property makes any value
transitionable:

```css
@property --t { syntax: "<number>"; inherits: true; initial-value: 0; }
.scene {
  transition: --t 500ms var(--ease-out);
  background: color-mix(in oklch, var(--bloom) calc(var(--t) * 100%), var(--bare));
}
.scene .sprout { transform: scale(calc(0.6 + var(--t) * 0.4)); }
@media (prefers-reduced-motion: reduce) { .scene { transition: none; } }
```

```js
slider.addEventListener("input", () =>
  scene.style.setProperty("--t", String(slider.value / slider.max)));
```

Under reduced motion the scene still reacts, just without the tween. The
delight never gates the task and never hides information.

## The arrival sequence (persuade pages)

One sequence, finished well inside 1.2 s: headline lines rise 24-40px from
behind a clip mask (600 ms `--ease-out-strong`, 80 ms between lines); at
200 ms the artefact draws in (`stroke-dashoffset`, 800-1000 ms
`--ease-move`, marks staggered 40 ms); at 500 ms copy and action rise 12px
over 400 ms. `zirv frontend render` captures at 2 s of virtual time: a
sequence still running is captured mid-flight, one waiting for scroll is
captured empty.

Stagger 30-60 ms between items, capped at 300-400 ms in total; past about
8 items reveal the rest together; order by importance, not DOM order.

## Properties and performance budget

- Animate `transform`, `opacity` and `clip-path`; never `width`, `height`,
  `top`, `left`, `margin` or `padding` (flagged; they reflow). Name
  transitioned properties; never `transition: all` (flagged).
- For disclosure heights, transition `grid-template-rows` from `0fr` to
  `1fr` on a wrapper.
- At most one `requestAnimationFrame` loop on the page; nothing runs while
  the tab is hidden or the element is off screen.
- Scroll work through IntersectionObserver or scroll timelines, never
  scroll listeners that read layout.
- `will-change` only on elements about to move, removed afterwards.
- Interactions answer within 100 ms (INP under 200 ms); the whole life
  layer stays under about 10 KB of script.

## Reduced motion

Under `prefers-reduced-motion: reduce`, remove travel, parallax, zooms and
loops; turn movement into opacity changes of 150 ms or less or make it
instant; keep feedback such as focus and pressed states. A floor that
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

## Interruption and input

Use transitions, which reverse from wherever they are, for state toggles;
keyframe animations restart and jump. Never block input while something
animates. Hover effects need a focus-visible twin and a tap state.

## Cliches and the better move

- Nothing responds -> every control answers hover, press and focus.
- Every section fades up on scroll -> one arrival sequence and one scroll
  moment with a purpose.
- Things appear and vanish in place -> they travel to where they went.
- Hover lift and shadow on every card -> lift only on things that act.
- Bouncy easing everywhere -> a damped spring on handled things only.
- Spinner-only loading -> skeletons at final size.
- Animated gradient blobs -> motion from the subject's own process.

Informed by impeccable (Apache-2.0), the Hyperframes skills (Apache-2.0) and
an Apache-2.0 frontend-design agent skill; ideas re-derived, not copied.
