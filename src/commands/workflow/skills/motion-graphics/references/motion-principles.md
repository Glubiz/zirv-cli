# Motion principles

Video is read in time. A viewer cannot pause, scroll back or zoom, so every
element must arrive with intent, stay long enough to be read, and leave
with purpose. "Everything fades in, then dims out" is a slideshow, not
motion design; so is a film where panels slide in and then sit still.

## Every element gets a verb

Before animating, write a verb for each element in the beat sheet. If you
cannot name the verb, the element is not designed yet. Recipes at 1080p:

| Verb | Recipe |
| --- | --- |
| rises | y 60-80px (anchor words) or 40-50px (others) to 0 behind a line mask, 0.5-0.7 s `power4.out` |
| lands | arrives with 4-8% overshoot and settles (recipe below) |
| slides in | x 120-240px to 0 along the film's dominant direction, 0.5 s `power3.out` |
| draws | SVG `stroke-dashoffset` from path length to 0, 0.6-1.2 s `power2.inOut`; rules `scaleX` 0 to 1 from the start edge, 0.5 s `power3.out` |
| wipes on | `clip-path: inset(0 100% 0 0)` to `inset(0 0 0 0)`, 0.4-0.6 s `power3.inOut` |
| stamps | scale 1.3 to 1 in 0.25 s `power4.out`, opacity 0 to 1 in 0.12 s, a 4-6px camera nudge on impact |
| floods | a colour field wipes across the whole frame on a beat, 0.35-0.5 s `expo.inOut` |
| becomes | a word or shape turns into an object or into the next word: scale, mask and position matched across the change |
| counts | a proxy object tweened 0 to N, `Math.round` into tabular numerals, `power2.out` or `steps(N)` |
| types | characters revealed with `steps(N)` at 18-30 characters per second |
| splits | characters staggered 0.02-0.03 s each, only for words of 12 characters or fewer |
| pushes | the camera wrapper scales 1 to 1.06-1.15 across the scene, `sine.inOut` |
| travels | the camera moves to another region of the world, 0.6-1.2 s `expo.inOut` or `power3.inOut` |
| hands over | an object leaves one place with `power3.in` and arrives in another with `power3.out`, same speed at the hand-over |

Mix at least three verbs and three directions across a film. Uniform
`y: 30, opacity: 0` on everything is the web habit that makes video flat.

## Ease vocabulary (GSAP)

- `.out` for entrances (fast start, gentle landing), `.in` for exits
  (gentle start, fast departure), `.inOut` for moves between two on-screen
  positions and camera moves.
- `power3.out`: the house default entrance.
- `power4.out`, `expo.out`: dramatic arrivals, hero words, things that land
  with weight.
- `power3.in`, `power4.in`: exits and the outgoing half of a transition.
- `power2.inOut`, `sine.inOut`: calm moves, camera drift, breathing.
- `expo.inOut`: whip-fast camera moves between regions, colour floods.
- `steps(N)`: typing, ticking counters, clocks, mechanical displays.
- `none` (linear): only continuous mechanical motion tied to time, such as a
  dial at constant speed or a scrolling tape.
- `back.out(s)` overshoots by a measurable amount: 1.0 about 4%, 1.2 about
  5%, 1.4 about 7%, 1.7 about 10%. Hero moves use 1.0-1.5; transforms only.
  Never `bounce` or `elastic`.

Choose about three easing characters for a film (for example `power4.out`
arrivals, `power3.in` exits, `expo.inOut` camera) and vary distance,
duration and direction within them. In one scene, at most two tweens share
the identical ease, duration and offset.

## Every hero move: anticipation, overshoot, follow-through, settle

The moves that carry the story -- the hero word, the key object, the
wordmark -- get all four. Small supporting moves get a clean `power3.out`.

```js
// key object already on screen moves to its new place; ">" = right after the previous tween
tl.to(obj, { x: "-=24", duration: 0.14, ease: "power2.in" }, t)               // anticipation, ~5% back
  .to(obj, { x: 480, scale: 1.06, duration: 0.42, ease: "power4.out" }, ">")  // overshoot 6%
  .to(obj, { scale: 1, duration: 0.3, ease: "power2.inOut" }, ">")            // settle
  .fromTo(obj.querySelectorAll(".part"), { x: -18 }, { x: 0, duration: 0.36,
    ease: "back.out(1.4)", stagger: 0.04 }, "<-0.2");                         // follow-through: parts drag, catch up
// an arriving word: one tween with a measured overshoot
tl.fromTo(word, { y: 90, scale: 0.92 }, { y: 0, scale: 1, duration: 0.55, ease: "back.out(1.3)" }, t2)
  .fromTo(word, { opacity: 0 }, { opacity: 1, duration: 0.18, ease: "power2.out" }, t2);
```

