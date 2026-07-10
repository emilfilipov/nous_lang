# Lullaby Brand Guidelines

The visual identity for **Lullaby**, the compiled systems language. One system,
applied at the fidelity each surface allows: full-colour in the offline docs and
web, ANSI-approximated in the CLI, and a filled icon for OS/app/installer contexts.

> Personality: **warm & friendly**. Lullaby is a serious, fast, memory-safe systems
> language with a calm, gentle surface. The brand leans into that contrast — soft to
> meet, solid underneath.

Canonical assets live in [`assets/brand/`](../assets/brand/). Do not re-draw the
identity by hand; reuse these files or regenerate the raster icons with
`python assets/brand/render_icons.py`.

## The wordmark

The identity is a **plain wordmark**: the lowercase word **"lullaby"**, always
lowercase, tightly tracked, set in **Nunito ExtraBold (800)** and tinted **lavender
`#8b6ff0`** (`#c9bafd` on dark grounds). There is no pictorial mark, crescent, or
star — the word itself is the logo.

- **Primary — the wordmark.** [`lullaby-mark.svg`](../assets/brand/lullaby-mark.svg),
  the lowercase `lullaby` in lavender `#8b6ff0`. Use everywhere a lockup fits: docs
  headers, the web, inline, and the CLI (as a lavender/ANSI-tinted lowercase
  `lullaby`). Recolour to plum ink `#372a54` only where a mono, single-ground lockup
  is needed.
- **`l` monogram — the small-size exception.** [`lullaby-icon.svg`](../assets/brand/lullaby-icon.svg)
  and [`lullaby.ico`](../assets/brand/lullaby.ico): a plain lowercase **`l`** (a
  rounded vertical stroke with a small foot) in **cream** on a soft lavender→sky
  tile. Use **only** for tiny square slots — the app/file icon, favicon, taskbar, and
  the installer — where the full wordmark would be illegible. Everywhere a lockup
  fits, use the wordmark.

Clear space: keep at least the cap-height of the wordmark clear on all sides. Don't
recolour the wordmark outside the palette, change its typeface or case, rotate it,
add effects, or stretch it.

## Palette

| Token | Hex | Role |
| --- | --- | --- |
| Lavender | `#c4b5fd` | Primary accent (mark ink: `#8b6ff0` light / `#c9bafd` dark) |
| Peach | `#fecaca` | Warm accent |
| Sky | `#bae6fd` | Cool accent |
| Moonglow | `#ffdca6` | Highlight |
| Cream | `#fff8ef` | Light ground / mark-on-tile |
| Plum ink | `#372a54` | Text; also the dusk ground in dark mode |

Neutrals are never pure black — text is the plum ink so the system stays warm. Dark
mode grounds on dusk plum (`#161020`–`#221a31`); the pastels stay luminous on it.

## Typography

- **Nunito** — one rounded humanist family for everything a person reads (display,
  UI, body). Weights: 400 / 600 / 700 / 800. Bundled with the toolchain
  ([`nunito.woff2`](../assets/brand/nunito.woff2), OFL) and embedded in the offline
  docs so they render identically offline, on any machine.
- **Monospace** for code: Cascadia Code / JetBrains Mono / system mono fallback.

## Voice & tagline

Friendly, plain, and reassuring. Say what happens; no jargon where a plain word will
do; errors explain the fix. A gentle bedtime metaphor is welcome but never at the
cost of clarity.

- **Tagline:** *Serious systems code. Sweet dreams.*
- Sign-off used by the CLI/first-run: *sleep easy — it's memory-safe.*

## Asset inventory

| File | Use |
| --- | --- |
| `assets/brand/lullaby-mark.svg` | Primary lavender `lullaby` wordmark |
| `assets/brand/lullaby-wordmark.png` | Raster wordmark (real Nunito ExtraBold, transparent) for GitHub/READMEs where the font is absent |
| `assets/brand/lullaby-icon.svg` | Filled `l`-monogram tile icon (app/favicon/installer) |
| `assets/brand/lullaby.ico` | Multi-size icon 16–256 (exe, installer, favicon) |
| `assets/brand/lullaby-icon-256.png`, `-512.png` | Raster app icon |
| `assets/brand/lullaby-social-card.png` | 1200×630 social-preview card (Open Graph / Twitter, GitHub repo preview) |
| `assets/brand/nunito.woff2` | Bundled body/display typeface (OFL) |
| `assets/brand/render_icons.py` | Regenerates the raster icons from the geometry |
| `assets/brand/render_wordmark.py` | Regenerates the raster wordmark from Nunito ExtraBold |
| `assets/brand/render_social_card.py` | Regenerates the social-preview card |

The colour tokens, tagline, and lockups are previewed in the visual-identity board
(shared separately); this document is the source of truth for hex values and usage.
