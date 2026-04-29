# tak-rs brand

The mark and wordmark for tak-rs. Built ground-up — no off-the-shelf
icons, no AI-generated raster art, no font licenses to worry about.
Everything here is hand-authored SVG that scales clean from 16 px
favicon to 8 K display.

## Files

| File | Use |
|---|---|
| `mark.svg` | Square mark, light surfaces. README headers, GitHub avatar, business cards. |
| `mark-inverse.svg` | Mark for dark surfaces. Slide decks, terminal-themed sites. |
| `wordmark.svg` | Mark + `tak.rs` lockup, light surfaces. README hero, presentations. |
| `wordmark-inverse.svg` | Lockup for dark surfaces. |
| `banner.svg` | Wide hero (16:5), light surface. README top, OpenGraph. |
| `favicon.svg` | 32 × 32 reticle-only mark. Favicon + tab icons. |

## Concept

The mark is one sentence: **the target is Rust**.

- **Hexagonal frame** — operator-insignia heritage. Echoes the
  mil-spec aesthetic of TAK without copying it. Flat-top
  orientation reads as institutional, structural, modern.
- **CoT reticle** — the cursor-on-target glyph at the heart of
  the TAK protocol. Outer ring + four inset arms.
- **Center target in Rust orange** — the single asymmetric
  statement. Every other element is graphite. The eye lands
  on the orange dot and the design's argument lands with it.

The wordmark mirrors the mark: `tak` and `rs` rendered in equal
weight, separated by a Rust-orange disc that's the same idea as
the mark's center target at a different scale. Reading across the
lockup, you see two orange dots saying the same thing.

## Color tokens

| Token | Hex | Use |
|---|---|---|
| `--ink` | `#0B1014` | Primary structure on light surfaces |
| `--bone` | `#F2EFEA` | Light surface; pairs with rust without competing |
| `--rust` | `#CE422B` | Single asymmetric accent; the rust-lang.org orange |
| `--slate` | `#1A2027` | Secondary text, muted captions |
| `--graphite` | `#5A6470` | Tertiary text, fine print |

The palette is intentionally narrow. **Three colors carry the
brand: ink, bone, rust.** Slate and graphite are typographic only.

## Typography

The wordmark sets in **`ui-monospace`** with the system stack as
fallback:

```css
font-family: ui-monospace, "SF Mono", "Cascadia Mono",
             "JetBrains Mono", Menlo, Consolas, monospace;
font-weight: 700;
letter-spacing: -0.04em;
```

Monospace is on-brand for a Rust kernel: terminal heritage,
consistent character cells, no proportional drift across renders.
The slight negative letter-spacing tightens the lockup so it reads
as a single mark instead of three glyphs.

The tagline uses the same family at 500 weight, 0.16 em positive
letter-spacing, uppercase — a small string carries banner weight
when spaced.

## Geometry

All marks share a unit-grid:

- viewBox 256 × 256, center (128, 128)
- Hex vertex radius: 96, flat-top
- Reticle ring radius: 66
- Reticle arm: r = 22 (inner) → r = 58 (outer)
- Center target radius: 11
- Stroke discipline: hex 12 (heaviest, the frame), reticle 9
  (secondary, the instrument)

These ratios were chosen for clean pixel-snap at 32 px and 64 px
renders without anti-aliasing artifacts on integer DPR displays.

## Don'ts

- Don't recolor the rust accent. The asymmetric statement only
  works in `#CE422B`. Other oranges (Mozilla, Hacker News, etc.)
  will visually muddle the parentage.
- Don't tighten the reticle's center gap. The arms intentionally
  do not touch the center disc — the negative space is part of
  the mark.
- Don't add taglines inside the mark. The mark stands alone; the
  tagline lives in the wordmark or banner only.
- Don't apply effects (shadow, gradient, bevel). The mark is
  flat. Period.
- Don't substitute the period in `tak.rs` with a real glyph.
  The Rust-orange disc is the period — that's the whole reason
  the lockup works.

## Provenance

Designed for tak-rs in 2026. Free to use under the project's
license (MIT OR Apache-2.0) for any tak-rs-related communication.
For derivative work or third-party use, attribute "tak-rs brand
mark."
