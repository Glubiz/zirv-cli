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
| Arrival sequence, whole | up to 1.2 s; each piece 400-800 ms |

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
  transition: transform 140ms var(--ease-out), background-color 120ms linear,
    box-shadow 150ms var(--ease-out);
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
const reduceMotion = () => matchMedia("(prefers-reduced-motion: reduce)").matches;
function commit(update) {
  if (!document.startViewTransition || reduceMotion()) return update();
  document.startViewTransition(update);
}
```

```css
/* give each moving item a unique name, e.g. style="view-transition-name: packet-0412" */
::view-transition-group(*) {
  animation-duration: 300ms;
  animation-timing-function: var(--ease-move);
}
/* no root cross-fade; normal blending stops the two opaque snapshots adding up to white */
::view-transition-old(root), ::view-transition-new(root) {
  animation: none;
  mix-blend-mode: normal;
}
```

Name only the items that can move (up to about 50); turning off the root
cross-fade keeps the rest of the page still.

FLIP when the DOM nodes are kept (keyed rendering) and View Transitions are
unavailable (`reduceMotion()` is the helper above):

```js
function flip(elements, mutate) {
  if (reduceMotion()) return mutate(); // the CSS floor does not stop el.animate()
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
  if (reduceMotion()) return; // the toast simply appears
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
  <figure class="story-art" aria-hidden="true">
    <svg viewBox="0 0 600 400">
      <path class="path" pathLength="1" d="..."/>
      <g class="detail-3">...</g>
    </svg>
  </figure>
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
/* default (no script, reduced motion, print): everything drawn and visible */
.story-art .path { stroke-dasharray: 1; stroke-dashoffset: 0;
  transition: stroke-dashoffset 600ms var(--ease-move); }
.story-art .detail-3 { transition: opacity 300ms linear; }
/* only while scripted: hide what a step has not reached yet; reached stays drawn */
.story-art.is-live[data-step="1"] .path { stroke-dashoffset: 1; }
.story-art.is-live:is([data-step="1"], [data-step="2"]) .detail-3 { opacity: 0; }
@media (max-width: 700px) {
  .story { grid-template-columns: 1fr; }
  .story-art { top: 0; z-index: 1; max-height: 40vh; }
}
```

```js
const art = document.querySelector(".story-art");
if ("IntersectionObserver" in window && !matchMedia("(prefers-reduced-motion: reduce)").matches) {
  art.dataset.step = "1";
  art.classList.add("is-live");
  const io = new IntersectionObserver((entries) => {
    for (const e of entries) if (e.isIntersecting) art.dataset.step = e.target.dataset.step;
  }, { rootMargin: "-45% 0px -45% 0px" });
  document.querySelectorAll(".story-steps [data-step]").forEach((el) => io.observe(el));
}
```

Each rule hides only what a step has not reached, so steps 2 and 3 keep
everything drawn so far, and without script the artefact is complete: that
is what readers without script, reduced motion and print see.

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

One process the product watches, shown moving: parcels along a route, a
clock, plants growing, a counter turning over. At most one per viewport. It
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

A live "now" marker on time-bound data is ambient without animating:
position it from a custom property updated once a minute, over the span the
view shows (for a rail timetable, its first and last departure).

```js
const view = document.querySelector(".timetable"); // data-start / data-end: ISO times
const start = Date.parse(view.dataset.start), end = Date.parse(view.dataset.end);
const tick = () => view.style.setProperty("--now",
  String(Math.min(1, Math.max(0, (Date.now() - start) / (end - start)))));
tick(); setInterval(tick, 60_000);
```

```css
.timetable { position: relative; }
.now-marker { position: absolute; inset-block: 0; width: 2px; background: var(--accent);
  inset-inline-start: calc(var(--now, 0) * 100%); }
```

A generative canvas scene can draw the world's process when no imagery
exists. Example: seedlings on a greenhouse bench growing and leaning toward
a lamp that drifts along the glass. Seeded, cheap, stopped off screen and
under reduced motion, guarded against duplicate loops, with a pause button:

```html
<figure class="bench">
  <canvas class="scene" aria-hidden="true"></canvas>
  <button class="scene-toggle" type="button">Pause animation</button>
</figure>
```

```js
const canvas = document.querySelector(".bench canvas");
const toggle = document.querySelector(".bench .scene-toggle");
const ctx = canvas.getContext("2d");
const still = matchMedia("(prefers-reduced-motion: reduce)");
let seed = 11, visible = false, paused = false, running = false, last = 0;
const rand = () => (seed = (seed * 16807) % 2147483647) / 2147483647;
const sprouts = Array.from({ length: 60 }, () =>
  ({ x: 0.03 + rand() * 0.94, max: 0.3 + rand() * 0.45, rate: 0.6 + rand() * 0.8 }));
let growth = still.matches ? 20000 : 2500; // ms of growth shown; a formed first frame
function fit() {
  const dpr = Math.min(devicePixelRatio, 2);
  canvas.width = canvas.clientWidth * dpr; canvas.height = canvas.clientHeight * dpr;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
}
function leaf(x, y, size, angle) {
  ctx.beginPath(); ctx.ellipse(x, y, size * 2, size, angle, 0, Math.PI * 2); ctx.fill();
}
function draw(t) {
  const w = canvas.clientWidth, h = canvas.clientHeight, soil = h * 0.92;
  const lamp = w * (0.5 + 0.35 * Math.sin(t * 0.00012)); // the lamp drifting along the glass
  ctx.clearRect(0, 0, w, h);
  ctx.lineWidth = 2; ctx.lineCap = "round";
  ctx.strokeStyle = "#4f6f32"; ctx.fillStyle = "#7fa548";
  for (const s of sprouts) {
    const grown = Math.min(1, (t / 8000) * s.rate); // full height after 5-13 s
    const x = s.x * w, top = soil - grown * s.max * h;
    const lean = ((lamp - x) / w) * 36 * grown;      // lean toward the light
    ctx.beginPath(); ctx.moveTo(x, soil);
    ctx.quadraticCurveTo(x, (soil + top) / 2, x + lean, top); ctx.stroke();
    if (grown > 0.4) { leaf(x + lean - 5, top, 3 * grown, -0.5); leaf(x + lean + 5, top, 3 * grown, 0.5); }
  }
}
function frame(now) {
  growth += Math.min(now - last, 50); last = now; // no jump after a pause or tab switch
  draw(growth);
  if (visible && !paused && !still.matches && !document.hidden) requestAnimationFrame(frame);
  else running = false;
}
function play() {
  if (running || paused || !visible || still.matches || document.hidden) return;
  running = true; last = performance.now(); requestAnimationFrame(frame);
}
toggle.hidden = still.matches;
toggle.addEventListener("click", () => {
  paused = !paused;
  toggle.textContent = paused ? "Play animation" : "Pause animation";
  play();
});
fit(); draw(growth);
new IntersectionObserver(([e]) => { visible = e.isIntersecting; play(); }).observe(canvas);
document.addEventListener("visibilitychange", play);
still.addEventListener("change", () => { toggle.hidden = still.matches; play(); });
```

Keep it under 4 ms a frame, cap the pixel ratio at 2, keep the pause button
visible and keyboard-reachable while the scene moves, and keep any text over
the canvas at 4.5:1 against its brightest frame.

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

One sequence, finished by about 1.0 s: headline lines rise 24-40px from
behind a clip mask (600 ms `--ease-out-strong`, 80 ms between at most three
lines, so the last lands at 760 ms); at 200 ms the artefact draws in
(`stroke-dashoffset`, 600-800 ms `--ease-move`, done by 1.0 s, marks
staggered 40 ms inside that); at 500 ms copy and action rise 12px over
400 ms, landing at 900 ms. `zirv frontend render` captures at 2 s of virtual time: a
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
