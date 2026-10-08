# Transitions and rhythm

Rhythm is the contrast between fast and slow. Transitions are the grammar
that joins beats. A film with one tempo and crossfades between every scene
reads as slides, however polished each slide is.

## Write the rhythm first

- Before timings, write the rhythm in words, for example
  "fast-fast-SLOW-fast-hold". Then assign seconds.
- The slowest scene runs about 3x the fastest.
- Place hits on a beat grid even without music: at 120 bpm a beat is
  0.5 s, so land important moves on 0.5 s multiples. With a supplied music
  track, cut on the beats `hyperframes beats` writes out.
- One idea per film. A 15 s piece that tries to say five things feels like
  noise: one argument, at most three supporting facts.

## Beat sheets

Times in seconds. Each beat names what is on screen, its verb, and how it
leaves.

6 s -- bumper or looping GIF, rhythm "fast-SLOW-hold", one transition type:

| Time | Beat |
| --- | --- |
| 0-0.8 | Hook: one image already in motion |
| 0.8-3.8 | The turn: one signature move, slow |
| 3.8-4.3 | Settle |
| 4.3-6.0 | Sign-off hold, or flow back into frame one |

12 s -- launch clip, rhythm "fast-fast-SLOW-fast-hold", two transition
types:

| Time | Beat |
| --- | --- |
| 0-0.6 | Cold open: something already moving |
| 0.6-3.2 | Problem: escalation in 2-3 quick hits |
| 3.2-3.7 | Held breath: near stillness before the turn |
| 3.7-7.2 | Turn: the product acts -- the signature move |
| 7.2-9.4 | Payoff: the result and its consequence |
| 9.4-12.0 | End card: lands by 10.0, holds still at least 1.5 s |

15 s -- as 12 s, with a 3 s proof beat (one concrete demonstration) between
turn and payoff; at most three transition types.

30 s -- two to three transition types, repeated:

| Time | Beat |
| --- | --- |
| 0-2 | Hook |
| 2-8 | Problem: three 2 s hits on hard cuts |
| 8-9 | Held breath |
| 9-15 | Turn: signature move with a camera move |
| 15-24 | Proof: three 3 s demonstrations, same transition each time |
| 24-26.5 | Payoff |
| 26.5-30 | End card and hold |

## Transition recipes

Pick two or three types per film and repeat them so they become grammar.

- Hard cut: on action (mid-motion), where motion vectors match. Use for
  three or more quick, tempo-matched switches.
- Whip pan: outgoing scene `x: 0 -> -400`, `blur 0 -> 24px` (full frame;
  at most 10px on text-only layers), 0.3 s `power3.in`; cut; incoming
  `x: 400 -> 0`, `blur 24px -> 0`, 0.3 s `power3.out`. Same axis,
  direction and speed on both sides.
- Zoom-through: push into a detail that becomes the next scene -- outgoing
  `scale 1 -> 1.2` in 0.2 s `power3.in`; incoming `scale 0.75 -> 1` in
  0.5 s `expo.out`. Push through something meaningful: the score digit, a
  window, a document.
- Mask or wipe: `clip-path: inset(0 100% 0 0) -> inset(0 0 0 0)` or a
  shaped mask, 0.4-0.6 s `power3.inOut`. Make the wipe edge an object from
  the world: a scan line, a tide line, a page sliding over.
- Match cut with a carrier: one element persists across the cut at the same
  position and size, then takes its new role (the old session's score
  becomes the new session's score; a cursor block becomes a letter of the
  wordmark). Build the carrier on the root, outside both scenes, and tween
  it across the boundary while the scenes swap beneath it.
- Crossfade: only with a carrier visible through it, never as default glue.
  Fade to black only at the very end, if at all.

Exit determines entry: same axis, same direction, matched speed. Pair a
`power4.in` exit with a `power4.out` entry over the same distance and
duration, and cut mid-motion, at peak velocity.

Direction is grammar: choose one dominant direction (left to right for time
and progress) and reserve the opposite for meaning (failure, rewind, loss).

## Camera

- Build a `#camera` wrapper around the stage and move it; do not fake camera
  moves by animating every element.
- Push-in: scale 1 -> 1.06-1.15 across a scene, `sine.inOut` or
  `power1.inOut`, for focus and tension.
- Pan to reveal: x by 10-30% of the frame over 1-2 s, `power2.inOut`,
  across a stage larger than the frame (for example 3840px wide).
- Whip between regions: 0.4-0.6 s `expo.inOut`, blur peaking at the
  midpoint.
- Parallax: background moves 0.3x, midground 0.6x, foreground 1x of the
  camera's travel.
- Rack focus: blur the background 0 -> 6px over 0.4 s as the foreground
  lands.
- Impact shake: plus or minus 4-8px for 4-6 frames with `steps`; once per
  film at most.

## Causal chains

Let events cause each other: a score crosses its threshold, a document
ejects, a new session catches it, the score resets. Click, squash, release,
flight, impact, recoil, reveal. Causality reads as story; parallel fades
read as slides.

## Kinetic type

- Words rise from behind a line mask: 60-80px for anchor words, 40-50px for
  the rest, 0.5-0.7 s `power4.out`, 0.06-0.1 s between words.
- Swap a word in place with a slot roll: the old word to y -100%, the new
  from +100%, 0.35 s `power3.inOut` (rot becomes fresh).
- Emphasise by time and scale: the key word arrives last, larger, or holds
  longer. Colouring one word is the web habit; timing is the video tool.
- Typing: reveal characters with `steps(N)` at 18-30 characters per second;
  a caret blinking every 0.53 s is a loop with meaning.
- Counters: a proxy tween with tabular numerals, `power2.out` for a value
  settling or `steps` for a ticking display.

## Holds

- Text holds follow the read-time rule in motion-principles.md.
- A 0.3-0.75 s held breath before the climax makes the climax land.
- The end card holds still for 1.5-2.5 s; never change the film's duration
  just to hide a timing defect -- fix the beat instead.

Informed by the Hyperframes skills (heygen-com/hyperframes, Apache-2.0);
ideas re-derived, not copied.
