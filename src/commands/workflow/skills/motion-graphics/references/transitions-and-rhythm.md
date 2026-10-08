# Transitions and rhythm

Rhythm is the contrast between fast and slow. Transitions are the grammar
that joins beats, and they carry energy: a cut on the beat, a mask that
wipes, a shape that becomes the next scene. A film with one tempo and
dim-and-fade between scenes reads as slides, however polished each slide
is.

## Write the rhythm first

- Before timings, write the rhythm in words, for example
  "fast-fast-SLOW-fast-build-settle". Then assign seconds.
- Beat grid: at 120 bpm a beat is 0.5 s and a half-beat 0.25 s. Cuts,
  floods, stamps and word hits land on grid multiples, even without music.
  With a supplied music track, cut on the beats `hyperframes beats` writes.
- The slowest scene runs about 3x the fastest.
- One idea per film. A 15 s piece that tries to say five things feels like
  noise: one argument, at most three supporting facts.

## Beat sheets

Times in seconds. Each beat names what is on screen, its verb, and how it
leaves. No beat holds still longer than about 0.75 s except the final
settle.

6 s -- bumper or looping GIF, rhythm "fast-SLOW-build", one transition
type:

| Time | Beat |
| --- | --- |
| 0-0.5 | Hook: type or object already in motion, first hit on beat one |
| 0.5-3.5 | The turn: one signature move with the camera |
| 3.5-5.0 | End card builds |
| 5.0-6.0 | Final settle, or flow back into frame one |

12 s -- launch clip, rhythm "fast-fast-SLOW-fast-build-settle", two or
three transition types:

| Time | Beat |
| --- | --- |
| 0-0.5 | Cold open: already moving, a word hits on the first beat |
| 0.5-3.0 | Problem: 3-5 hits on the grid, kinetic type leading |
| 3.0-3.5 | Held breath: the one near-still moment before the turn |
| 3.5-7.0 | Turn: the signature move -- camera travel, a colour flood on the beat |
| 7.0-9.5 | Payoff: the result and its consequence (UI may appear here, as a prop) |
| 9.5-11.0 | End card builds: wordmark constructs, line lands with weight |
| 11.0-12.0 | Final settle; the last frame is a poster |

15 s -- as 12 s, with a 3 s proof beat (one concrete demonstration) between
turn and payoff; at most three transition types.

30 s -- two or three transition types, repeated:

| Time | Beat |
| --- | --- |
| 0-2 | Hook |
| 2-8 | Problem: three 2 s hits on hard cuts, type leading |
| 8-8.5 | Held breath |
| 8.5-15 | Turn: signature move with a long camera travel |
| 15-24 | Proof: three 3 s demonstrations, same transition each time |
| 24-26.5 | Payoff |
| 26.5-28.8 | End card builds |
| 28.8-30 | Final settle |

## Transition recipes

Pick two or three types per film and repeat them so they become grammar.
Every transition lands on the beat grid.

- Hard cut: on action (mid-motion), where motion vectors match. Use for
  three or more quick, tempo-matched switches.
- Whip pan: outgoing `x: 0 -> -400`, `blur 0 -> 24px` (full frame; at most
  10px on text-only layers), 0.3 s `power3.in`; cut; incoming
  `x: 400 -> 0`, `blur 24px -> 0`, 0.3 s `power3.out`. Same axis,
  direction and speed on both sides.
- Zoom-through: push into a detail that becomes the next scene -- outgoing
  `scale 1 -> 1.2` in 0.2 s `power3.in`; incoming `scale 0.75 -> 1` in
  0.5 s `expo.out`. The detail is meaningful: a number, a window, a letter.
- Type-through: the camera flies into a letter's counter (the hole of an
  O, an e, a 0); its inside becomes the next scene's ground. Scale the word
  8-20x around the counter's centre over 0.5-0.7 s `expo.in`, cut inside
  the counter, continue with the new scene settling from scale 1.1.
- Colour flood: a full-frame field of the accent or dominant wipes across
  on the beat (`clip-path: inset(0 100% 0 0)` to `inset(0)` or a circle
  growing from the carrier, 0.35-0.5 s `expo.inOut`); the next scene is
  revealed inside it, often with the palette flipped (dark to light).
