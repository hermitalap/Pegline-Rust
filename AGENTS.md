# AGENTS.md — working notes for the Rust rewrite

Read this before touching the repository. It is the contract for agents; the
product behaviour lives in `docs/BEHAVIOR.md` and must not be invented here.

## What this is

Pegline: screenshots hang on a clothesline along the top of the screen. A
Windows tray app, always-on-top, click-through except over photos, never taking
focus. Being rewritten from C#/WPF to **Rust + Win32 + DirectComposition**.

The WPF implementation is frozen under `archive/wpf-v1/` and still builds. It is
the reference for behaviour and for the numbers to beat.

## Goals: small, light, fast

These are targets, not hard constraints. Ship the MVP first, then optimise in
rounds (see `PLAN.md` §1–§2).

- **Small** — one file, no installer, 3–15 MB.
- **Light** — tens of MB of private memory at rest (MVP ≤ 60 MB, target ≤ 30 MB).
- **Fast** — a stable 60 fps, with 120 fps on 120 Hz displays. Never hardcode
  16.6 ms: the frame loop follows the display, springs integrate by real `dt`,
  and the render budget is 8.3 ms.

Do not gold-plate during MVP. Do not ship an optimisation without a before/after
number.

## Status

**Phase A (M0–M4) is complete and acceptance-checked as of 2026-11-02**: shell, tray,
settings, hotkeys, the DirectComposition line with springs/cards/hover/copy/context
menu, the folder and clipboard capture pipeline (twins merged, real Win+Shift+S
verified), and the flight/drop windows all run. `docs/PARITY.md` records what passed.

**Memory was accepted by the user (2026-11-02)**, and three zero-cost reductions
landed: six cards now sit at **50.7 MB** private before the line is first shown, and
~128 MB after the flight window has been created once (the flight window is now built
lazily). Probe sees 80.9 MB with the line visible. `PLAN.md` §3.2 has the breakdown.
Do not trade features or visual quality for memory; the remaining options (hidden
swap chain, thumbnail atlas, destroying the idle flight window) are optional. Phase B
(M5) is unblocked. Read `PLAN.md` §8 before picking the next task.

**Those numbers were re-measured on 2026-11-06 and have drifted.** Probe
`--mode=line` now reports **92.2–92.5 MB** private (p50 16.67 ms, draw p50 0.80 ms),
`--mode=static` 58.8 MB and `--mode=flight` 76.7 MB. A `git worktree` checkout of
`v2.3.0`'s HEAD measures **92.0–92.3 MB** on the same machine, i.e. the drift
happened between 2026-11-02 and v2.3.0 — during the Phase B work — and not in the
2026-11-06 modularisation, which measured no change in memory, frame rate or draw
time against HEAD. The 50.7 MB and ~128 MB app-level figures above are from
2026-11-02 and were not re-measured; the probe cannot reach that state.

