# Concept and world

The difference between a competent clip and one a studio would sign is
decided before any markup: an idea that is not literal, a world the camera
can travel through, type that performs, colour that happens on the beat,
and an ending that is built rather than shown.

## 1. Concept first: three metaphors

Write three candidate concepts for the film's one argument, from literal to
abstract:

1. Literal: the product's screens doing the thing.
2. A metaphor from the subject's physical world: an object, material or
   process that behaves the way the argument says.
3. Typographic: the argument performed by type and shape alone.

Pick the least literal concept the audience still reads within about one
second of seeing it; a typographic or metaphor film with one short literal
proof beat is often the strongest mix. Test: describe the film in one
sentence without naming any interface. If you cannot, it is still literal.

Worked example -- an expense app, "receipts sorted before you get home":

- Literal: phone screens capture a receipt and show a report.
- Metaphor: a blizzard of paper receipts swirls, then folds itself into one
  envelope that seals with a stamp.
- Typographic: the word "receipts" multiplies into a messy pile of words;
  the letters sort themselves by size into one clean line, "sorted."

Chosen: the metaphor, with the typographic sort as the turn and one 2 s
screen as proof. Write the choice and the reason in `direction.md`.

## 2. UI is a prop

- Interface on screen for at most a third of the film's running time.
- When it appears, it proves a claim: cropped to the 3-6 lines that matter,
  scaled 2-3x, entering as an object in the world (held, placed, slid in
  from the carrier) -- never a panel parked beside the text for a scene.
- Every scene built as "text left, panel right" is the web habit; vary the
  composition from beat to beat.

## 3. Build one world for the camera

Design one stage larger than the frame that holds every scene as a region,
and move a camera through it. Scenes do not appear from black: they are
already there, and the edge of the next region is visible while you travel.

```html
<div id="root" data-composition-id="main" data-duration="12"
     data-width="1920" data-height="1080">
  <div id="camera">                <!-- scale only: pushes and pulls -->
    <div class="layer far"></div>  <!-- atmosphere, moves 0.3x -->
    <div class="layer mid"></div>  <!-- regions at x 0, 1920, 3840 ... moves 1x -->
    <div class="layer near"></div> <!-- framing foreground, moves 1.4x -->
  </div>
</div>
```

```js
const travel = (x, y, at, duration = 0.9, ease = "expo.inOut") => {
  for (const [layer, k] of [[".far", 0.3], [".mid", 1], [".near", 1.4]])
    tl.to(layer, { x: -x * k, y: -y * k, duration, ease }, at);
};
travel(1920, 0, 3.5);   // to region two on the beat
travel(1920, 1080, 7);  // down to region three
```

- Size: 3x3 frames (5760x3240) or a long strip (7680x1080) is plenty.
- Depth: the far layer softer (blur 2-4px, lower contrast); the near layer
  may crop into the frame as a dark, slightly blurred foreground.
- One light direction for the whole world; continuity of objects across
  regions.

## 4. Colour as an event

- Give the palette a before and an after state that carries the argument
  (warm and cluttered to cool and ordered, dim to lit).
- Flood the frame with a colour field on a beat one to three times per film
  (transitions-and-rhythm.md), and flip contrast (dark world to light
  world, or back) at the turn.
- One accent; fields come from the palette's dominant and accent.
- Near-black with an acid accent, navy gradients and neon cyan or purple are
  the model's defaults: use them only when the subject's world is that.

## 5. Type leads

- Kinetic type is the protagonist for most of the film: statement hits at
  11-18% of frame height, scale jumps on the beat, per-word and per-letter
  builds, type that becomes an object (recipes in
  transitions-and-rhythm.md).
- A display face with character, chosen by the world (`zirv skill read
  frontend-craft references/typography.md`), at two weights far apart.
  Habitual picks (Inter, Space Grotesk, system stacks) read as a template.

## 6. The end card is built

- Built from the film's carrier: the last object becomes the wordmark,
  letters land one by one (`back.out(1.3)`, 0.04 s stagger), strokes draw,
  or a mask opens the wordmark out of the final shape.
- The line lands with weight on a beat: scale 1.12 to 1 in 0.3 s
  `power4.out`, a 4-6px camera nudge, the ground settling behind it.
- Final settle of 1-1.5 s: a 1-2% push easing out to rest. The last frame
  is still and composed as a poster: wordmark, line and one element of the
  world, anchored to edges, not a lone centred logo.
- Never a wordmark that simply fades in, centred, with an accent dot,
  beside a panel left over from the previous scene.

## 7. Every frame a poster

- Snapshot every 0.5 s across the film (`snapshot --at 0.5,1,1.5,...`) and
  review the frames as a contact sheet.
- Each frame has one clear focal point, type at video scale and a
  composition that works as a still; no muddle of half-formed entrances.
- Neighbouring frames differ meaningfully (the density check in
  motion-principles.md), except inside the final settle.
- Name the frame you would print as the poster. If none qualifies, the film
  is not finished.

## Cliches and the better move

- A film of screens -> type and world lead; UI is a prop.
- Every scene the same split -> regions of one world joined by camera
  travel.
- A literal concept -> the least literal metaphor that still reads.
- Fade in, sit, dim out -> verbs, beats, floods and carriers.
- A static end card beside a leftover panel -> an end card built from the
  carrier, then a poster frame.

Informed by the Hyperframes skills (heygen-com/hyperframes, Apache-2.0);
ideas re-derived, not copied.
