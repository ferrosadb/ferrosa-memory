# Ferrosa branding assets

This directory holds Ferrosa-family Discord/community identity assets for Ferrosa Memory and Ferrosa Loom.

Both marks preserve the parent Ferrosa grammar — periodic-table frame, orbital system, serif chemistry monogram — while changing color, symbol, and secondary motif so each sub-product stays visually distinct.

## Ferrosa Memory Discord logo

Ferrosa Memory uses an electric blued-steel treatment: `Fm / 100`, memory graph paths, and cool blue/cyan tones.

| Asset | Use |
|---|---|
| `ferrosa-memory-discord-logo.svg` | Source vector asset. Use this for future edits and exports. |
| `ferrosa-memory-discord-logo-1024.png` | Primary Discord server/avatar upload asset. |
| `ferrosa-memory-discord-logo-512.png` | Smaller raster export for docs, previews, and integrations. |
| `ferrosa-memory-discord-logo-preview.png` | Review sheet showing square and circular Discord crops. |
| `render-ferrosa-memory-logo.mjs` | Rebuild script for PNG exports from the SVG source. |

### Memory visual notes

- **Family resemblance:** periodic-table frame, orbital system, serif chemistry monogram.
- **Distinct Memory treatment:** electric blued steel instead of terracotta/copper, memory graph paths/nodes, `Fm` rather than `Fe`.
- **Primary palette:**
  - Electric cyan: `#7ee7ff`
  - Core blue: `#348cff`
  - Blued steel: `#516d92`
  - Deep field: `#05070c` / `#09111f`
- **Discord guidance:** use `ferrosa-memory-discord-logo-1024.png`; the composition is inset for Discord's circular crop.

## Ferrosa Loom Discord icon

Ferrosa Loom is the creator/editor/agent-runner UI. It uses a woven amethyst-thread treatment: `Fl / 114` as a Flerovium-style periodic-table nod, crossed agent lanes, and loom/shuttle cues.

| Asset | Use |
|---|---|
| `ferrosa-loom-discord-logo.svg` | Source vector asset. Use this for future edits and exports. |
| `ferrosa-loom-discord-logo-1024.png` | Primary Discord server/avatar upload asset. |
| `ferrosa-loom-discord-logo-512.png` | Smaller raster export for docs, previews, and integrations. |
| `ferrosa-loom-discord-logo-preview.png` | Review sheet showing square and circular Discord crops. |
| `render-ferrosa-loom-logo.mjs` | Rebuild script for PNG exports from the SVG source. |

### Loom visual notes

- **Family resemblance:** periodic-table frame, orbital system, serif chemistry monogram.
- **Distinct Loom treatment:** amethyst + molten thread palette, woven runner paths, `Fl / 114` rather than `Fe / 26` or `Fm / 100`.
- **Meaning:** Loom weaves models, memory, agents, prompts, files, and generated artifacts into a creative working surface.
- **Primary palette:**
  - Amethyst: `#c882f0`
  - Molten thread: `#f3b35f`
  - Warm highlight: `#fff0b8`
  - Runner blue: `#6d7cff`
  - Deep field: `#07060c` / `#0d0818`
- **Discord guidance:** use `ferrosa-loom-discord-logo-1024.png`; the composition is inset for Discord's circular crop.

## Rebuild

From the `ferrosa-memory` repo root:

```bash
node docs/brand/render-ferrosa-memory-logo.mjs
node docs/brand/render-ferrosa-loom-logo.mjs
```

These regenerate the 1024px, 512px, and preview PNGs from their SVG sources.
