# Tokens CSS

`brand/tokens.css` is the single source of the brand's values. Web pages
link or import it; motion compositions copy it. Nothing else declares a
brand colour, family, size, radius, duration or ease: pages and films
refer to roles (`var(--color-accent)`), so a token change reaches every
surface.

## Template

The values below are a fictional ceramics studio (porcelain ground,
celadon brand colour, cobalt accent) and only show the shape. Replace every
value from evidence and mark unapproved identity values `/* proposed */`.

```css
/* brand/tokens.css -- <Brand> design tokens. Guide: brand/BRAND.md.
   Matches the guide's changelog row of <YYYY-MM-DD>. */

/* Fonts: files in brand/assets/fonts/, resolved relative to this file. */
@font-face {
  font-family: "<Display family>";
  src: url("assets/fonts/display-latin-600-normal.woff2") format("woff2");
  font-weight: 600; font-style: normal; font-display: swap;
}
@font-face {
  font-family: "<Text family>";
  src: url("assets/fonts/text-latin-400-normal.woff2") format("woff2");
  font-weight: 400; font-style: normal; font-display: swap;
}
@font-face {
  font-family: "<Text family>";
  src: url("assets/fonts/text-latin-600-normal.woff2") format("woff2");
  font-weight: 600; font-style: normal; font-display: swap;
}

:root {
  /* Colour roles: light-dark(light, dark), resolved by each element's
     color-scheme. Pages follow the visitor; a surface or film can force
     one with color-scheme: light or dark. */
  color-scheme: light dark;
  --color-bg: light-dark(oklch(0.975 0.006 90), oklch(0.19 0.01 250));
  --color-surface: light-dark(oklch(0.995 0.004 90), oklch(0.23 0.012 250));
  --color-ink: light-dark(oklch(0.24 0.02 250), oklch(0.93 0.01 90));
  --color-ink-muted: light-dark(oklch(0.47 0.02 250), oklch(0.74 0.015 90));
  --color-line: light-dark(oklch(0.88 0.01 90), oklch(0.34 0.012 250));
  --color-brand: light-dark(oklch(0.62 0.06 165), oklch(0.70 0.06 165));
  --color-accent: light-dark(oklch(0.48 0.17 262), oklch(0.70 0.14 262));
  --color-accent-ink: light-dark(oklch(0.48 0.17 262), oklch(0.76 0.12 262));
  --color-focus: light-dark(oklch(0.55 0.16 262), oklch(0.78 0.12 262));
  --color-success: light-dark(oklch(0.50 0.12 150), oklch(0.75 0.12 150));
  --color-warning: light-dark(oklch(0.60 0.13 75), oklch(0.80 0.12 80));
  --color-danger: light-dark(oklch(0.52 0.17 28), oklch(0.72 0.14 28));
  --color-info: light-dark(oklch(0.52 0.12 245), oklch(0.76 0.10 245));

  /* Families */
  --font-display: "<Display family>", Georgia, serif;
  --font-text: "<Text family>", "Helvetica Neue", Arial, sans-serif;
  --font-mono: "<Mono family>", ui-monospace, Menlo, monospace;

  /* Type scale: ratio 1.25 from an 18px body */
  --text-small: 0.875rem;
  --text-body: 1.125rem;
  --text-lede: 1.5rem;
  --text-h3: 1.75rem;
  --text-h2: 2.25rem;
  --text-h1: clamp(2.5rem, 1.6rem + 3vw, 3.5rem);
  --text-display: clamp(3.5rem, 2rem + 9vw, 11rem);
  --leading-body: 1.55;
  --leading-heading: 1.1;
  --leading-display: 0.92;
  --tracking-display: -0.03em;
  --tracking-label: 0.06em;

  /* Spacing: 4px base */
  --space-1: 4px;  --space-2: 8px;  --space-3: 12px; --space-4: 16px;
  --space-5: 24px; --space-6: 32px; --space-7: 48px; --space-8: 64px;
  --space-9: 96px; --space-10: 128px;

  /* Radii, borders, elevation */
  --radius-s: 4px; --radius-m: 8px; --radius-l: 16px;
  --border-width: 1px;
  --shadow-1: 0 1px 2px oklch(0.24 0.02 250 / 0.08);
  --shadow-2: 0 1px 2px oklch(0.24 0.02 250 / 0.08),
    0 8px 24px -4px oklch(0.24 0.02 250 / 0.12);

  /* Interface motion; the GSAP name with the same curve in comments */
  --dur-press: 120ms;
  --dur-small: 180ms;
  --dur-medium: 260ms;
  --dur-large: 340ms;
  --ease-out: cubic-bezier(0.22, 1, 0.36, 1);       /* power4.out */
  --ease-out-strong: cubic-bezier(0.16, 1, 0.3, 1); /* expo.out */
  --ease-in: cubic-bezier(0.5, 0, 0.75, 0);         /* power3.in */
  --ease-move: cubic-bezier(0.65, 0, 0.35, 1);      /* power2.inOut */

  /* Film motion: plain numbers (seconds) and GSAP ease names, read by
     compositions with getComputedStyle */
  --film-beat: 0.5;
  --film-enter: 0.6;
  --film-exit: 0.45;
  --film-travel: 0.9;
  --gsap-enter: power4.out;
  --gsap-exit: power3.in;
  --gsap-move: power2.inOut;

  /* Video type at 1080p. A composition sets --frame-scale on :root
     (frame height / 1080) after importing this file. */
  --frame-scale: 1;
  --video-statement: calc(160px * var(--frame-scale));
  --video-headline: calc(96px * var(--frame-scale));
  --video-body: calc(36px * var(--frame-scale));
  --video-label: calc(24px * var(--frame-scale));
}
```

