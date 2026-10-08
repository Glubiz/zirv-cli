# Brand template

The skeleton for `brand/BRAND.md`, and the pointer stub used when the brand
lives in another repository. Copy the skeleton, keep the section numbers,
and fill each line from evidence (references/discovery.md). Every rule
carries a status -- `[settled]` (backed by evidence or approved) or
`[proposed]` (awaiting the operator) -- and its source. Delete hints in
angle brackets as you fill them; write "none yet" rather than inventing.

The short examples in the hints come from a fictional ceramics studio and
only show the level of detail; never copy them into a real guide.

## The guide

```markdown
# <Brand name> brand guide

Status: <draft | identity approved YYYY-MM-DD by <name>>
Tokens: brand/tokens.css. Assets: brand/assets/. Changelog: last section.
This guide outranks the frontend profile and general design references for
everything it settles. Its don'ts beat any reference suggestion.

## 1. Identity and promise
- What it is: <one factual sentence> [settled|proposed] (source: <url/path>)
- Promise: <what the user gets, in their words, from evidence>
- Personality: <three adjectives, each with what it rules out>
  <e.g. "earthy, not rustic-cute; precise, not clinical; warm, not loud">

## 2. Audience
- Primary: <who, where and when they meet the brand, what they already know>
- Secondary: <if evidence shows one>

## 3. Voice and vocabulary
- Tone by situation: marketing <..>; product interface <..>; errors <..>;
  empty states <..>; release notes <..>.
- Person and register: <we/you, formal/plain, contractions yes/no>
- Grammar: <sentence case or title case; numerals; units; date and time
  format; how the product name is spelled and capitalised>
- Words we use / words we avoid:

  | Use | Avoid | Why |
  | --- | --- | --- |
  | <"glaze firing"> | <"process"> | <the trade's own word> |

- Sample lines (real or approved): heading <..>; button <..>; error <..>;
  empty state <..>.

## 4. Logo
- Files: brand/assets/logo/<primary.svg, mark.svg, mono-dark.svg,
  mono-light.svg> (source: <url/path>)
- Clear space: <e.g. the height of the mark's inner circle on every side>
- Minimum size: <mark 24px, wordmark 96px wide on screen; 48px tall in a
  1080p film>
- Allowed backgrounds: <roles from section 5, imagery only with a scrim>
- Misuse: never stretch, rotate, recolour outside the palette, add shadows
  or gradients, outline, rebuild the wordmark in another typeface, or place
  it on a busy image without a scrim.

## 5. Colour
| Role | Token | Light | Dark | Use | Verified contrast |
| --- | --- | --- | --- | --- | --- |
| background | --color-bg | <oklch> | <oklch> | page ground | -- |
| surface | --color-surface | | | cards, panels | -- |
| ink | --color-ink | | | body text | <x:1 on bg> |
| ink-muted | --color-ink-muted | | | secondary text | <x:1 on bg> |
| line | --color-line | | | borders, rules | <3:1 if it carries meaning> |
| brand | --color-brand | | | large fields, identity | -- |
| accent | --color-accent | | | primary action, signature only | <3:1 as UI> |
| accent-ink | --color-accent-ink | | | accent as text | <4.5:1> |
| focus | --color-focus | | | focus rings | <3:1> |
| success / warning / danger / info | --color-success ... | | | status, always with a word and icon | |

- Proportion: <e.g. ~60% neutral, ~30% brand, <=10% accent>
- Never: <forbidden pairs, e.g. accent text on brand fields>

## 6. Type
- Families: display <family, weights>; text <..>; mono <..>. Files in
  brand/assets/fonts/; licence <OFL | commercial, held by ..>; fallback
  stack <..>.
- Roles (tokens): display, h1, h2, h3, body, small, label, code, numeric --
  size, weight, line height, tracking each.
- Rules: <case, emphasis, numerals (tabular for data), maximum line length>

## 7. Layout and geometry
- Grid and widths: <columns, gutter, max content width, text measure>
- Spacing scale: <token names and values>
- Radii, borders, elevation: <tokens; when each is used>
- Density: <marketing pages vs product screens>

## 8. Imagery and illustration
- Photography: <subjects, framing, light, grading toward the palette>
- Illustration: <style, line weight, palette use>
- Never: <stock clichés, generated people, ...>
- Alt text: <rule>

## 9. Icons
- Set <name or custom>, grid <24px>, stroke <1.5px>, corners <round>,
  fill rules <outline by default, filled for selected state>.

## 10. Interface motion
- Durations and eases: tokens in section "motion" of tokens.css.
- Signature interactions: <e.g. a press settles with the spring ease>
- Reduced motion: <what changes>

## 11. Film motion
- Pace: <beat grid, e.g. 0.5 s at 120 bpm; cuts per 10 s>
- Eases: <GSAP names per role, e.g. power4.out arrivals, power3.in exits>
- Transitions: <the 2-3 types this brand uses, and the ones it never uses>
- Type at 1080p: <statement, headline, body, label sizes>
- Logo animation and end card: <how the mark builds, hold length, the
  final poster frame>
- Sound: <music character, SFX, voice; or "silent">

## 12. Accessibility commitments
- Contrast <AA 4.5:1 text, 3:1 UI>; focus <style>; reduced motion <rule>;
  targets <44px>; video <captions for every spoken line>.

## 13. Don'ts
- <The brand's explicit prohibitions. They beat any general advice.>

## 14. Open proposals
| Item | Proposal | Evidence | Alternative | Asked on |
| --- | --- | --- | --- | --- |

## Changelog
| Date | Change | Reason | Source |
| --- | --- | --- | --- |
| <YYYY-MM-DD> | Guide established from <evidence> | <request> | <commit/url> |
```

Changelog rules: one row per change, newest last; a changed token or rule
names the old value ("accent oklch(0.62 0.17 35) replaces oklch(0.58 0.19
30): failed 3:1 on the dark surface"); approvals get their own row.

## The pointer stub

When the brand is owned by another repository, `brand/BRAND.md` is only
this:

```markdown
# <Brand name> brand guide (pointer)

The brand is owned by another repository. Read the guide, tokens and assets
there; edit them only there.

Source: <git URL or path> @ <tag, branch or commit>
Guide: <path inside the source, e.g. brand/BRAND.md>
Tokens: <path, e.g. brand/tokens.css>
Assets: <path, e.g. brand/assets/>
Local additions: <product-specific rules that extend the brand, never
override it; or "none">
```

Read a pinned ref (a tag or commit) when one is named, and report which ref
you read. If the source cannot be reached, say so and stop UI or motion work
that depends on identity-level decisions rather than guessing them.