- Anticipation: 0.1-0.2 s counter-move at 2-5% of the distance.
- Overshoot: 4-8% on scale or position, transforms only; opacity always on
  its own `power2.out` tween that completes within the first 60% of the
  move.
- Follow-through: attached parts (letters of a word, items in a group, a
  shadow) trail the main body by 0.04-0.12 s and settle after it.
- Settle: 0.2-0.35 s `power2.inOut` back to rest.

## Durations

At 30 fps, 0.1 s is 3 frames.

- Fast 0.15-0.3 s: small elements, labels, cuts in a tempo run.
- Medium 0.3-0.5 s: words, cards, icons.
- Slow 0.5-0.8 s: hero type, large shapes, transitions.
- Very slow 0.8-2 s: camera travel, ambient drift, line drawings.

Element entrances last 0.8 s at most (line and path draws, camera moves and
ambient drift are exempt and follow the list above); exits take about 75%
of their entrance. Do not default to 0.4-0.5 s for everything: rhythm comes
from contrast, and the slowest scene should run about 3x the fastest.

## Stagger and overlap

- Stagger by importance: the first thing to appear is read as the most
  important. 0.05-0.12 s per item, total under 0.5 s. Use GSAP's `stagger`
  (`{ each: 0.08, from: "start" }`), not hand-written delays.
- Offset a scene's first tween 0.1-0.3 s after the scene starts, so the cut
  lands before the motion.
- Overlap: start the next tween 0.1-0.3 s before the previous ends
  (`"-=0.2"`); sequential, non-overlapping tweens feel robotic.
- Secondary elements trail the primary by 0.08-0.15 s.

## Scene arc

Each scene builds during its first 30%, breathes from 30-70% (the viewer
reads while the camera drifts or a secondary element completes), and
resolves in the last 30% (the exit begins or a carrier hands over).

## Holds: legible, never frozen

- Text on screen for 3 s must be readable in 2. A line needs at least
  `max(1.5 s, words / 3.5 + 0.5 s)` of legible time: a 6-word line about
  2.2 s.
- Legible is not frozen. No hold longer than about 0.75 s is static,
  except the final settle of 1-1.5 s. Under a read hold keep living
  micro-motion: a camera drift of 1-3% scale or 20-40px across the hold,
  light travelling across the ground, a secondary element finishing its
  move, the type itself drifting 10-20px in one direction. Text never moves
  faster than about 20px per second while it is being read.
- Fewer words beat longer holds: one statement of up to 8 words per beat.

## The pause test and the density check

- Pause on any frame: something meaningful is mid-flight, or a line is
  being read while the frame around it still lives.
- Planned stillness: a held breath of 0.3-0.75 s just before the climax,
  and the final settle. Nothing else.
- Density check: snapshot every 0.5 s across the film. Each frame must
  differ meaningfully from the one before (a new element, a new scale, a
  new colour field, a camera move) -- outside the final settle. Two
  near-identical neighbours are dead air: cut the time or add a beat.
- A scene that finishes entering with seconds left over is a planning bug.

## Ambient motion: the rule

Background elements may move only when the movement means something in the
subject's world -- light travelling as time passes, a material ageing, a
crowd, weather -- and then slowly and in one direction across the whole
scene: at most 2-4% in scale or 40px in travel at 1080p. Wobble, float and
pulse loops on text or objects simulate activity without meaning and fail
the pause test; the living micro-motion under a hold is a slow one-way
drift, never a loop. Every ambient move lives on the timeline with a finite
duration.

## Deterministic, seekable timelines

The renderer seeks to arbitrary times and captures frames, so:

- One paused `gsap.timeline()` registered at `window.__timelines["<id>"]`;
  nothing that runs on its own clock.
- No `Math.random()`, `Date.now()` or `performance.now()`. For scatter or
  noise, use a seeded generator (for example mulberry32 with a fixed seed).
- No `repeat: -1` unless the root has a finite `data-duration`; anything
  after `data-duration` is cut off.
- Animate `x`, `y`, `scale`, `rotation`, `opacity` and `clip-path`; never
  `width`, `height`, `top` or `left`; never tween `display` or
  `visibility`.
- Use `fromTo`, not `from`: `from` reads the current state, which is wrong
  after a seek.
- Do not put a CSS transform and a GSAP tween on the same property of the
  same element; wrap the element and animate the wrapper.
- Derive per-frame values (a counter, a colour, a rig's joint angles) from
  a tween on a proxy object whose update applies them; never from timers,
  `requestAnimationFrame` or the previous frame.
- Split text into spans synchronously before building the timeline
  (recipe in transitions-and-rhythm.md), so every seek finds the same DOM.

Informed by the Hyperframes skills (heygen-com/hyperframes, Apache-2.0);
ideas re-derived, not copied.
