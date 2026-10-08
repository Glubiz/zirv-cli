# Critique

One bounded pass that turns evidence into a ranked, concrete fix list. Use
it on the plan before building and on the render after ELEVATE
(elevate.md). A clean detector is a floor, not a verdict; a correct page
that does not move, speak or surprise is not finished.

## 1. Gather evidence that can see life

- All fresh `zirv frontend render` captures (390, 768, 1440). They show
  only the resting first viewport after 2 s.
- A full-height capture at 1440 and 390 for any page longer than one
  screen (recipe in layout-and-composition.md).
- State captures: loading, empty, error and success (query parameters,
  toggles or fixtures).
- Life captures: mid-arrival and after it, a hover state, a keyboard focus
  state, and two scroll positions, at 1440 and 390. The script below drives
  the browser recorded in the render report through the DevTools protocol
  with Node 22 or newer (built-in `fetch` and `WebSocket`):

```js
// life-capture.mjs: node life-capture.mjs <browser> <url> <out-dir> [width] [selector]
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
const [browser, url, out, width = "1440", selector = "main a, main button"] = process.argv.slice(2);
const chrome = spawn(browser, ["--headless=new", "--remote-debugging-port=9333",
  "--hide-scrollbars", `--window-size=${width},1000`, "about:blank"], { stdio: "ignore" });
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
let pages = null;
for (let i = 0; i < 50 && !pages; i++) {
  await wait(200);
  pages = await fetch("http://127.0.0.1:9333/json").then((r) => r.json()).catch(() => null);
}
if (!pages) throw new Error("no DevTools endpoint on 127.0.0.1:9333: port blocked or browser did not start");
const ws = new WebSocket(pages.find((p) => p.type === "page").webSocketDebuggerUrl);
await new Promise((r) => ws.addEventListener("open", r, { once: true }));
let seq = 0;
const pending = new Map();
ws.addEventListener("message", (e) => {
  const m = JSON.parse(e.data);
  if (pending.has(m.id)) { pending.get(m.id)(m.result); pending.delete(m.id); }
});
const cdp = (method, params = {}) =>
  new Promise((r) => { pending.set(++seq, r); ws.send(JSON.stringify({ id: seq, method, params })); });
const js = async (expression) =>
  (await cdp("Runtime.evaluate", { expression, returnByValue: true })).result.value;
const shot = async (name) => {
  const { data } = await cdp("Page.captureScreenshot");
  writeFileSync(`${out}/${width}-${name}.png`, Buffer.from(data, "base64"));
};
const tab = async () => {
  for (const type of ["rawKeyDown", "keyUp"])
    await cdp("Input.dispatchKeyEvent", { type, key: "Tab", code: "Tab", windowsVirtualKeyCode: 9 });
};
await cdp("Page.enable");
await cdp("Page.navigate", { url });
await wait(400); await shot("arrival-mid");
await wait(2600); await shot("arrival-done");
const [x, y] = await js(`(() => { const el = document.querySelector(${JSON.stringify(selector)});
  el.scrollIntoView({ block: "center" }); const r = el.getBoundingClientRect();
  return [r.x + r.width / 2, r.y + r.height / 2]; })()`);
await cdp("Input.dispatchMouseEvent", { type: "mouseMoved", x, y });
await wait(400); await shot("hover");
await cdp("Input.dispatchMouseEvent", { type: "mouseMoved", x: 0, y: 0 });
await js("scrollTo(0, 0)");
for (const type of ["mousePressed", "mouseReleased"]) // click an empty corner: Tab starts at the top
  await cdp("Input.dispatchMouseEvent", { type, x: 1, y: 1, button: "left", clickCount: 1 });
await tab(); await tab();
await wait(300); await shot("focus");
const height = await js("document.documentElement.scrollHeight");
for (const f of [0.35, 0.7]) {
  await js(`scrollTo(0, ${Math.round(height * f)})`);
  await wait(900); await shot(`scroll-${Math.round(f * 100)}`);
}
ws.close(); chrome.kill();
```

  If the debugging port cannot bind (a sandbox), or no browser is
  available, say which life states are unverified; never claim them.
- The plan: subject, world, type pair, palette, layout sketch, voice, life
  layer, signature, and the three moments ELEVATE replaced.

Look at everything side by side, first at thumbnail size, then at 100%.
Compare against the plan, not against what you remember building.

## 2. Five-second read

For each first viewport, write three answers without reading body copy:
what is this, who is it for, what do I do next? If any answer needs the
paragraph text, the hierarchy failed.

## 3. Interchangeable-product test

Mentally swap in a competitor's name and logo, then an unrelated product's.
If the page still works, it is anonymous. The hero artefact, the type, the
palette, the composition and the voice must belong to this subject's
world. Second question: is this what a bare "make a page for X" request
would produce? If yes, it is the model average, however tidy.

## 4. Squint test

Blur the capture by about 8px. You should see one dominant element, a clear
second, then everything else. Equal weights everywhere means no hierarchy;
three things shouting means none of them is heard.

## 5. The alive test

Check each item; every "no" on a new persuade or experience surface is a
finding. Substantial operate surfaces answer the operate items instead of
the scroll, ambient and imagery items.

- Responsive elements: count the interactive elements and how many answer
  hover, press and focus-visible. Anything below all of them is a finding.
- State changes: when something changes place or state, does it visibly
  travel or grow from its cause (View Transitions, FLIP, a confirmation from
  its control)?
- The scroll moment: name it. Content is visible without it, and the full
  height capture shows every section complete.
