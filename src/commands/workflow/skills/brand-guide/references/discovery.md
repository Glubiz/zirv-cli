# Discovery

How to build a brand guide from evidence: what to collect, how to extract
colours, fonts, logos and voice, how to settle conflicts, and how to ask
for approval. Nothing goes into the guide without a source; what the
evidence cannot settle is written as `[proposed]`.

## 1. Evidence checklist

Collect in this order of authority and note each source with its URL or
path and date:

1. Explicit operator statements and any existing brand document in the
   repository (a guidelines PDF, a design-system package, a `docs/brand`
   folder).
2. What ships: the production website's HTML and CSS, the app's built
   styles.
3. The source's tokens and themes: CSS custom properties, theme files,
   utility-framework config, component library overrides.
4. Logo and icon files in the repository; the site's favicon, touch icon
   and social preview image.
5. Words: README, documentation intros, interface copy, error messages,
   release notes, store listings.
6. Other surfaces: terminal interfaces (help text and colours), earlier
   films and GIFs, slide decks, printed material photographed in the repo.

Stop collecting a category once the evidence agrees; keep collecting where
it conflicts.

## 2. A live site

`zirv frontend render` blocks external hosts, so fetch with ordinary tools.
A network sandbox may need the site's hosts allowed. Work in a scratch
folder, never in the repository:

```sh
mkdir -p "$TMPDIR/brand-evidence" && cd "$TMPDIR/brand-evidence"
curl -sSL -A "Mozilla/5.0" -o index.html "https://www.example.com/"
grep -oiE '<link[^>]+>' index.html | grep -i stylesheet   # linked CSS
grep -oiE '<style[^>]*>' index.html | wc -l                # inline style blocks
```

Fetch each linked stylesheet (resolve relative URLs against the page) into
the same folder, and skip third-party hosts: chat widgets, consent banners,
analytics and font-service CSS are not brand evidence (font services are
handled in section 4).

## 3. Colours by frequency, then by role

```sh
cat *.css index.html \
  | grep -oiE '#[0-9a-f]{3,8}\b|rgba?\([^)]*\)|hsla?\([^)]*\)|oklch\([^)]*\)' \
  | tr 'A-F' 'a-f' | sort | uniq -c | sort -rn | head -30
grep -ohE -- '--[A-Za-z0-9_-]+:[^;}]+' *.css | sort | uniq -c | sort -rn | head -60
```

- Existing custom property names are the strongest role evidence
  (`--brand-primary`, `--text-muted`): keep the site's meaning, rename to
  the guide's roles in tokens.css, and note the mapping.
- Frequency in CSS is not area on screen. Assign roles by where colours are
  used: the most frequent near-white and near-black are usually ground and
  ink; the chromatic colour on buttons, links and the logo is brand or
  accent. Confirm with selectors:
  `grep -nE 'button|\.btn|a:hover|--primary' *.css`.
- Look at the page itself before settling: a local headless browser
  screenshot of the URL (not `zirv frontend render`), or the operator's
  screenshot.
- Convert every settled value to OKLCH for tokens.css, and check each
  text/ground pair for contrast (4.5:1 text, 3:1 UI) in light and dark. A
  brand colour that fails as text keeps its role for fills and gets a darker
  or lighter `-ink` variant, logged as a decision.

## 4. Fonts

```sh
grep -ohiE 'font-family:[^;}]+' *.css | sort | uniq -c | sort -rn | head
grep -ohiE 'url\([^)]+\.(woff2?|ttf|otf)[^)]*\)' *.css | sort -u
grep -oiE '<link[^>]+fonts?\.[^>]+>' index.html        # hosted font services
```

- The most frequent family on headings is the display face; on body text,
  the text face; on code, the mono face. Record weights actually used.
- A hosted font-service URL names the family and weights in its query;
  record them.
- Licence decides what goes into `brand/assets/fonts/`: open-licence
  families (SIL OFL and similar) are downloaded as woff2, for example from
  the family's `@fontsource` package (`npm pack @fontsource/<family>`,
  extract, copy the needed `files/*.woff2`). A commercial family is
  recorded with its licence holder and never copied; propose an
  open-licence fallback with similar metrics and mark it `[proposed]`.

## 5. Logo and favicon

```sh
grep -oiE '<link[^>]+rel="?(icon|apple-touch-icon|mask-icon)"?[^>]*>' index.html
grep -oiE '<meta[^>]+property="?og:image"?[^>]*>' index.html
grep -oiE '<(img|svg)[^>]*logo[^>]*>' index.html
```

- Prefer SVG. Save files into `brand/assets/logo/` with their source URL in
  the guide; an inline `<svg>` logo is copied out as its own file.
- Never redraw, trace, recolour or "clean up" a logo. If only a raster
  exists, store it, and list the missing vector under open proposals.
- Measure what the evidence shows: the mark's proportions, the colours it
  uses, the smallest size it appears at.

## 6. Voice

- Collect 10-20 real lines: headings, buttons, errors, empty states, docs
  intros, release notes. Quote them in the guide's voice section.
- Describe what they share: person (we/you), sentence length, formality,
  humour or none, recurring words, words never used.
- Derive do and don't words only from repeated evidence. One line is an
  anecdote; three are a pattern.
- For example, a community orchestra's site that always writes "concert",
  "players" and "season", and never "event" or "content", gives three use
  words and two avoid words. Its slogan is not yours to rewrite.

## 7. Other surfaces

- Terminal interfaces: the colours a CLI prints (search the source for its
  colour library or ANSI codes) and the tone of its help text.
- Earlier films, GIFs and decks: grab frames; note pace (cuts per 10 s),
  transition types, how the logo appears, the end card, music or silence.

## 8. Conflicts

Resolve in this order, and write the rule you used next to the decision:

1. The operator's statement beats any artefact.
2. What ships beats mockups and stale documents.
3. Newer beats older (check commit dates and file dates).
4. The value used across many places beats a one-off.
5. Still two candidates: propose one, list the other as the alternative.

## 9. Presenting proposals

One table, once, at the end of establishing (or with a revision diff):

| Item | Proposal | Evidence | Alternative |
| --- | --- | --- | --- |
| Accent | oklch(0.48 0.17 262) | buttons and links on 4 pages | the logo's teal |
| Display face | <family>, 600 | headings site-wide | none |

Ask for approval of identity-level items -- logo, primary colours, type
families, voice -- and anything else marked `[proposed]`. On approval,
change the status markers, add a changelog row with the date and who
approved, and update the guide's status line. Until then, work uses the
proposals and says so.

## Never

- Invent a tagline, mission, audience, metric or rule.
- Adopt values from third-party embeds as brand values.
- Copy a commercial font file or a logo from a site the operator does not
  own.
- Treat a reference's general advice as brand evidence.

Informed by impeccable (Apache-2.0) and the Hyperframes skills
(Apache-2.0); ideas re-derived, not copied.
