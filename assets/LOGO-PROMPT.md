# keel — logo generation prompt (anthropic-art style)

The anthropic-art aesthetic is a **loose, hand-drawn raster illustration** made by an image
model. Paste the assembled prompt below into an image-generation surface, or, if you have the
skill installed: `use $anthropic-art to generate a square logo of [SUBJECT], sky-blue ground`.

Generate **1:1 square**, make 3–4, pick the best, then ask for "same but bolder / cleaner lines"
to refine. For a favicon, crop tighter to the focal shape.

---

## Shared style block (keep this for every concept)

```
USE CASE: brand logo / app icon for "keel", an agent-native version-control system
ASSET TYPE: square icon, 1:1
BACKGROUND: full-bleed opaque [ACCENT NAME] ([ACCENT HEX]) — no transparency, colored all the
  way to the corners
CARRIER: one irregular, hand-torn ivory-white shape (#FAF9F5) occupying ~65–75% of the frame,
  materially distinct from the background, slightly asymmetric
STYLE / MEDIUM: flat editorial illustration; thick hand-drawn ink strokes with organic wobble;
  woodcut / brush-pen feel; confident, imperfect, human — NOT geometric
PALETTE: exactly three — near-black ink (#141413) + ivory (#FAF9F5) + one accent. Nothing else.
COMPOSITION: one focal cluster, 65–80% of the frame, centered, legible at thumbnail size
TEXT: none
AVOID: gradients, drop shadows, 3D, photorealism, corporate vector gloss, perfect geometry,
  clip-art, thin uniform lines, symmetry that looks mechanical, white or transparent background
```

**Accent options** (pick one): clay `#D97757` · sky blue `#6E9EC9` · cactus `#7C8B69`
· heather `#8F82B4` · fig `#C27BA0` · oat `#E3DACC`

---

## Concept A — the keel-beam (RECOMMENDED: most literal to the name, most distinctive)

```
SUBJECT (near-black #141413 hand-drawn ink): the keel and ribs of a wooden boat under
  construction, seen from the side — one long, gently curved backbone beam running across the
  lower third, with a row of curved ribs/frames rising from it like a ship's skeleton. Bold,
  uneven ink strokes; a couple of ribs slightly wonky. Reads unmistakably as a spine / backbone
  / the structure everything is built on.
```

## Concept B — anchor (safe, iconic, unmistakably nautical)

```
SUBJECT (near-black #141413 hand-drawn ink): a single bold ship's anchor — ring at top, thick
  shank, curved arms and pointed flukes, a stock across the top — drawn in loose, imperfect ink,
  slightly asymmetric, hand-inked rather than vector-clean.
```

## Concept C — fin keel beneath the waterline

```
SUBJECT (near-black #141413 hand-drawn ink): a sailboat's fin keel with a rounded ballast bulb
  hanging beneath a loose, wavy hand-drawn waterline; the hull only hinted as one simple curved
  shape at the surface. The keel below the water is the hero — the hidden structure that keeps
  the boat upright.
```

## Concept D — helm wheel (steering / staying on course)

```
SUBJECT (near-black #141413 hand-drawn ink): a ship's helm wheel — a spoked wooden steering
  wheel with a hub and handles — drawn with bold, uneven, hand-inked spokes, slightly
  imperfect, filling the ivory carrier.
```

---

## Fully assembled example (Concept A · clay) — copy/paste this

```
Editorial illustration in the Anthropic art style, 1:1 square logo for "keel", an agent-native
version-control system.

Full-bleed opaque clay-orange background (#D97757), colored to the corners, no transparency.
On top, one irregular hand-torn ivory-white shape (#FAF9F5) filling ~70% of the frame, slightly
asymmetric.

On the ivory, in thick hand-drawn near-black ink (#141413): the keel and ribs of a wooden boat
under construction, seen from the side — one long gently-curved backbone beam across the lower
third, with a row of curved ribs rising from it like a ship's skeleton. Bold, imperfect, human
ink strokes with organic wobble; a couple of ribs slightly uneven. It should read as a spine /
backbone — the structure everything is built on.

Exactly three colors: near-black ink, ivory, and clay orange. Flat, hand-drawn, woodcut / brush-
pen feel, generous negative space, one focal subject occupying ~70% of the frame, legible at
thumbnail size, no text.

Avoid: gradients, shadows, 3D, photorealism, corporate vector gloss, perfect geometry, clip-art,
thin uniform lines, white or transparent background.
```

---

**Recommendation:** start with **Concept A (keel-beam)** in **clay `#D97757`** — it's the most
literal to "keel," the most distinctive, and the ribs give it the hand-drawn character the style
wants. Keep **B (anchor)** as the safe fallback. Once you have one you like, generate the same
subject in the other accents for colorways.