- Ambient world motion: name the process it shows. One per viewport at
  most, paused off screen, off under reduced motion, stopping within 5 s or
  with a pause control.
- The delight: name it and where on the primary path it sits.
- Imagery and depth: at least one layered or overlapping moment and one
  full-bleed moment; an emotional anchor (a person or a place) where the
  world has one.
- Type: one display moment with real tension; persuade body 18-20px;
  actions large enough to invite a press.
- Rhythm: write each section's composition in one word; no two neighbours
  repeat, and loud and quiet sections alternate.
- Voice: buttons say verb and object; empty, error and success states sound
  like the product.
- ELEVATE: the three generic moments named in the plan are gone, replaced
  by moments only this product could have.
- Operate: a suggested next action with its reason; time drawn against a
  now-line where work is time-bound; a changed item travels to its group;
  no spinner for a local action.
- The director test: name the one frame a design director would screenshot
  to show a colleague. If you cannot name one, the page is not finished.

## 6. Timidity is a defect

Correct but quieter or stiller than the plan is a finding. Check:

- Fallback type in the capture where the plan names a face: the font is not
  loading (hosted CDN blocked, wrong path, missing @font-face).
- One weight for every heading, a display size shrunk to "safe", body and
  buttons at 14-16px on a persuade page.
- Bands of left-aligned text with an empty right half; nothing overlaps;
  no full-bleed moment.
- The atmosphere, illustration or emotional anchor from the plan is missing.
- The signature or the delight is absent or reduced to a decoration.

Fix toward the plan, not away from it.

## 7. Cliche scan

Each one is a finding unless the brief or the established system earns it:

- One headline word in the accent colour -> emphasis from scale, weight,
  width, line break or italic.
- Tracked all-caps eyebrows and column headers -> sentence case, or a label
  that carries information.
- Hero plus three cards; identical rounded cards with soft shadows -> the
  artefact as hero; compositions derived from content.
- Hairline-separated text-left, list-right sections repeated -> a shape per
  section, loud and quiet alternating.
- A flat pale tint on every band -> surfaces by role, one immersive band.
- System or Inter type by habit -> type chosen by the world.
- Purple-blue gradients, neon glow, glass, gradient text -> a palette from
  the world, solid material surfaces.
- Status pills everywhere; an alert panel that repeats table rows ->
  urgency grouped in the data, status as icon plus word.
- A full select control on every row -> an inline action with a combobox
  and a suggested choice.
- Everything centred -> a strong edge.
- A page where nothing moves -> the life layer (interface-motion.md).
- Correct, voiceless copy -> the world's vernacular, states that speak.
- Invented metrics, testimonials, counts or logos -> remove; demonstrate.

## 8. Score the touched dimensions

Score 1-5; 4 means "a good studio would ship it", 5 means "this could only
be this product, and it feels alive". Anchors for 3 and 5:

- product-specificity: 3 right content in a generic frame; 5 artefact,
  type, palette and voice belong only to this subject.
- user-journey: 3 the primary path exists but competes; 5 arrival, job and
  success are one obvious path with recovery visible.
- hierarchy: 3 readable, several elements shout; 5 one dominant per
  viewport, passes the squint test.
- system-coherence: 3 mostly consistent with stray values; 5 every size,
  colour, radius, space and easing comes from the tokens.
- typography: 3 legible but habitual faces or a flat scale; 5 faces chosen
  by the world, a display moment with tension, 60-75ch measure, tuned
  tracking and numerals.
- color-contrast: 3 passes AA but timid or evenly spread; 5 dominant plus
  sharp accent from the world, colour fields used as events, AA everywhere
  including over imagery, status not by colour alone.
- layout-rhythm: 3 tidy grid with one section shape repeated; 5 layout
  derived from content, layered depth, a full-bleed moment, loud and quiet
  alternating.
- interaction-affordance: 3 controls work but look alike or sit still; 5
  every control answers hover, press and focus, changes show their cause,
  feedback within 100 ms.
- state-completeness: 3 loading, empty and error exist but are generic; 5
  every state explains itself in the product's voice and offers the next
  step without layout shift.
- responsive-composition: 3 the desktop layout shrinks; 5 each width is
  recomposed and the signature and life layer survive at 390.
- accessibility: 3 basics with gaps; 5 semantic, labelled, focus-visible,
  reduced-motion paths for every effect, zoom and contrast verified.
- content-clarity: 3 accurate but voiceless copy; 5 specific, in the
  world's vernacular, actions say what they do, nothing invented.
- resilience: 3 breaks on long strings, slow data or missing APIs; 5 long,
  short and localized content, slow fonts, failures and browsers without
  View Transitions or scroll timelines all hold.

## 9. Floor checks

Tab through: focus visible everywhere, order logical. Spot-check contrast on
muted text and on text over atmosphere or canvas. 200% zoom without loss.
The longest real string in every label. Reduced motion on: no travel,
loops stopped, everything still reachable. Each state. Fix every blocking
detector finding; give a reason for each advisory one you keep.

## 10. Write the fix list

Each finding: where (viewport, region, selector), what is wrong as seen,
why it matters to the user or the plan, and the concrete fix (a property
and value, or a structural move). Rank by impact; batch them; apply in one
round; capture once more; stop. No open-ended polish loops, and no new
direction in the fix round -- if the plan itself was wrong, say so.

Informed by impeccable (Apache-2.0), the Hyperframes skills (Apache-2.0) and
an Apache-2.0 frontend-design agent skill; ideas re-derived, not copied.
