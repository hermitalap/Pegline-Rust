# Pegline 2.2.0 release notes

Release date: **2026-11-06**  
Platform: Windows 10 / 11, x86_64  
Artifact: `dist/Pegline-2.2.0-win-x64.zip` (single executable + README + LICENSE + CHANGELOG)  
Tag: `v2.2.0`

- `pegline.exe`: 1,758,720 B (1.68 MB), static CRT
- `Pegline-2.2.0-win-x64.zip`: 759,480 B
- sha256: `de22d67be17477f231105f99234a85851a61fcd5b2201e1b7ea4fb6ed2f6d1e2`

## What changed since 2.1.1

2.2.0 folds in the work that was on `main` after the 2.1.1 package:

- **Annotation editor 2.0**: a frameless floating hanging picture with a glass
  card, clothespin, baked shadow and five floating glass pills; Lucide tool
  icons; a fifth eraser tool that removes whole marks under a 14 DIP disc;
  colours and line widths moved to circular controls. Saving writes the PNG at
  the source resolution back over the card's file; cancelling leaves it alone.
- **WYSIWYG sticky notes**: a borderless 460x360 DIP paper editor with Markdown
  projection on the lines away from the caret, inline IME composition, atomic
  BOM+CRLF files and archive-on-take-down.
- **Card interactions**: a picture's left hover pill is now **Annotate** (right
  stays Open); a note keeps Copy / Open, and a single click on a note opens its
  editor while a picture still copies.
- **Editor flights**: the annotation opening turns the hanging card upright with
  a short damped angular spring before it grows into the editor. Note flights
  start moving immediately and cross-fade the card paper into a cached WIC
  render of the editor paper during the first 0.12 s, so there is no
  fade-then-fly pause. The annotation return carries a 2048 px marked preview
  and no longer blinks through an empty editor window.
- **Clothespin timing**: the pin belongs to the rope. A brand-new note has none;
  an existing note fades its pin out over the first 0.12 s of leaving the line;
  a returning note fades it in only over the last ~0.18 s as it reaches the slot.
- **Build cleanup**: the obsolete M0 `spike` binary is no longer part of the
  crate; its evidence remains in the git history and `docs/SPIKE-01-RESULTS.md`.

## Verification

- `cargo test`: **179 passed**, 1 ignored (the opt-in registry test).
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`: clean.
- `cargo build --release`: succeeds; the embedded VERSIONINFO reports
  `2.2.0.0` (checked by `tools/package-release.ps1`).
- Release binary: 1,758,720 B; portable zip: 759,480 B,
  sha256 `de22d67be17477f231105f99234a85851a61fcd5b2201e1b7ea4fb6ed2f6d1e2`.
- Visual evidence for the flight changes is in `docs/VISUAL-TESTS.md` §3.13 and
  `docs/visual/note-entry-editor-image.png`, `note-return-editor-image.png`,
  `editor-entry-flight.png`, `editor-return-flight.png`.
- New measurements in this round were limited to debug-build image generation
  (note editor 920x720: 3.7-8.7 ms) and release-build editor setup (about 52 ms
  including the 7.6 ms image). No fresh 60/120 Hz frame-rate or private-memory
  run was made for this package; the 2.1.1 numbers in `README.md` remain the
  reference.

## Known limitations

- 120 Hz, a second monitor and sleep/wake are still untested on this machine.
- The three audio cues still need a subjective listening check.
- The annotation editor does not participate in the snapshot shortcut flow;
  it is opened from a picture card's Annotate command.

## Upgrade and data

- Copy `pegline.exe` anywhere and run it; there is no installer and no runtime.
- Data stays in `%APPDATA%\Pegline` and `%LOCALAPPDATA%\Pegline`, shared with
  the WPF 1.x build; notes remain `.txt` with UTF-8 BOM + CRLF.
- Settings, note colours, autostart and the pegged file are kept as-is.