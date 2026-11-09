# LiquidGlass experiment archive

Archived on 2026-11-09. This directory records the LiquidGlass visual
experiment. It is not part of the product build and was never merged into
`main` or released.

## What was tried

- A probe-only CPU reference renderer (`probe --mode=glass`) that draws the
  LiquidGlass ring over a photograph and writes four variants at 100 % and
  200 %.
- A standalone GPU demo (`glass_demo`) with a D3D11/D2D pipeline: a full
  superellipse lens, thick capsules, a live desktop backdrop and twelve
  sliders for the visual parameters.
- A branch implementation of the tuned look in the main app's D2D surfaces:
  the 4 DIP card ring, the card action capsules and the annotation editor
  capsules.

The tuned values used by the branch were:

| parameter | value |
|---|---|
| blur | 8.00 |
| tint | 0.00 |
| edge tint | 0.10 |
| noise | 0.00 |
| dispersion | 1.00 |
| magnify | 1.00 |
| power | 2.83 |
| f power | 1.50 |
| glow | 0.20 |
| light angle | 225° |
| shininess | 2 |
| edge width | 9.00 |

## Where the code is

The branch was `feat/liquid-glass-visual`, with two commits:

- `f8341ae` — the probe and the GPU demo
- `43ea50f` — the tuned treatment in the main app

The branch is preserved by the annotated tag `archive/liquid-glass-visual`.
The working tree was returned to `main` at `3b5d524`; only this archive
directory was added afterwards.

A separate patch, `uncommitted.patch`, captures the unfinished work that was
in the working tree when the experiment was stopped: note-peek, flight and
annotation-editor rings. **It does not compile** (the editor ring was being
moved from `Assets` to `EditorState` when the experiment stopped). Keep it
for reference; do not apply it to `main`.

## Restore the experiment

```powershell
git worktree add ..\Pegline-liquid-glass archive/liquid-glass-visual
cd ..\Pegline-liquid-glass
cargo build --release --bin glass_demo
.\target\release\glass_demo.exe --scale=2
```

The GPU demo opens a 1400x900 DIP window (2800x1800 at 2x). Move the sliders
with the mouse, press `B` to switch between the live desktop and the owl
photo, `R` to reset the parameters and `Esc` to quit. On this machine it
measured 59–60 fps with the photo backdrop at 2800x1800, 58 fps at 1400x900,
and about 42 fps with the live GDI capture.

The probe still works from the tag:

```powershell
cargo run --release --bin probe -- --mode=glass
```

It rewrites the eight `docs/visual/liquid-glass-*.png` images in the
worktree.

## Files

- `docs/LIQUID-GLASS.md` — the experiment write-up from the branch.
- `docs/visual/liquid-glass-*.png` — the eight probe variants.
- `docs/visual/glass-gpu*.png` — the GPU demo screenshots.
- `docs/visual/mainapp-*.png` — the branch's line and editor screenshots.
- `uncommitted.patch` — the unfinished note-peek / flight / editor work.

## Outcome

The GPU lens and the large capsules read well at 200 %. The 4 DIP card ring is
the hard case: at 4–8 physical pixels there is no room for blur, the
refraction collapses into a coloured fringe and the highlight reads as a
sticker. The final recommendation was to keep the card at a fixed top-left
highlight with a faint inner line, and reserve the real lens for larger
surfaces such as the capsules and the editor controls. This is why the work
stays on the branch and is archived here rather than merged.