Rules for the file:

- Roles, not palettes: name a token by what it does (`--color-accent`),
  never by its hue (`--blue-500`). A page that needs a new role adds it here
  and to the guide's colour table, with a changelog row.
- Every text and background pair the guide lists is checked in both schemes
  (4.5:1 text, 3:1 UI); write the ratio in the guide's table.
- `light-dark()` needs a current browser (2024 or later); if the product
  must support older ones, declare the dark values again inside
  `@media (prefers-color-scheme: dark)` instead.
- Durations stay in ms for CSS and in plain seconds for films, so neither
  side has to convert units.

## Using it in a web page

- Static page: `<link rel="stylesheet" href="/brand/tokens.css">` (or the
  relative path from the page). The font URLs inside resolve relative to
  tokens.css, so `brand/assets/` must be served next to it. When the site's
  public folder does not include `brand/`, a build step copies `brand/`
  into it; never fork the values.
- Bundled app: `import "../brand/tokens.css";` once at the entry point.
  Utility frameworks map their theme to the variables
  (`colors: { accent: "var(--color-accent)" }`).
- Own styles reference roles only: `background: var(--color-bg); color:
  var(--color-ink); font: var(--text-body)/var(--leading-body)
  var(--font-text);`.

## Using it in a motion project

The renderer serves only the project folder, so copy the brand in before
snapshots and renders, from the repository root:

```sh
mkdir -p motion/<slug>/brand
cp brand/tokens.css motion/<slug>/brand/tokens.css
cp -R brand/assets motion/<slug>/brand/
```

- Add `brand/` to the project's `.gitignore`: it is a derived copy,
  refreshed before every render, never edited there.
- Write in `direction.md` which guide changelog date the copy matches.
- Link it in the composition's head, then set the frame scale and the
  scheme the film uses:

```html
<link rel="stylesheet" href="brand/tokens.css">
<style>
  :root { --frame-scale: 1; color-scheme: dark; } /* 0.375 on a 720x405 canvas */
  .headline { font: 600 var(--video-headline)/0.95 var(--font-display);
    color: var(--color-ink); letter-spacing: var(--tracking-display); }
</style>
```

- Read film timings and eases from the tokens in the timeline script
  (`tl` is the composition's paused timeline):

```js
const css = getComputedStyle(document.documentElement);
const token = (name) => css.getPropertyValue(name).trim();
tl.fromTo(".headline", { y: 80 }, {
  y: 0, duration: Number(token("--film-enter")), ease: token("--gsap-enter"),
}, Number(token("--film-beat")));
```

- A canvas or a GSAP colour tween needs a resolved colour, not the
  `light-dark()` text of the variable:

```js
const resolveColor = (name) => {
  const probe = document.createElement("i");
  probe.style.color = `var(${name})`;
  document.body.append(probe);
  const value = getComputedStyle(probe).color;
  probe.remove();
  return value;
};
```

- When the brand lives in another repository, copy from that checkout's
  paths named in the stub, and record its ref with the date.
