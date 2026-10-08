# Motion principles

Video is read in time. A viewer cannot pause, scroll back or zoom, so every
element must arrive with intent, stay long enough to be read, and leave
with purpose. "Everything fades in, then dims out" is a slideshow, not
motion design.

## Every element gets a verb

Before animating, write a verb for each element in the beat sheet. If you
cannot name the verb, the element is not designed yet. Recipes at 1080p:

| Verb | Recipe |
| --- | --- |
| rises | y 60-80px (anchor words) or 40-50px (others) to 0 behind a line mask, 0.5-0.7 s `power4.out` |
| slides in | x 120-240px to 0 along the film's dominant direction, 0.5 s `power3.out` |
| draws | SVG `stroke-dashoffset` from path length to 0, 0.6-1.2 s `power2.inOut`; rules `scaleX` 0 to 1 from the start edge, 0.5 s `power3.out` |
| wipes on | `clip-path: inset(0 100% 0 0)` to `inset(0 0 0 0)`, 0.4-0.6 s `power3.inOut` |
| stamps | scale 1.3 to 1 in 0.25 s `power4.out`, opacity 0 to 1 in 0.12 s |
| counts | a proxy object tweened 0 to N, `Math.round` into tabular numerals, `power2.out` or `steps(N)` |
| types | characters revealed with `steps(N)` at 18-30 characters per second |
| fills | a bar `scaleX` 0 to value from the start edge, `power2.out`, colour shifting with value |
| splits | characters staggered 0.02-0.03 s each, only for words of 12 characters or fewer |
| pushes | the camera wrapper scales 1 to 1.06-1.15 across the scene, `sine.inOut` |
| ejects / catches | an object leaves one container with `power3.in` and lands in another with `power3.out`, same speed at the hand-over |

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
- `expo.inOut`: whip-fast camera moves between regions.
- `steps(N)`: typing, ticking counters, clocks, mechanical displays.
- `none` (linear): only continuous mechanical motion tied to time, such as a
  dial at constant speed or a scrolling tape.
- `back.out(1.2-1.7)`: a playful pop, transforms only. Avoid `bounce` and
  `elastic`; smooth beats bouncy.

Choose about three easing characters for a film (for example `power4.out`
arrivals, `power3.in` exits, `expo.inOut` camera) and vary distance,
duration and direction within them. In one scene, at most two tweens share
the identical ease, duration and offset.

## Durations

At 30 fps, 0.1 s is 3 frames.

- Fast 0.15-0.3 s: small elements, labels, cuts in a tempo run.
- Medium 0.3-0.5 s: words, cards, icons.
- Slow 0.5-0.8 s: hero type, large shapes, transitions.
- Very slow 0.8-2 s: camera pushes, ambient drift, line drawings.

Element entrances last 0.8 s at most (line and path draws, camera moves and
ambient drift are exempt and follow the table above); exits take about 75% of their entrance. Do
not default to 0.4-0.5 s for everything: rhythm comes from contrast, and
the slowest scene should run about 3x the fastest.

## Stagger, overlap and follow-through

- Stagger by importance: the first thing to appear is read as the most
  important. 0.05-0.12 s per item, total under 0.5 s. Use GSAP's `stagger`
  (`{ each: 0.08, from: "start" }`), not hand-written delays.
- Offset a scene's first tween 0.1-0.3 s after the scene starts, so the cut
  lands before the motion.
- Overlap: start the next tween 0.1-0.3 s before the previous ends
  (`"-=0.2"`); sequential, non-overlapping tweens feel robotic.
- Secondary elements trail the primary by 0.08-0.15 s.
- Follow-through: after a large move, settle 2-4% in scale or 4-8px in
  position with `power2.out`.
- Anticipation: before a big move, 0.1-0.2 s of counter-move at 2-5% of the
  distance.
- Overshoot only on transforms; put opacity on a separate `power2.out`
  tween that completes within the first 60% of the move.

## Scene arc

Each scene builds during its first 30%, breathes from 30-70% (the viewer
reads; a small secondary move or camera push keeps it alive), and resolves
in the last 30% (the exit begins or a carrier hands over).

## Holds and read time

- Text on screen for 3 s must be readable in 2.
- Hold each line fully still and legible for at least
  `max(1.5 s, words / 3.5 + 0.5 s)`: a 6-word line needs about 2.2 s.
- Fewer words beat longer holds: one statement of up to 8 words per beat.

## The pause test

Pause on any frame: something meaningful is mid-flight, or a line is being
read inside its hold. Planned stillness is allowed in exactly three places:

- a read hold (above),
- a held breath of 0.3-0.75 s just before the climax,
- the end-card hold.

A scene that finishes entering with seconds left over is a planning bug:
cut its time or give it another beat.

## Ambient motion: the rule

Background decoratives may move only when the movement means something in
the subject's world -- light travelling as time passes, water for a place,
grain for film -- and then slowly across the whole scene: at most 2-4% in
scale or 40px in travel at 1080p. Foreground text and UI never idle-loop;
wobble, float and pulse loops simulate activity without meaning and fail
the pause test. Every ambient move lives on the timeline with a finite
duration.

## Deterministic, seekable timelines

The renderer seeks to arbitrary times and captures frames, so:

- One paused `gsap.timeline()` registered at `window.__timelines["<id>"]`;
  nothing that runs on its own clock.
- No `Math.random()`, `Date.now()` or `performance.now()`. For scatter or
  noise, use a seeded generator (for example mulberry32 with a fixed seed).
- No `repeat: -1` unless the root has a finite `data-duration`; anything
  after `data-duration` is cut off.
- Animate `x`, `y`, `scale`, `rotation`, `opacity`; never `width`,
  `height`, `top` or `left`; never tween `display` or `visibility`.
- Use `fromTo`, not `from`: `from` reads the current state, which is wrong
  after a seek.
- Do not put a CSS transform and a GSAP tween on the same property of the
  same element; wrap the element and animate the wrapper.
- Derive per-frame values (a counter, a gauge colour, a rig's joint
  angles) from a tween on a proxy object whose update applies them; never
  from timers, `requestAnimationFrame` or the previous frame.

Informed by the Hyperframes skills (heygen-com/hyperframes, Apache-2.0);
ideas re-derived, not copied.