- Mask or wipe: `clip-path` inset, circle or polygon, 0.4-0.6 s
  `power3.inOut`. Make the wipe edge an object from the world: a scan line,
  a page edge, a door, a shutter.
- Morph: one shape becomes another -- a `clip-path: polygon()` with the same
  number of points on both sides, or two SVG paths with matching command
  counts, 0.5-0.8 s `power3.inOut`; or the cheap version, two shapes inside
  one shared mask, the outgoing scaling down as the incoming scales up.
- Match cut with a carrier: one element persists across the cut at the same
  position and size, then takes its new role (a counter's last digit
  becomes the first digit of the next scene's number; a cursor block
  becomes a letter of the wordmark). Build the carrier on the root, outside
  both scenes, and tween it across the boundary while the scenes swap
  beneath it.
- Never dim-and-fade. A crossfade only with a carrier visible through it;
  fade to black only at the very end, if at all.

Exit determines entry: same axis, same direction, matched speed. Pair a
`power4.in` exit with a `power4.out` entry over the same distance and
duration, and cut mid-motion, at peak velocity.

Direction is grammar: choose one dominant direction (left to right for time
and progress) and reserve the opposite for meaning (failure, rewind, loss).

## Camera

- Build a `#camera` wrapper around one large stage (the world, see
  concept-and-world.md) and move it; never fake camera moves by animating
  every element.
- Push-in: scale 1 -> 1.06-1.15 across a scene, `sine.inOut` or
  `power1.inOut`, for focus and tension.
- Travel: x and y across the stage to the next region, 0.6-1.2 s
  `expo.inOut` or `power3.inOut`, with a blur peak of 6-12px at the
  midpoint for fast travel.
- Parallax: put each depth layer in its own wrapper and move them by 0.3x
  (background), 0.6x (midground) and 1x (foreground) of the camera's
  travel, all on the same timeline position.
- Rack focus: blur the background 0 -> 6px over 0.4 s as the foreground
  lands.
- Impact: a 4-8px nudge for 4-6 frames with `steps` on a stamp or a
  landing; once or twice per film.

## Causal chains

Let events cause each other: a coin drops, the jar fills, the lid snaps
shut, the jar slides into a row of full jars. Click, squash, release,
flight, impact, recoil, reveal. Causality reads as story; parallel fades
read as slides.

## Kinetic type as the lead actor

Type is the protagonist for most of a film; UI and objects support it.

Split words into masked spans once, synchronously, before building the
timeline (escape any `<` or `&` in the source text first):

```js
for (const el of document.querySelectorAll("[data-split]")) {
  el.innerHTML = el.textContent.trim().split(/\s+/)
    .map((w) => `<span class="mask"><span class="w">${w}</span></span>`).join(" ");
}
```

```css
.mask { display: inline-block; overflow: hidden; vertical-align: top; padding-bottom: 0.08em; }
.w { display: inline-block; }
```

- Word rise: `.w` from y 60-80px (anchor words) or 40-50px, 0.5-0.7 s
  `power4.out`, 0.06-0.1 s between words, hits on the half-beat grid.
- Scale jumps: the line at statement size, then the key word cuts to 2-3x
  on the beat while the rest steps back; emphasis by time and scale,
  never by colouring one word.
- Per-letter build: letters land with `back.out(1.2-1.4)` and a 0.03-0.05 s
  stagger, each from a different small offset, for the words that matter.
- Type becomes object: a letter's stroke extends into a line that becomes
  an axis, a path or a horizon; a counter opens into a window the camera
  flies through; a word fills, cracks, stacks or empties to show what it
  says.
- Swap in place with a slot roll: the old word to y -100%, the new from
  +100%, 0.35 s `power3.inOut` ("draft" becomes "final").
- Typing: reveal characters with `steps(N)` at 18-30 characters per
  second; a caret blinking every 0.53 s is a loop with meaning.
- Counters: a proxy tween with tabular numerals, `power2.out` for a value
  settling or `steps` for a ticking display.

## Holds

Text holds follow the read-time rule in motion-principles.md and keep
living micro-motion underneath. A 0.3-0.75 s held breath before the climax
makes the climax land. The final settle lasts 1-1.5 s. Never change the
film's duration just to hide a timing defect -- fix the beat instead.

Informed by the Hyperframes skills (heygen-com/hyperframes, Apache-2.0);
ideas re-derived, not copied.