**Phase B (M5) landed on 2026-11-02**: hover preview, sticky notes, the annotation
editor (opened from a picture's right-click "Annotate" command), the drag ghost,
the three sounds, the shortcut recorder and the full tray menu are implemented,
with the new `en/es/zh` strings. Phase C is
measured in `PLAN.md` §3.3: 1.29 MB exe, ~105 ms cold start, 60.5 fps on the line,
200 consecutive captures settling at +3.7%, and pooled helper windows released after
10–30 s idle. The remaining acceptance gaps are a 120 Hz display, a second monitor
and sleep/wake; do not invent numbers for them.

**2.1.1 was released on 2026-11-03** (2.0.0 was the first complete Rust release).
Every launch now drops the line for 2.5 s (5 s on the first run) so a start is
visible; the landing hand-off mirrors the real card's row spring and 2.2° sway
while the flight window fades over an opaque copy, so the two images never
separate. The IME candidate window is also fixed and verified with Microsoft
Pinyin at 200% DPI (`docs/VISUAL-TESTS.md` §3.8); the composition itself is now
drawn inline in the text box — see the 2026-11-06 paragraph below. The 2.0.0
release build, checksum and known limitations are in `docs/RELEASE-2.0.0.md`;
rebuild the portable package with `pwsh tools\package-release.ps1`. The only
open items are hardware specific (120 Hz, a second monitor, sleep/wake) and the
subjective listening check for the three audio cues; see `PLAN.md` §8. Do not
trade features or visual quality for memory to chase the original 60 MB MVP
target — the user accepted the measured trade-offs in `PLAN.md` §3.2–§3.5.

**2.2.0 was released on 2026-11-06**: it folds in the annotation editor 2.0,
the WYSIWYG note editor, the card button/click changes and the editor-flight
round below, plus the removal of the M0 spike binary. The tag is `v2.2.0`; the
portable package is built with `pwsh tools\package-release.ps1`.

**The WYSIWYG note editor is implemented on 2026-11-04** (`docs/NOTE-EDITOR.md`,
`docs/DECISIONS.md` D9): `src/note_editor/{model,display,render,window}.rs`, the
entry hotspot and hover note in `src/app/` + `src/window/preview.rs`, and atomic
write/archive in `src/notes.rs`. It reuses the existing flight window for hover →
centre and editor → card, paints the paper to an opaque D2D bitmap for ClearType,
and keeps notes as `.txt` + BOM + CRLF. Verified by desktop screenshots for the
hover/centre flight, the empty editor, the full Markdown projection (headings,
lists, tasks, quote, inline code, link, code block, Chinese), scrolling, saving a
new note and its card flight. The entry hover is a fixed home slot with a
0.56 s/0.6 damped spring (it overshoots about 7 % and settles, no swing), a
pointer-distance pull capped at 30 DIP and a 15 ms physics timer (0.22 s quick
drop from the peeking corner, 0.42 s fly-out); the blank note wears the same
glass card as a hanging picture
(144×96 DIP frame, the 136×88 paper inset by `PHOTO_INSET`) with an ink add-note
icon above an ink `new_note` label sized and coloured like a short note's body
text; a second spring of the same response and
damping leans that card by up to 5° about its centre — clockwise in the
top-left/bottom-right quadrants, counter-clockwise in the other two, zero on the
crosshair through the centre — so the twist lags and settles with the travel
(`PEGLINE_NOTE_DEMO_POINTER` parks the demo pointer in any quadrant). A window
cannot rotate, so the UI thread keeps moving it with `SetWindowPos` while the
render thread repaints the card only when the lean changes, coalescing the
requests; the flight that carries a brand-new note to the editor passes
`pin = false`, because the note is not hanging yet; the exit test is the fixed
hotspot plus 24 DIP, never the moving note itself, so moving the mouse away
always flies it out. Discarding an empty new note does not close the editor
window in place (that read as a crash): `Flight::tumble` shrinks the paper to
45 %, spins it in a random direction and drops it off the screen under gravity
(2600 DIP/s², integrated by real frame `dt`), and the editor is hidden only
after the flight's first frame (`FlightEvent::Started`); `--note-discard-demo`
reproduces it.
Memory was measured: +46.0 MB while the editor is
open (release, 200 %), and the editor's device/resources are released on close;
see `PLAN.md` §9. Chinese IME was verified with Microsoft Pinyin at 200 % DPI:
the composition run is underlined on the active line, the candidate window sits
at the caret, and Space commits 你好. The existing-note edit/save/archive path was
verified with `--note-storage-demo`: a temporary note is edited, written as
BOM+CRLF, moved to `Notes\Archive`, and cleaned up. Still pending: 200 % DPI
re-centre, three-language menu/balloon checks and the keyboard-to-pixel latency;
do not mark those as passed without a measurement. The old note "Edit" path now
opens this editor instead of Notepad; "Archive" replaces "Discard" for notes.
On 2026-11-05 the entry gained a top-corner peek whenever the pointer is anywhere
over the strip: a card tilted 18° and mostly above the screen, with only its
bottom-right corner (12 DIP) showing. The 120 ms dwell was removed so entering
the hotspot (including the corner column at the top edge) drops the full note in
0.22 s from that off-screen pose and rotates it upright. The line stays pinned
while the editor is open. An edited note now flies back through
a `SetRehang` scene command that keeps the row from reflowing; an editor pass with
no changed text plays the same flight instead of closing in place. See `PLAN.md`
§9.2 and the `docs/visual/note-entry-*` / `note-rehang-*` screenshots.

**The annotation editor was rebuilt as a floating hanging picture on 2026-11-05**
(`docs/BEHAVIOR.md` §11): a frameless, transparent, always-on-top window with no
title bar or caption buttons, an enlarged white-glass card (4 DIP frame, 16 DIP
radius, clothespin, baked nine-patch shadow) and five floating glass pills 18
DIP outside the card, their controls concentric with the pill (circular tools,
colour dots and widths; stadium action buttons), and five tool glyphs imported
from Lucide (ISC/MIT, `assets/icons/`): arrow, rectangle, text, eraser and
`eye-off` for the mosaic. The eraser removes whole marks under a 14 DIP disc
(one undo step per drag, Esc cancels); line widths are 4/8/12 dots. The photo
and marks render into an opaque card bitmap
(ClearType) composited through a rounded mask; the window itself uses a
premultiplied swap chain. Opening from a card first turns its current tilt
upright with a short damped spring, then grows a source decode (up to 2048 px)
out of its slot and shows the editor on landing; clicking outside the card,
losing focus, Ctrl+S or Save writes the PNG and flies a marked 2048 px preview
back to the card, while Esc cancels with a sharp copy of the untouched picture.
The source card is hidden for the whole edit. Restored cards now read their real
pixel size (`wic::pixel_size`)
instead of the thumbnail size, and the note editor hides the edited note's card
under the opening flight and restores it with the return. `--annotate-demo`
drives the card-to-editor round trip without a mouse; the visual evidence is in
`docs/VISUAL-TESTS.md` §3.12 and `docs/visual/editor-2.0.png` / `editor-flight.png`.

**On 2026-11-06 the annotation text tool draws the IME composition itself**
(BEHAVIOR §11): it handles `WM_IME_STARTCOMPOSITION` / `WM_IME_COMPOSITION` /
`WM_IME_ENDCOMPOSITION`, reads `GCS_RESULTSTR` (the single commit path;
`WM_IME_CHAR` is a fallback only for a window with no IME context) and
`GCS_COMPSTR` / `GCS_CURSORPOS`, clears `ISC_SHOWUICOMPOSITIONWINDOW` in
`WM_IME_SETCONTEXT`, and lays out `text + composition` with the pinyin underlined
inside the white box. The box width and the caret follow that composed layout;
`COMPOSITIONFORM` points at the insertion point and `CANDIDATEFORM` at the bottom
of the box, and a `WM_APP_IME_CARET` post from the render thread re-anchors the
candidate list once the new composition has been measured (the IME asks before
the frame is painted). Esc, a click, Save, Ctrl+S or focus loss cancel an open
composition instead of baking pinyin into the picture; a composition never enters
`marks`, the undo history or the saved PNG. Verified with Microsoft Pinyin at
200% DPI on `--editor-demo --editor-hold`; screenshots
`docs/visual/editor-ime-inline.png` / `editor-ime-commit.png`, details in
`docs/VISUAL-TESTS.md` §3.16. IMEs with their own skin (Sogou and friends) still
draw their own composition window; the app can only give them the right insertion
point.

**On 2026-11-06 the card interactions and editor flights changed** (BEHAVIOR
§5.1, §8, §9.5, §10.1, §11; DECISIONS D10): a picture's left hover pill is now
**Annotate** (right stays Open), a note keeps Copy / Open, and a single click on
a note opens its editor while a picture still copies. Editor flights no longer
carry an enlarged thumbnail: a note flight starts moving at once and cross-fades
the card paper into a cached WIC render of the editor paper (same
`paint_paper_on` code, 920×720 at 200 %, 3.7–8.7 ms on the render thread) during
the first 0.12 s; an annotation flight decodes the source at up to 2048 px on the
flight thread. The annotation flight turns a swinging card upright with a 0.16 s
damped angular spring before moving; note flights do that during the travel so
the click has no fade-then-fly pause. The pin belongs to the rope: a returning
note shows it only for the last ~0.18 s, and an existing note leaving for the
editor fades it out during the first 0.12 s. Return flights keep the glass
frame from the first frame. The annotation editor is
kept composed after it reports a result and is hidden only when the return
flight's first frame is up, which removes the exit flicker; its render thread
waits for `Stop` before dropping the surface.

## Read first

1. `PLAN.md` — milestones, budget, decisions, open questions.
2. `docs/ARCHITECTURE.md` — what is being built and why.
3. `docs/BEHAVIOR.md` — the spec; every user-visible detail is there.
4. `docs/RUST-STACK.md` — crates, build flags, packaging.
5. `docs/SPIKE-01.md` — the first task.

## Build and test (once the crate exists)

```powershell
cargo build --release          # target\release\pegline.exe
cargo test                     # pure logic only
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo run --bin probe -- --mode=flight
```

There is no MSBuild, no .NET, no WPF in the new code. The old build scripts under
`archive/wpf-v1/` are for the archived app only.

## Hard rules

- **Do not port WPF idioms.** No retained visual tree, no layout pass, no
  `UpdateLayeredWindow`. The overlay is a GPU swap chain composited by DWM.
- **Never rasterise a whole window on the CPU.** If a frame needs a CPU buffer
  the size of the window, the design is wrong.
- **No per-frame heap allocation** in the render or animation loop. Cache
  brushes, geometries, text layouts; reuse scratch buffers.
- **No CPU readback of GPU surfaces.** Hit-testing is computed from the same
  layout math as drawing, never from pixels.
- **Shadows are baked once**, not blurred per frame.
- **The UI thread never blocks** on decode, encode, compare, file IO, shell
  operations, or `Present`. Use the render thread and the worker pool.
- **`unsafe` is allowed only** around Win32/COM/D3D calls, with a comment stating
  the precondition. COM objects come from the `windows` crate; do not hand-write
  `Release`.
- **No `.unwrap()` / `.expect()`** on IO, COM or user input. Log and degrade.
- Every user-visible string goes through `i18n` with en/es/zh (copy the existing
  translations from `archive/wpf-v1/src`; do not retranslate).
- Comments and identifiers in English; UTF-8 without BOM; CRLF for text files.

## Performance rules (measured on the old app)

- A transparent WPF window cost ~200 MB and 8.7 fps for a full-screen flight.
  The Rust overlay must stay under 80 MB and above 59.5 fps in the same scenario.
- 6K PNG decode is ~40 ms; never do it on the UI thread.
- The window surface is the expensive thing: keep the line strip at 6016×420 and
  the flight window at the path bounding box, and shrink/hide them when idle.
- The card thumbnails are decoded at the card's physical size (136 DIP × screen
  scale, 480 px cap) so the blit is 1:1; the capture flight image is 1600 px,
  editor flights up to 2048 px; the twin compare is 2048 px.

## Workflow

1. One change at a time; keep the diff readable.
2. `cargo build` + `cargo test` + `clippy`.
3. Run the app; check the animation by hand — the animation is the product.
4. Run `probe` for frame rate and private memory; record the numbers in
   `PLAN.md` before starting the next item.
5. Update `docs/PARITY.md` when a checklist item starts passing.

## Git

- `target/` and any generated icons are ignored; never commit build output.
- Commit subjects: short imperative, `feat:` / `fix:` / `perf:` / `docs:`.
- `archive/wpf-v1/` is frozen: do not fix or refactor it. If the reference is
  wrong, change `docs/BEHAVIOR.md` and say why.
