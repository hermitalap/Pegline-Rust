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

Design phase finished; **no Rust code yet**. Do not start features before M0
(`docs/SPIKE-01.md`) passes: the whole architecture depends on GPU-composited
per-pixel transparency working.

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
- The card thumbnails are 480 px; the flight image is 1600 px; the twin compare
  is 2048 px. Do not decode bigger than needed.

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
